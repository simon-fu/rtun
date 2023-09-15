

use std::{collections::HashMap, time::Duration};

use anyhow::{Result, Context, bail};
use protobuf::MessageField;
use tokio_util::compat::{FuturesAsyncReadCompatExt, FuturesAsyncWriteCompatExt};

use crate::{actor_service::{ActorEntity, start_actor, handle_first_none, AsyncHandler, ActorHandle, wait_next_none, handle_next_none, handle_msg_none}, huid::{HUId, gen_huid::gen_huid}, channel::{ChSender, CHANNEL_SIZE, ChReceiver, ChId, ChSenderWeak}, async_rt::{spawn_with_name, spawn_with_inherit}, switch::{invoker_ctrl::{OpOpenShell, OpOpenShellResult, OpOpenSocks, OpOpenSocksResult, CtrlWeak, OpKickDown, OpKickDownResult, OpOpenP2P, OpOpenP2PResult}, next_ch_id::NextChId, agent::ch_socks::ChSocks, entity_watch::{CtrlGuard, OpWatch, WatchResult}}, ice::{ice_peer::{IcePeer, IceConfig, IceArgs}, throughput::run_throughput, webrtc_ice_peer::{WebrtcIcePeer, WebrtcIceConfig, DtlsIceArgs}, ice_quic::{UpgradeToQuic, QuicStream, QuicIceCert}}, proto::{OpenP2PResponse, open_p2presponse::Open_p2p_rsp, P2PArgs, p2pargs::P2p_args, P2PQuicArgs, QuicSocksArgs, QuicThroughputArgs, WebrtcThroughputArgs, P2PDtlsArgs}};
use tokio::{sync::mpsc, time::timeout};

use super::{super::invoker_ctrl::{CloseChannelResult, OpCloseChannel, CtrlHandler, CtrlInvoker}, ch_socks::run_socks_conn};

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

    pub async fn disable_shell(&self, v: bool) -> Result<()> {
        let r = self.handle.invoker().invoke(DisableShell(v)).await??;
        Ok(r)
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
        disable_shell: false,
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
        if self.disable_shell {
            bail!("shell disabled")
        }

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
        let p2p_args = req.0.p2p_args.take().with_context(||"no p2p args")?;
        match p2p_args {
            P2p_args::QuicSocks(mut args) => {
                let args = args.base.take().with_context(||"no base in quic socks args")?;
                return handle_quic_socks(self.socks_server.clone(), args).await
            },
            P2p_args::QuicThrput(args) => {
                return handle_quic_throughput(args).await
            },
            P2p_args::WebrtcThrput(args) => {
                return handle_webrtc_throughput(args).await
            }
        }

        // match tun_args {
        //     Tun_args::Throughput(thr_args) => {
        //         let ptype = thr_args.peer_type;
        //         if ptype == 11 {
        //             return handle_p2p_myice(ice_args, thr_args).await
        //         } else if ptype == 12 {
        //             return handle_p2p_webrtc(ice_args, thr_args).await
        //         } 
        //         bail!("unknown p2p type {ptype}", )
        //     },
        //     Tun_args::Socks(socks_args) => {
        //         return handle_p2p_socks(self.socks_server.clone(), ice_args, socks_args).await;
        //     }
        // }
    }
}

struct DisableShell(bool);

#[async_trait::async_trait]
impl AsyncHandler<DisableShell> for Entity {
    type Response = Result<()>; 

    async fn handle(&mut self, req: DisableShell) -> Self::Response {
        self.disable_shell = req.0;
        Ok(())
    }
}

async fn handle_quic_socks(socks_server: super::ch_socks::Server, mut remote_args: P2PQuicArgs) -> Result<OpenP2PResponse> {
    let remote_ice: IceArgs = remote_args.ice.take().with_context(||"no ice in P2PQuicArgs")?.into();


    let mut peer = IcePeer::with_config(IceConfig {
        servers: vec![
            "stun:stun1.l.google.com:19302".into(),
            "stun:stun2.l.google.com:19302".into(),
            "stun:stun.qq.com:3478".into(),
        ],
        // disable_dtls: remote_args.cert_fingerprint.is_none(),
        ..Default::default()
    });
    
    let uid = peer.uid();
    let local_ice = peer.server_gather(remote_ice.clone()).await?;
    tracing::debug!("starting quic tunnel {uid}, local {local_ice:?}, remote {remote_ice:?}");

    let local_cert = QuicIceCert::try_new()?;
    let local_cert_der = local_cert.to_bytes()?.into();
    

    spawn_with_name(format!("quic-socks-{uid}"), async move {
        // tracing::debug!("starting, local {local_args:?}, remote {remote_args:?}");

        let r = quic_tunnel_task(peer, local_cert, socks_server).await;
        
        tracing::debug!("finished {r:?}");
    });

    // peer.into_accept_and_chat(remote_args).await?;
    
    let rsp = OpenP2PResponse {
        open_p2p_rsp: Some(Open_p2p_rsp::Args(P2PArgs { 
            p2p_args: Some(P2p_args::QuicSocks(QuicSocksArgs {
                base: MessageField::some(P2PQuicArgs { 
                    ice: Some(local_ice.into()).into(), 
                    cert_der: local_cert_der, 
                    ..Default::default()
                }),
                ..Default::default()
            })), 
            ..Default::default()
        })),
        ..Default::default()
    };

    Ok(rsp)
}

async fn quic_tunnel_task(mut peer: IcePeer, local_cert: QuicIceCert, socks_server: super::ch_socks::Server,) -> Result<()> {

    
    let conn = peer.accept().await?;
    let peer_addr = conn.remote_addr();
    let conn = conn.upgrade_to_quic2(&local_cert, None).await?;

    loop {
        let pair = conn.accept_bi().await
        .with_context(||"accpet bi failed")?;
        let stream = QuicStream::new(pair);

        let uid = gen_huid();
        let name = format!("{uid}");
        let server = socks_server.clone();
        spawn_with_inherit(name, async move {
            let r = run_socks_conn(stream, peer_addr, server).await;
            tracing::debug!("finished {r:?}");
            r
        });
    }
    
}

async fn handle_quic_throughput(mut remote_args: QuicThroughputArgs) -> Result<OpenP2PResponse> {
    tracing::debug!("handle_quic_throughput");

    let mut base = remote_args.take_base();
    let thr_args = remote_args.take_throughput();
    
    let remote_args: IceArgs = base.take_ice().into();
    let remote_cert = base.take_cert_der();
    let local_cert = QuicIceCert::try_new()?;
    let local_cert_der = local_cert.to_bytes()?.into();
    
    // let remote_args: IceArgs = remote_args.into();

    let mut peer = IcePeer::with_config(IceConfig {
        servers: vec![
            "stun:stun1.l.google.com:19302".into(),
            "stun:stun2.l.google.com:19302".into(),
            "stun:stun.qq.com:3478".into(),
        ],
        ..Default::default()
    });
    
    tracing::debug!("remote_args {remote_args:?}");
    let local_args = peer.server_gather(remote_args).await?;
    tracing::debug!("local_args {local_args:?}");
    

    let rsp = OpenP2PResponse {
        open_p2p_rsp: Some(Open_p2p_rsp::Args(P2PArgs { 
            p2p_args: Some(P2p_args::QuicThrput(QuicThroughputArgs {
                base: MessageField::some(P2PQuicArgs { 
                    ice: Some(local_args.into()).into(), 
                    cert_der: local_cert_der, 
                    ..Default::default()
                }),
                throughput: Some(thr_args.clone()).into(),
                ..Default::default()
            })), 
            ..Default::default()
        })),
        ..Default::default()
    };

    spawn_with_name("p2p-throughput", async move {
        tracing::debug!("starting");

        let r = async move {
            let conn = peer.accept().await?
            .upgrade_to_quic2(&local_cert, Some(remote_cert)).await?;
            let (wr, rd) = conn.accept_bi().await?;
            run_throughput(rd, wr, thr_args).await
        }.await;
        
        tracing::debug!("finished {r:?}");
    });

    // peer.into_accept_and_chat(remote_args).await?;
    

    Ok(rsp)
}

async fn handle_webrtc_throughput(mut remote_args: WebrtcThroughputArgs) -> Result<OpenP2PResponse> {
    tracing::debug!("handle_webrtc_throughput");

    let mut base = remote_args.take_base();
    let thr_args = remote_args.take_throughput();

    // let remote_args: IceArgs = req.0.args.take().with_context(||"no remote p2p args")?.into();
    let remote_args = DtlsIceArgs {
        ice: base.take_ice().into(),
        cert_fingerprint: base.cert_fingerprint.map(|x|x.into()),
    };

    let mut peer = WebrtcIcePeer::with_config(WebrtcIceConfig {
        servers: vec![
            "stun:stun1.l.google.com:19302".into(),
            "stun:stun2.l.google.com:19302".into(),
            "stun:stun.qq.com:3478".into(),
        ],
        disable_dtls: remote_args.cert_fingerprint.is_none(),
        ..Default::default()
    });
    
    let local_args = peer.gather_until_done().await?;
    
    let rsp = OpenP2PResponse {
        open_p2p_rsp: Some(Open_p2p_rsp::Args(P2PArgs { 
            p2p_args: Some(P2p_args::WebrtcThrput(WebrtcThroughputArgs {
                base: MessageField::some(P2PDtlsArgs { 
                    ice: Some(local_args.ice.into()).into(), 
                    cert_fingerprint: local_args.cert_fingerprint.map(|x|x.into()),
                    ..Default::default()
                }),
                throughput: Some(thr_args.clone()).into(),
                ..Default::default()
            })), 
            ..Default::default()
        })),
        ..Default::default()
    };

    spawn_with_name("p2p-throughput", async move {
        tracing::debug!("starting");

        let r = async move {
            let conn = peer.kick_and_ugrade_to_kcp(remote_args, false).await?;
            let (rd, wr) = conn.split();
            // run_throughput(rd, wr, args).await
            timeout(
                Duration::from_secs(10), 
                run_throughput(rd.compat(), wr.compat_write(), thr_args)
            ).await?
        }.await;
        
        tracing::debug!("finished {r:?}");
    });

    // peer.into_accept_and_chat(remote_args).await?;
    
    // let rsp = OpenP2PResponse {
    //     open_p2p_rsp: Some(Open_p2p_rsp::Args(local_args.into())),
    //     ..Default::default()
    // };

    Ok(rsp)
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
    disable_shell: bool,
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

