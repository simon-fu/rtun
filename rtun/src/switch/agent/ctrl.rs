

use std::collections::HashMap;

use anyhow::{Result, Context};
use tokio_util::compat::{TokioAsyncWriteCompatExt, TokioAsyncReadCompatExt};

use crate::{actor_service::{ActorEntity, start_actor, handle_first_none, AsyncHandler, ActorHandle, wait_next_none, handle_next_none, handle_msg_none}, huid::HUId, channel::{ChSender, CHANNEL_SIZE, ChReceiver, ChId, ChSenderWeak}, async_rt::spawn_with_name, switch::{invoker_ctrl::{OpOpenShell, OpOpenShellResult, OpOpenSocks, OpOpenSocksResult, CtrlWeak, OpKickDown, OpKickDownResult, OpOpenP2P, OpOpenP2PResult}, next_ch_id::NextChId, agent::ch_socks::ChSocks, entity_watch::{CtrlGuard, OpWatch, WatchResult}}, ice::{ice_peer::{IcePeer, IceConfig, IceArgs}, throughput::run_throughput}, proto::{OpenP2PResponse, open_p2presponse::Open_p2p_rsp, open_p2pargs::Tun_args}};
use tokio::sync::mpsc;

use super::super::invoker_ctrl::{CloseChannelResult, OpCloseChannel, CtrlHandler, CtrlInvoker};

use super::ch_shell::open_shell;

pub type AgentCtrlInvoker = CtrlInvoker<Entity>;

pub struct AgentCtrl {
    handle: ActorHandle<Entity>,
}

impl  AgentCtrl { 

    pub fn clone_ctrl(&self) -> AgentCtrlInvoker {
        CtrlInvoker::new(self.handle.invoker().clone())
    }

    pub async fn shutdown(&self) {
        self.handle.invoker().shutdown().await;
    }

    pub async fn wait_for_completed(&mut self) -> Result<()> {
        self.handle.wait_for_completed().await?;
        Ok(())
    }
}


pub async fn make_agent_ctrl(uid: HUId) -> Result<AgentCtrl> {

    let entity = Entity {
        next_ch_id: Default::default(),
        uid,
        socks_server: super::ch_socks::Server::try_new("127.0.0.1:1080").await?,
        channels: Default::default(),
        weak: None,
        guard: CtrlGuard::new(),
    };

    let handle = start_actor(
        format!("agent-ctrl-{}", uid),
        entity, 
        handle_first_none,
        wait_next_none, 
        handle_next_none, 
        handle_msg_none,
    );
    
    let session = AgentCtrl {
        handle,
    };
    
    let weak = session.clone_ctrl().downgrade();

    session.handle.invoker().invoke(SetWeak(weak)).await?;

    Ok(session)
}


// #[async_trait::async_trait]
// impl AsyncHandler<OpOpenChannel> for Entity {
//     type Response = OpenChannelResult; 

//     async fn handle(&mut self, req: OpOpenChannel) -> Self::Response {
//         let local_ch_id = self.next_ch_id.next_ch_id();
//         let peer_tx = req.0;

//         // let (mux_tx, mux_rx) = mpsc::channel(CHANNEL_SIZE);
//         let (local_tx, local_rx) = self.add_channel(local_ch_id, &peer_tx);
        
//         tracing::debug!("open channel {local_ch_id:?} -> {:?}", peer_tx.ch_id());

//         let name = format!("{}-ch-{}->{}", self.uid, local_ch_id, peer_tx.ch_id());

//         {
//             let weak = self.weak.clone();
//             spawn_with_name(name,  async move {
//                 let r = channel_service(peer_tx, local_rx ).await;
//                 tracing::debug!("finished with {:?}", r);

//                 if let Some(weak) = weak {
//                     if let Some(ctrl) = weak.upgrade() {
//                         let _r = ctrl.close_channel(local_ch_id).await;
//                     }
//                 }
//             });
//         }
        
//         Ok(local_tx)
//     }
// }

#[async_trait::async_trait]
impl AsyncHandler<OpCloseChannel> for Entity {
    type Response = CloseChannelResult; 

    async fn handle(&mut self, req: OpCloseChannel) -> Self::Response {
        Ok(self.remove_channel(req.0))
    }
}

#[async_trait::async_trait]
impl AsyncHandler<OpOpenShell> for Entity {
    type Response = OpOpenShellResult; 

    async fn handle(&mut self, req: OpOpenShell) -> Self::Response {

        let exec = open_shell(req.1).await?;

        let peer_tx = req.0;
        let local_ch_id = self.next_ch_id.next_ch_id();

        let (local_tx, local_rx) = self.add_channel(local_ch_id, &peer_tx);
        
        tracing::debug!("open shell {local_ch_id:?} -> {:?}", peer_tx.ch_id());

        {
            let name = format!("local-{}-{}->{}", self.uid, local_ch_id, peer_tx.ch_id());
            let weak = self.weak.clone();

            spawn_with_name(name, async move {
    
                let r = exec.run(peer_tx, local_rx).await;
                tracing::debug!("finished with [{:?}]", r);
    
                if let Some(weak) = weak {
                    if let Some(ctrl) = weak.upgrade() {
                        let _r = ctrl.close_channel(local_ch_id).await;
                    }
                }
    
            });
        }

        // shell.spawn(Some(name), peer_tx, local_rx, self.weak.clone(), local_ch_id);

        Ok(local_tx)

    }
}


#[async_trait::async_trait]
impl AsyncHandler<OpOpenSocks> for Entity {
    type Response = OpOpenSocksResult; 

    async fn handle(&mut self, req: OpOpenSocks) -> Self::Response {
        
        let exec = ChSocks::try_new(req.1)?;

        let peer_tx = req.0;
        let local_ch_id = self.next_ch_id.next_ch_id();

        let (local_tx, local_rx) = self.add_channel(local_ch_id, &peer_tx);

        // let (mux_tx, mux_rx) = mpsc::channel(CHANNEL_SIZE);
        
        tracing::debug!("open socks {local_ch_id:?} -> {:?}", peer_tx.ch_id());

        let name = format!("socks-{}-{}->{}", self.uid, local_ch_id, peer_tx.ch_id());
        let weak = self.weak.clone();
        let server = self.socks_server.clone();

        spawn_with_name(name, async move {

            let r = exec.run(server, peer_tx, local_rx).await;
            tracing::debug!("finished with [{:?}]", r);

            if let Some(weak) = weak {
                if let Some(ctrl) = weak.upgrade() {
                    let _r = ctrl.close_channel(local_ch_id).await;
                }
            }

        });

        Ok(local_tx)

    }
}

#[async_trait::async_trait]
impl AsyncHandler<OpWatch> for Entity {
    type Response = WatchResult; 

    async fn handle(&mut self, _req: OpWatch) -> Self::Response {
        Ok(self.guard.watch())
    }
}

#[async_trait::async_trait]
impl AsyncHandler<OpKickDown> for Entity {
    type Response = OpKickDownResult; 

    async fn handle(&mut self, _req: OpKickDown) -> Self::Response {
        Ok(())
    }
}

#[async_trait::async_trait]
impl AsyncHandler<OpOpenP2P> for Entity {
    type Response = OpOpenP2PResult; 

    async fn handle(&mut self, mut req: OpOpenP2P) -> Self::Response {
        let remote_args: IceArgs = req.0.args.take().with_context(||"no remote p2p args")?.into();

        let mut peer = IcePeer::with_config(IceConfig {
            servers: vec![
                // "stun:stun1.l.google.com:19302".into(),
                // "stun:stun2.l.google.com:19302".into(),
                "stun:stun.qq.com:3478".into(),
            ],
            disable_dtls: remote_args.cert_fingerprint.is_none(),
            ..Default::default()
        });
        
        let local_args = peer.gather_until_done().await?;
        
        match req.0.tun_args {
            Some(Tun_args::Throughput(args)) => {
                spawn_with_name("p2p-throughput", async move {
                    tracing::debug!("starting");
                    
                    let r = async move {
                        let conn = peer.accept(remote_args).await?;
                        let (wr, rd) = conn.accept_bi().await?;
                        run_throughput(rd.compat(), wr.compat_write(), args).await
                    }.await;
                    
                    tracing::debug!("finished {r:?}");
                });
            },
            _ => {

            }
        }

        // peer.into_accept_and_chat(remote_args).await?;
        
        let rsp = OpenP2PResponse {
            open_p2p_rsp: Some(Open_p2p_rsp::Args(local_args.into())),
            ..Default::default()
        };

        Ok(rsp)

        // let r = PunchPeer::bind_and_detect("0.0.0.0:0").await;
        // match r {
        //     Ok((peer, nat)) => {
        //         let nat_type = nat.nat_type();
        //         if nat_type != Some(NatType::Cone) {
        //             tracing::warn!("nat type {nat_type:?}");
        //         } else {
        //             tracing::debug!("nat type {nat_type:?}");
        //         }

        //         let local_ufrag = peer.local_ufrag().to_string();
        //         let mapped = nat.into_mapped().with_context(||"empty mapped address")?;
        //         let args = req.0.args.take().with_context(||"no remote p2p args")?;

        //         tracing::debug!("remote p2p args {args:?}");
        //         let remote_addr = args.addr.parse().with_context(||"parse remote addr failed")?;

        //         launch_p2p(peer, args.ufrag.into(), remote_addr)?;
                
        //         let rsp = OpenP2PResponse {
        //             open_p2p_rsp: Some(Open_p2p_rsp::Args(P2PArgs {
        //                 ufrag: local_ufrag.into(),
        //                 addr: mapped.to_string().into(),
        //                 ..Default::default()
        //             })),
        //             ..Default::default()
        //         };

        //         Ok(rsp)
        //     },
        //     Err(e) => Err(e.into()),
        // }
    }
}


impl CtrlHandler for Entity {}

#[async_trait::async_trait]
impl AsyncHandler<SetWeak> for Entity {
    type Response = (); 

    async fn handle(&mut self, req: SetWeak) -> Self::Response {
        self.weak = Some(req.0);
    }
}

struct SetWeak(CtrlWeak<Entity>);


pub struct Entity {
    uid: HUId,
    next_ch_id: NextChId,
    socks_server: super::ch_socks::Server,
    channels: HashMap<ChId, ChItem>,
    weak: Option<CtrlWeak<Self>>,
    guard: CtrlGuard,
}

impl Entity {
    fn add_channel(&mut self, ch_id: ChId, peer_tx: &ChSender) -> (ChSender, ChReceiver) {

        let (local_tx, local_rx) = mpsc::channel(CHANNEL_SIZE);
        let local_tx = ChSender::new(ch_id, local_tx);
        let local_rx = ChReceiver::new(local_rx);
        
        self.channels.insert(ch_id, ChItem {
            peer_tx: peer_tx.downgrade(),
            local_tx: local_tx.downgrade(),
        });

        ( local_tx, local_rx, )
    }

    fn remove_channel(&mut self, ch_id: ChId) -> bool {
        self.channels.remove(&ch_id).is_some()
    }
}


impl ActorEntity for Entity {
    type Next = ();

    type Msg = ();

    type Result = ();

    fn into_result(self, _r: Result<()>) -> Self::Result {
        ()
    }
}

struct ChItem {
    peer_tx: ChSenderWeak,
    local_tx: ChSenderWeak,
}

impl Drop for ChItem {
    fn drop(&mut self) {
        if let Some(tx) = self.local_tx.upgrade() {
            let _r = tx.try_send_zero();
        }

        if let Some(tx) = self.peer_tx.upgrade() {
            let _r = tx.try_send_zero();
        }
    }
}

