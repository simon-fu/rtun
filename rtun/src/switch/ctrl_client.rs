

use std::time::Duration;

use anyhow::{Result, anyhow, bail, Context};
use chrono::Local;
use protobuf::Message;

use crate::{actor_service::{ActorEntity, start_actor, handle_first_none, AsyncHandler, ActorHandle, handle_msg_none, Action}, huid::HUId, channel::{ChId, ChPair}, proto::{OpenChannelResponse, open_channel_response::Open_ch_rsp, OpenShellArgs, C2ARequest, c2arequest::C2a_req_args, OpenSocksArgs, CloseChannelArgs, ResponseStatus, Ping, Pong, KickDownArgs, OpenP2PResponse, P2PArgs}};

use super::{invoker_ctrl::{CloseChannelResult, OpCloseChannel, CtrlHandler, CtrlInvoker, OpOpenShell, OpOpenShellResult, OpOpenSocks, OpOpenSocksResult, OpKickDown, OpKickDownResult, OpOpenP2P, OpOpenP2PResult}, invoker_switch::{SwitchInvoker, SwitchHanlder}, next_ch_id::NextChId, entity_watch::{OpWatch, WatchResult, CtrlGuard, CtrlWatch}};

pub type CtrlClientSession<H> = CtrlClient<Entity<H>>;
pub type CtrlClientInvoker<H> = CtrlInvoker<Entity<H>>;


pub struct CtrlClient<H: SwitchHanlder> {
    handle: ActorHandle<Entity<H>>,
}

impl<H: SwitchHanlder>  CtrlClient<H> { 

    pub fn clone_invoker(&self) -> CtrlInvoker<Entity<H>> {
        CtrlInvoker::new(self.handle.invoker().clone())
    }

    pub async fn shutdown(&self) {
        self.handle.invoker().shutdown().await;
    }

    pub async fn wait_for_completed(&mut self) -> Result<()> {
        self.handle.wait_for_completed().await?;
        Ok(())
    }

    pub async fn shutdown_and_waitfor(&mut self) -> Result<()> {
        self.handle.invoker().shutdown().await;
        self.handle.wait_for_completed().await?;
        Ok(())
    }
}


pub async fn make_ctrl_client<H: SwitchHanlder>(uid: HUId, pair: ChPair, switch: SwitchInvoker<H>, disable_bridge_ch: bool) -> Result<CtrlClient<H>> {

    // let mux_tx = switch.get_mux_tx().await?;
    let switch_watch = switch.watch().await?;

    let entity = Entity {
        // gen_ch_id: ChId(0),
        // uid,
        pair,
        switch,
        // mux_tx,
        next_ch_id: Default::default(),
        guard: CtrlGuard::new(),
        switch_watch,
        disable_bridge_ch,
    };

    let handle = start_actor(
        format!("ctrl-client-{}", uid),
        entity, 
        handle_first_none,
        wait_next, 
        handle_next, 
        handle_msg_none,
    );

    Ok(CtrlClient {
        handle,
    })
}


// #[async_trait::async_trait]
// impl<H: SwitchHanlder> AsyncHandler<OpOpenChannel> for Entity<H> {
//     type Response = OpenChannelResult; 

//     async fn handle(&mut self, req: OpOpenChannel) -> Self::Response {
//         let data = OpenChannelRequest {
//             open_ch_req: None,
//             ..Default::default()
//         }.write_to_bytes()?;

//         self.pair.tx.send_data(data.into()).await.map_err(|_e|anyhow!("send failed"))?;

//         let packet = self.pair.rx.recv_packet().await.with_context(||"recv failed")?;

//         let rsp = OpenChannelResponse::parse_from_bytes(&packet.payload)
//         .with_context(||"parse response failed")?
//         .open_ch_rsp.with_context(||"has no response")?;
        
//         let ch_id = match rsp {
//             Open_ch_rsp::ChId(v) => ChId(v),
//             Open_ch_rsp::Status(status) => bail!("response status {:?}", status),
//             // _ => bail!("unknown"),
//         };

//         let tx = self.switch.add_channel(ch_id, req.0).await?;
        
//         Ok(tx)
//     }
// }

#[async_trait::async_trait]
impl<H: SwitchHanlder> AsyncHandler<OpCloseChannel> for Entity<H> {
    type Response = CloseChannelResult; 

    async fn handle(&mut self, req: OpCloseChannel) -> Self::Response { 
        if self.disable_bridge_ch {
            bail!("bridge ch disabled")
        }

        let r = c2a_close_channel(&mut self.pair, CloseChannelArgs {
            ch_id: req.0.0,
            ..Default::default()
        }).await;

        // tracing::debug!("close channel result [{r:?}]");
        if let Err(e) = r {
            tracing::debug!("close remote channel failed [{e:?}]");
        }

        let r = self.switch.remove_channel(req.0).await?;
        Ok(r)
    }
}

#[async_trait::async_trait]
impl<H: SwitchHanlder> AsyncHandler<OpOpenShell> for Entity<H> {
    type Response = OpOpenShellResult; 

    async fn handle(&mut self, mut req: OpOpenShell) -> Self::Response {

        if self.disable_bridge_ch {
            bail!("bridge ch disabled")
        }

        let req_ch_id = self.next_ch_id.next_ch_id();
        req.1.ch_id = Some(req_ch_id.0);

        let tx = self.switch.add_channel(req_ch_id, req.0).await?;

        let r = c2a_open_shell(&mut self.pair, req.1).await;
        match r {
            Ok(v) => {
                assert_eq!(v, req_ch_id);
                Ok(tx)
            }
            Err(e) => {
                let _r = self.switch.remove_channel(req_ch_id).await;
                Err(e)
            }
        }
    }
}

#[async_trait::async_trait]
impl<H: SwitchHanlder> AsyncHandler<OpOpenSocks> for Entity<H> {
    type Response = OpOpenSocksResult; 

    async fn handle(&mut self, mut req: OpOpenSocks) -> Self::Response {
        if self.disable_bridge_ch {
            bail!("bridge ch disabled")
        }

        let req_ch_id = self.next_ch_id.next_ch_id();
        req.1.ch_id = Some(req_ch_id.0);

        let tx = self.switch.add_channel(req_ch_id, req.0).await?;

        let r = c2a_open_socks(&mut self.pair, req.1).await;
        match r {
            Ok(v) => {
                assert_eq!(v, req_ch_id);
                Ok(tx)
            }
            Err(e) => {
                let _r = self.switch.remove_channel(req_ch_id).await;
                Err(e)
            }
        }
    }
}

#[async_trait::async_trait]
impl<H: SwitchHanlder> AsyncHandler<OpWatch> for Entity<H> {
    type Response = WatchResult; 

    async fn handle(&mut self, _req: OpWatch) -> Self::Response {
        Ok(self.guard.watch())
    }
}

#[async_trait::async_trait]
impl<H: SwitchHanlder> AsyncHandler<OpKickDown> for Entity<H> {
    type Response = OpKickDownResult; 

    async fn handle(&mut self, req: OpKickDown) -> Self::Response {
        let r = c2a_kick_down(&mut self.pair, req.0).await;
        match r {
            Ok(_v) => {
                Ok(())
            }
            Err(e) => {
                Err(e)
            }
        }
    }
}

#[async_trait::async_trait]
impl<H: SwitchHanlder> AsyncHandler<OpOpenP2P> for Entity<H> {
    type Response = OpOpenP2PResult; 

    async fn handle(&mut self, req: OpOpenP2P) -> Self::Response {
        let r = c2a_open_p2p(&mut self.pair, req.0).await;
        match r {
            Ok(args) => {
                Ok(args)
            }
            Err(e) => {
                Err(e)
            }
        }
    }
}

// struct SetNoSocks(bool);

// #[async_trait::async_trait]
// impl<H: SwitchHanlder> AsyncHandler<SetNoSocks> for Entity<H> {
//     type Response = Result<()>; 

//     async fn handle(&mut self, req: SetNoSocks) -> Self::Response {
//         self.disable_bridge_ch = req.0;
//         Ok(())
//     }
// }

impl<H: SwitchHanlder> CtrlHandler for Entity<H> {}



pub struct Entity<H: SwitchHanlder> {
    // uid: HUId,
    // gen_ch_id: ChId,
    pair: ChPair,
    switch: SwitchInvoker<H>,
    // mux_tx: ChTx,
    next_ch_id: NextChId,
    guard: CtrlGuard,
    switch_watch: CtrlWatch,
    disable_bridge_ch: bool,
}

// impl<H: SwitchHanlder> Entity<H> {
//     fn next_ch_id(&mut self) -> ChId {
//         let ch_id = self.gen_ch_id;
//         self.gen_ch_id.0 += 1;
//         ch_id
//     }
// }

pub enum Next {
    SwitchGone,
    CtrlChBroken,
    Ping,
}

async fn wait_next<H: SwitchHanlder>(entity: &mut Entity<H>) -> Next {
    let duration = Duration::from_secs(10);
    tokio::select! {
        _r = entity.switch_watch.watch() => Next::SwitchGone,
        _r = entity.pair.rx.recv_packet() => Next::CtrlChBroken,
        _r = tokio::time::sleep(duration) => Next::Ping,
    }
}

async fn handle_next<H: SwitchHanlder>(entity: &mut Entity<H>, next: Next) -> Result<Action> {
    match next {
        Next::SwitchGone => { tracing::debug!("switch has gone"); },
        Next::CtrlChBroken => { tracing::debug!("ctrl channel broken"); },
        Next::Ping => {
            let timeout = Duration::from_millis(90*1000);
            let r = tokio::time::timeout(timeout, c2a_ping(&mut entity.pair, Ping {
                timestamp: Local::now().timestamp_millis(),
                ..Default::default()
            })).await;

            match r {
                Ok(r) => {
                    let pong = r?;
                    let elapsed = Local::now().timestamp_millis() - pong.timestamp;
                    tracing::debug!("ping/pong latency {elapsed} ms\r");
                },
                Err(_elapsed) => {
                    tracing::warn!("ping/pong timeout {timeout:?}, shutdown switch\r");
                    entity.switch.shutdown().await
                },
            }

            // let pong = c2a_ping(&mut entity.pair, Ping {
            //     timestamp: Local::now().timestamp_millis(),
            //     ..Default::default()
            // }).await?;
            // let elapsed = Local::now().timestamp_millis() - pong.timestamp;
            // tracing::debug!("ping/pong elapsed {elapsed} ms\r");

            
            return Ok(Action::None)
        },
    }
    Ok(Action::Finished)
}

impl<H: SwitchHanlder> ActorEntity for Entity<H> {
    type Next = Next;

    type Msg = ();

    type Result = ();

    fn into_result(self, _r: Result<()>) -> Self::Result {
        ()
    }
}


pub async fn c2a_open_shell(pair: &mut ChPair, args: OpenShellArgs) -> Result<ChId> {
    let data = C2ARequest {
        c2a_req_args: Some(C2a_req_args::OpenSell(args)),
        ..Default::default()
    }.write_to_bytes()?;

    pair.tx.send_data(data.into()).await.map_err(|_e|anyhow!("send open shell failed"))?;

    let packet = pair.rx.recv_packet().await.with_context(||"recv open shell response failed")?;

    // let rsp = C2AResponse::parse_from_bytes(&data).with_context(||"parse open shell response failed")?;
    let rsp = OpenChannelResponse::parse_from_bytes(&packet.payload)
    .with_context(||"parse open shell response failed")?
    .open_ch_rsp.with_context(||"has no response")?;

    let shell_ch_id = match rsp {
        Open_ch_rsp::ChId(v) => ChId(v),
        Open_ch_rsp::Status(status) => bail!("open shell response status {:?}", status),
        // _ => bail!("unknown"),
    };

    // let pair = self.invoker.add_channel(shell_ch_id).await?;
    Ok(shell_ch_id)
}

pub async fn c2a_open_socks(pair: &mut ChPair, args: OpenSocksArgs) -> Result<ChId> {
    let data = C2ARequest {
        c2a_req_args: Some(C2a_req_args::OpenSocks(args)),
        ..Default::default()
    }.write_to_bytes()?;

    pair.tx.send_data(data.into()).await.map_err(|_e|anyhow!("send open socks failed"))?;

    let packet = pair.rx.recv_packet().await.with_context(||"recv open socks response failed")?;

    // let rsp = C2AResponse::parse_from_bytes(&data).with_context(||"parse open socks response failed")?;
    let rsp = OpenChannelResponse::parse_from_bytes(&packet.payload)
    .with_context(||"parse open socks response failed")?
    .open_ch_rsp.with_context(||"has no response")?;

    let opened_ch_id = match rsp {
        Open_ch_rsp::ChId(v) => ChId(v),
        Open_ch_rsp::Status(status) => bail!("open socks response status {status}"),
        // _ => bail!("unknown"),
    };

    // let pair = self.invoker.add_channel(shell_ch_id).await?;
    Ok(opened_ch_id)
}

pub async fn c2a_close_channel(pair: &mut ChPair, args: CloseChannelArgs) -> Result<ResponseStatus> {
    let ch_id = ChId(args.ch_id);

    let data = C2ARequest {
        c2a_req_args: Some(C2a_req_args::CloseChannel(args)),
        ..Default::default()
    }.write_to_bytes()?;

    pair.tx.send_data(data.into()).await.map_err(|_e|anyhow!("send close ch failed"))?;

    let packet = pair.rx.recv_packet().await
    .with_context(||format!("recv close ch response failed {:?}", ch_id))?;

    // let rsp = C2AResponse::parse_from_bytes(&data).with_context(||"parse close ch response failed")?;
    let status = ResponseStatus::parse_from_bytes(&packet.payload)
    .with_context(||"parse close ch response failed")?;

    Ok(status)
}

pub async fn c2a_ping(pair: &mut ChPair, args: Ping) -> Result<Pong> {

    let data = C2ARequest {
        c2a_req_args: Some(C2a_req_args::Ping(args)),
        ..Default::default()
    }.write_to_bytes()?;

    pair.tx.send_data(data.into()).await.map_err(|_e|anyhow!("send close ch failed"))?;

    let packet = pair.rx.recv_packet().await
    .with_context(||"recv ping failed")?;

    let pong = Pong::parse_from_bytes(&packet.payload)
    .with_context(||"parse pong failed")?;

    Ok(pong)
}

pub async fn c2a_kick_down(pair: &mut ChPair, args: KickDownArgs) -> Result<ResponseStatus> {

    let data = C2ARequest {
        c2a_req_args: Some(C2a_req_args::KickDown(args)),
        ..Default::default()
    }.write_to_bytes()?;

    pair.tx.send_data(data.into()).await.map_err(|_e|anyhow!("send close ch failed"))?;

    let packet = pair.rx.recv_packet().await
    .with_context(||"recv ping failed")?;

    let status = ResponseStatus::parse_from_bytes(&packet.payload)
    .with_context(||"parse kick down response failed")?;

    Ok(status)
}

pub async fn c2a_open_p2p(pair: &mut ChPair, args: P2PArgs) -> Result<OpenP2PResponse> {

    let data = C2ARequest {
        c2a_req_args: Some(C2a_req_args::OpenP2p(args)),
        ..Default::default()
    }.write_to_bytes()?;

    pair.tx.send_data(data.into()).await.map_err(|_e|anyhow!("send close ch failed"))?;

    let packet = pair.rx.recv_packet().await
    .with_context(||"recv ping failed")?;

    let rsp = OpenP2PResponse::parse_from_bytes(&packet.payload)
    .with_context(||"parse open p2p response failed")?;

    Ok(rsp)
}
