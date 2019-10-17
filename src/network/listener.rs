use actix::prelude::*;
use std::time::{Duration, Instant};
use tokio::codec::FramedRead;
use tokio::io::{AsyncRead, WriteHalf};
use tokio::net::{TcpListener, TcpStream};
use actix_raft::{
    NodeId,
    messages,
};
use std::sync::Arc;
use std::marker::PhantomData;
use std::collections::HashMap;
use serde::{Serialize, de::DeserializeOwned};

use crate::raft::{
    MemRaft,
    storage
};
use crate::network::{
    Network,
    NodeCodec,
    NodeRequest,
    NodeResponse,
    PeerConnected,
    remote::{
        RemoteMessageHandler,
        RegisterHandler,
        RemoteMessage,
        Provider,
    },
};

pub struct Listener {
    network: Addr<Network>,
    raft: Option<Addr<MemRaft>>,
}

impl Listener {
    pub fn new(address: &str, network_addr: Addr<Network>) -> Addr<Listener> {
        let server_addr = address.parse().unwrap();
        let listener = TcpListener::bind(&server_addr).unwrap();

        Listener::create(|ctx| {
            ctx.add_message_stream(listener.incoming().map_err(|_| ()).map(NodeConnect));

            Listener {
                network: network_addr,
                raft: None,
            }
        })
    }
}

impl Actor for Listener {
    type Context = Context<Self>;
}

#[derive(Message)]
struct NodeConnect(TcpStream);

impl Handler<NodeConnect> for Listener {
    type Result = ();

    fn handle(&mut self, msg: NodeConnect, _: &mut Context<Self>) {
        let remote_addr = msg.0.peer_addr().unwrap();
        let (r, w) = msg.0.split();

        let network = self.network.clone();

        NodeSession::create(move |ctx| {
            NodeSession::add_stream(FramedRead::new(r, NodeCodec), ctx);
            NodeSession::new(actix::io::FramedWrite::new(w, NodeCodec, ctx), network)
        });
    }
}

#[derive(Message)]
pub struct RaftCreated(pub Addr<MemRaft>);

impl Handler<RaftCreated> for NodeSession {
    type Result = ();

    fn handle(&mut self, msg: RaftCreated, ctx: &mut Context<Self>) {
        self.raft = Some(msg.0);
    }
}

// NodeSession
pub struct NodeSession {
    hb: Instant,
    network: Addr<Network>,
    framed: actix::io::FramedWrite<WriteHalf<TcpStream>, NodeCodec>,
    id: Option<NodeId>,
    handlers: HashMap<&'static str, Arc<dyn RemoteMessageHandler>>,
    raft: Option<Addr<MemRaft>>,
}

impl NodeSession {
    fn new(
        framed: actix::io::FramedWrite<WriteHalf<TcpStream>, NodeCodec>,
        network: Addr<Network>,
    ) -> NodeSession {
        NodeSession {
            hb: Instant::now(),
            framed: framed,
            network,
            id: None,
            handlers: HashMap::new(),
            raft: None,
        }
    }

    fn hb(&self, ctx: &mut Context<Self>) {
        ctx.run_interval(Duration::new(1, 0), |act, ctx| {
            if Instant::now().duration_since(act.hb) > Duration::new(10, 0) {
                println!("Client heartbeat failed, disconnecting!");
                ctx.stop();
            }

            // Reply heartbeat
            act.framed.write(NodeResponse::Ping);
        });
    }
}

impl Actor for NodeSession {
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Context<Self>) {
        self.hb(ctx);
    }
}

impl actix::io::WriteHandler<std::io::Error> for NodeSession {}


struct SendToRaft(String, String);

impl Message for SendToRaft
{
    type Result = Result<String, ()>;
}

impl Handler<SendToRaft> for NodeSession
{
    type Result = Response<String, ()>;

    fn handle(&mut self, msg: SendToRaft, ctx: &mut Context<Self>) -> Self::Result {
        let type_id = msg.0;
        let body = msg.1;

        let res = match type_id.as_str() {
            "AppendEntriesRequest" => {
                let raft_msg = serde_json::from_slice::<messages::AppendEntriesRequest<storage::MemoryStorageData>>(body.as_ref()).unwrap();
                if let Some(ref mut raft) = self.raft {
                    let future = raft.send(raft_msg)
                        .map_err(|_| ())
                        .and_then(|res| {
                            let res = res.unwrap();
                            let res_payload = serde_json::to_string::<messages::AppendEntriesResponse>(&res).unwrap();
                            futures::future::ok(res_payload)
                        });

                    Response::fut(future)
                }  else {
                    Response::reply(Ok("".to_owned()))
                }
            },
            "VoteRequest" => {
                let raft_msg = serde_json::from_slice::<messages::VoteRequest>(body.as_ref()).unwrap();
                if let Some(ref mut raft) = self.raft {
                    let future = raft.send(raft_msg)
                        .map_err(|_| ())
                        .and_then(|res| {
                            let res = res.unwrap();
                            let res_payload = serde_json::to_string::<messages::VoteResponse>(&res).unwrap();
                            futures::future::ok(res_payload)
                        });
                    Response::fut(future)
                }  else {
                    Response::reply(Ok("".to_owned()))
                }
            },
            "InstallSnapshotRequest" => {
                let raft_msg = serde_json::from_slice::<messages::InstallSnapshotRequest>(body.as_ref()).unwrap();
                if let Some(ref mut raft) = self.raft {
                    let future = raft.send(raft_msg)
                        .map_err(|_| ())
                        .and_then(|res| {
                            let res = res.unwrap();
                            let res_payload = serde_json::to_string::<messages::InstallSnapshotResponse>(&res).unwrap();
                            futures::future::ok(res_payload)
                        });
                    Response::fut(future)
                } else {
                    Response::reply(Ok("".to_owned()))
                }
            },
            _ => {
                Response::reply(Ok("".to_owned()))
            }
        };

        res
    }
}

impl StreamHandler<NodeRequest, std::io::Error> for NodeSession {
    fn handle(&mut self, msg: NodeRequest, ctx: &mut Context<Self>) {
        match msg {
            NodeRequest::Ping => {
                self.hb = Instant::now();
                // println!("Server got ping from {}", self.id.unwrap());
            },
            NodeRequest::Join(id) => {
                self.id = Some(id);
                self.network.do_send(PeerConnected(id, ctx.address()));
            },
            NodeRequest::Message(mid, type_id, body) => {
                let task = actix::fut::wrap_future(ctx.address().send(SendToRaft(type_id, body)))
                    .map_err(|err, _: &mut NodeSession, _| ())
                    .and_then(move |res, act, _| {
                        let payload = res.unwrap();
                        act.framed.write(NodeResponse::Result(mid, payload));
                        actix::fut::result(Ok(()))
                    });
                ctx.spawn(task);
            },
            _ => ()
        }
    }
}
