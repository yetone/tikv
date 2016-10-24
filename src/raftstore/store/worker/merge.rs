// Copyright 2016 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

use std::thread;
use std::time::Duration;
use std::fmt::{self, Formatter, Display};
use std::net::{TcpStream, SocketAddr};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use uuid::Uuid;

use kvproto::metapb::{Region, Peer};
use kvproto::raft_cmdpb::{RaftCmdRequest, RaftCmdResponse, AdminRequest, AdminCmdType};
use kvproto::msgpb::{Message, MessageType};

use server::ResolveTask;
use util::worker::{Scheduler, Runnable};
use util::codec::rpc;
use util::make_std_tcp_conn;
use util::transport::SendCh;
use raftstore::store::Msg;
use raftstore::store::util::ensure_schedule;
use raftstore::Result;

const MAX_RAFT_RPC_SEND_RETRY_COUNT: u64 = 2;
const RAFT_RPC_RETRY_TIME_MILLIS: u64 = 50;
const SOCKET_READ_TIMEOUT: u64 = 3;
const SOCKET_WRITE_TIMEOUT: u64 = 3;

/// Client to communicate with TiKV region for region merge.
/// It sends Raft command requests to the specified TiKV region and
/// waits for the corresponding responses.
pub trait RaftClient {
    /// `suspend_region` suspends the region which is about to be merged.
    fn send_suspend_region(&self, region: Region, leader: Peer) -> Result<()>;
    /// `shutdown_region` shutdowns a region which is merged before.
    fn send_shutdown_region(&self, region: Region, leader: Peer) -> Result<()>;
}

#[derive(Debug, Clone)]
enum TaskType {
    SuspendRegion,
    ShutdownRegion,
}

#[derive(Clone)]
pub struct TaskContext {
    task_type: TaskType,
    region: Region,
    peer: Peer,
    local_region: Region,
    local_peer: Peer,
    address: SocketAddr,
}

impl Display for TaskContext {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f,
               "task context, type {:?}, region {:?}, peer {:?}, local_region: {:?}, local_peer: \
                {:?}, address: {}",
               self.task_type,
               self.region,
               self.peer,
               self.region,
               self.local_peer,
               self.address)
    }
}

#[derive(Clone)]
pub enum Task {
    SuspendRegion {
        // the region to be suspended
        region: Region,
        // a hint to start raft rpc with
        leader: Peer,
        // local region which controls the region merge procedure
        local_region: Region,
        // local peer which controls the region merge procedure
        local_peer: Peer,
    },
    CommitMerge {
        // local region which controls the region merge procedure
        region: Region,
        // local region which controls the region merge procedure
        peer: Peer,
    },
    ShutdownRegion {
        // the region to be shutdown
        region: Region,
        // a hint to start raft rpc with
        leader: Peer,
    },
    AfterResolve {
        // a struct that passes the task context between callbacks
        context: TaskContext,
    },
}


impl Display for Task {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match *self {
            Task::SuspendRegion { ref region, ref leader, ref local_region, ref local_peer } => {
                write!(f,
                       "suspend region {:?}, leader {:?}, local region {:?}, local peer {:?}",
                       region,
                       leader,
                       local_region,
                       local_peer)
            }
            Task::CommitMerge { ref region, ref peer } => {
                write!(f,
                       "commit region merge for region {:?}, peer {:?}",
                       region,
                       peer)
            }
            Task::ShutdownRegion { ref region, ref leader } => {
                write!(f, "shutdown region {:?}, leader {:?}", region, leader)
            }
            Task::AfterResolve { ref context } => write!(f, "after resolve, context {}", context),
        }
    }
}

#[derive(Debug)]
struct RaftRpcClientCore {
    address: SocketAddr,
    stream: Option<TcpStream>,
}

fn send_request(stream: &mut TcpStream,
                msg_id: u64,
                request: &RaftCmdRequest)
                -> Result<(u64, RaftCmdResponse)> {
    let mut message = Message::new();
    message.set_msg_type(MessageType::Cmd);
    message.set_cmd_req(request.clone());

    try!(stream.set_write_timeout(Some(Duration::from_secs(SOCKET_WRITE_TIMEOUT))));
    try!(rpc::encode_msg(stream, msg_id, &message));

    try!(stream.set_read_timeout(Some(Duration::from_secs(SOCKET_READ_TIMEOUT))));
    let mut resp = Message::new();
    let id = try!(rpc::decode_msg(stream, &mut resp));
    if resp.get_msg_type() != MessageType::CmdResp {
        return Err(box_err!("invalid cmd response type {:?}", resp.get_msg_type()));
    }

    Ok((id, resp.take_cmd_resp()))
}

fn rpc_connect(address: SocketAddr) -> Result<TcpStream> {
    let stream = try!(make_std_tcp_conn(address));
    try!(stream.set_write_timeout(Some(Duration::from_secs(SOCKET_WRITE_TIMEOUT))));
    Ok(stream)
}

impl RaftRpcClientCore {
    pub fn new(address: SocketAddr) -> RaftRpcClientCore {
        RaftRpcClientCore {
            address: address,
            stream: None,
        }
    }

    fn try_connect(&mut self) -> Result<()> {
        let stream = try!(rpc_connect(self.address));
        self.stream = Some(stream);
        Ok(())
    }

    fn send(&mut self, msg_id: u64, req: &RaftCmdRequest) -> Result<RaftCmdResponse> {
        for _ in 0..MAX_RAFT_RPC_SEND_RETRY_COUNT {
            if self.stream.is_none() {
                if let Err(e) = self.try_connect() {
                    warn!("connect tikv failed {:?}", e);
                    thread::sleep(Duration::from_millis(RAFT_RPC_RETRY_TIME_MILLIS));
                    continue;
                }
            }

            let mut stream = self.stream.take().unwrap();

            let (id, resp) = match send_request(&mut stream, msg_id, req) {
                Err(e) => {
                    warn!("send message to tikv failed {:?}", e);
                    thread::sleep(Duration::from_millis(RAFT_RPC_RETRY_TIME_MILLIS));
                    continue;
                }
                Ok((id, resp)) => (id, resp),
            };

            if id != msg_id {
                return Err(box_err!("tikv response msg_id not match, want {}, got {}",
                                    msg_id,
                                    id));
            }

            self.stream = Some(stream);

            return Ok(resp);
        }
        Err(box_err!("send message to tikv failed, address: {}", self.address))
    }
}

pub struct RaftRpcClient {
    msg_id: AtomicUsize,
    core: Mutex<RaftRpcClientCore>,
}

impl RaftRpcClient {
    pub fn new(address: SocketAddr) -> RaftRpcClient {
        RaftRpcClient {
            msg_id: AtomicUsize::new(0),
            core: Mutex::new(RaftRpcClientCore::new(address)),
        }
    }

    pub fn send(&self, req: &RaftCmdRequest) -> Result<RaftCmdResponse> {
        let msg_id = self.alloc_msg_id();
        let resp = try!(self.core.lock().unwrap().send(msg_id, req));
        Ok(resp)
    }

    pub fn alloc_msg_id(&self) -> u64 {
        self.msg_id.fetch_add(1, Ordering::Relaxed) as u64
    }
}

impl RaftClient for RaftRpcClient {
    fn send_suspend_region(&self, region: Region, peer: Peer) -> Result<()> {
        let mut req = RaftCmdRequest::new();
        req.mut_header().set_region_id(region.get_id());
        req.mut_header().set_peer(peer);
        req.mut_header().set_uuid(Uuid::new_v4().as_bytes().to_vec());

        let mut admin_req = AdminRequest::new();
        admin_req.set_cmd_type(AdminCmdType::SuspendRegion);
        admin_req.mut_suspend_region().set_region(region);
        req.set_admin_request(admin_req);

        let _ = try!(self.send(&req));
        Ok(())
    }

    fn send_shutdown_region(&self, region: Region, peer: Peer) -> Result<()> {
        let mut req = RaftCmdRequest::new();
        req.mut_header().set_region_id(region.get_id());
        req.mut_header().set_peer(peer);
        req.mut_header().set_uuid(Uuid::new_v4().as_bytes().to_vec());

        let mut admin_req = AdminRequest::new();
        admin_req.set_cmd_type(AdminCmdType::ShutdownRegion);
        admin_req.mut_shutdown_region().set_region(region);
        req.set_admin_request(admin_req);

        let _ = try!(self.send(&req));
        Ok(())
    }
}

pub struct Runner {
    scheduler: Scheduler<Task>,
    resolve_scheduler: Scheduler<ResolveTask>,
    ch: SendCh<Msg>,
}

fn next_peer(region: &Region, last_peer: Peer) -> Peer {
    let last_peer_id = last_peer.get_id();
    let peers = region.get_peers();
    if let Some(index) = peers.iter().position(|p| p.get_id() == last_peer_id) {
        let next_index = (index + 1) % peers.len();
        return peers[next_index].clone();
    }
    if peers.len() == 0 {
        return last_peer;
    }
    peers[0].clone()
}

impl Runner {
    pub fn new(scheduler: Scheduler<Task>,
               resolve_scheduler: Scheduler<ResolveTask>,
               ch: SendCh<Msg>)
               -> Runner {
        Runner {
            scheduler: scheduler,
            resolve_scheduler: resolve_scheduler,
            ch: ch,
        }
    }

    fn handle_suspend_region(&self,
                             region: Region,
                             leader: Peer,
                             local_region: Region,
                             local_peer: Peer) {
        // TODO add impl
        // send a raft command "suspend region" to the specified region/leader
        // if network errors happen, try another peer
        // if get response "not leader", try another peer
        // if get response "leader is another peer", try the given peer
        // if get response "succeed", send self a commit merge task

        let store_id = leader.get_store_id();
        let scheduler = self.scheduler.clone();
        let last_peer = leader.clone();
        let cb = box move |r| {
            match r {
                Ok(addr) => {
                    let task = Task::AfterResolve {
                        context: TaskContext {
                            task_type: TaskType::SuspendRegion,
                            region: region,
                            peer: last_peer,
                            local_region: local_region,
                            local_peer: local_peer,
                            address: addr,
                        },
                    };
                    ensure_schedule(scheduler, task)
                }
                Err(e) => {
                    error!("failed to resolve store, err: {:?}", e);
                    // retry another peer
                    let next_peer = next_peer(&region, last_peer);
                    let task = Task::SuspendRegion {
                        region: region,
                        leader: next_peer,
                        local_region: local_region,
                        local_peer: local_peer,
                    };
                    ensure_schedule(scheduler, task);
                }
            }
        };
        // TODO rewrite it using `ensure_schedule`
        if let Err(e) = self.resolve_scheduler.schedule(ResolveTask::new(store_id, cb)) {
            error!("try to resolve err {:?}", e);
        }
    }

    fn handle_commit_merge(&self, _region: Region, _peer: Peer) {
        // TODO add impl
        // send a raft cmd "commit merge" to the specified peer
        // if it times out on waiting for response, retry
        // make sure get one response "ok"
    }

    fn handle_shutdown_region(&self, _region: Region, _leader: Peer) {
        // TODO add impl
        // send a raft command "shutdown region" to the specified region/leader
        // if get response "not leader", try another peer
        // if get response "leader is another peer", try the given peer
        // if network errors happen, abort this task.
        // PD will tell the specified region to shutdown in heartbeat communication
    }

    fn handle_after_resolve(&self, context: TaskContext) {
        match context.task_type {
            TaskType::SuspendRegion => {
                let client = RaftRpcClient::new(context.address);
                match client.send_suspend_region(context.region.clone(), context.peer.clone()) {
                    Ok(()) => {
                        // TODO check that the region info in response matches
                        // Succeed to suspend the specified region, and then go to next step
                        let task = Task::CommitMerge {
                            region: context.local_region,
                            peer: context.local_peer,
                        };
                        ensure_schedule(self.scheduler.clone(), task);
                    }
                    Err(e) => {
                        error!("fail to send raft rpc to peer {:?} error {:?}",
                               context.peer,
                               e);
                        // TODO what are all the possible errors returned here?
                        // Try another peer in the specified region
                        let next_peer = next_peer(&context.region, context.peer);
                        let task = Task::SuspendRegion {
                            region: context.region,
                            leader: next_peer,
                            local_region: context.local_region,
                            local_peer: context.local_peer,
                        };
                        ensure_schedule(self.scheduler.clone(), task);
                    }
                }
            }
            TaskType::ShutdownRegion => {}
        }
    }
}

impl Runnable<Task> for Runner {
    fn run(&mut self, task: Task) {
        debug!("executing task {}", task);

        match task {
            Task::SuspendRegion { region, leader, local_region, local_peer } => {
                self.handle_suspend_region(region, leader, local_region, local_peer)
            }
            Task::CommitMerge { region, peer } => self.handle_commit_merge(region, peer),
            Task::ShutdownRegion { region, leader } => self.handle_shutdown_region(region, leader),
            Task::AfterResolve { context } => self.handle_after_resolve(context),
        };
    }
}