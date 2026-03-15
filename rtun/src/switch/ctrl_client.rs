use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use chrono::Local;
use protobuf::Message;
use tokio::{sync::mpsc, task::JoinHandle};

use crate::{
    async_rt::spawn_with_name,
    actor_service::{
        handle_first_none, handle_msg_none, start_actor, Action, ActorEntity, ActorHandle,
        AsyncHandler,
    },
    channel::{ChId, ChPair, ChReceiver, ChSender},
    huid::HUId,
    proto::{
        c2arequest::C2a_req_args, ctrl_channel_packet, ctrl_rpc_response,
        open_channel_response::Open_ch_rsp, C2ARequest, CloseChannelArgs, CtrlChannelPacket,
        CtrlRpcResponse, ExecAgentScriptArgs, ExecAgentScriptResult, HardNatControlEnvelope,
        KickDownArgs, OpenChannelResponse, OpenP2PResponse, OpenShellArgs, OpenSocksArgs,
        P2PArgs, Ping, Pong, ResponseStatus,
    },
};

use super::{
    entity_watch::{CtrlGuard, CtrlWatch, OpWatch, WatchResult},
    invoker_ctrl::{
        CloseChannelResult, CtrlHandler, CtrlInvoker, OpCloseChannel, OpExecAgentScript,
        OpExecAgentScriptResult, OpKickDown, OpKickDownResult, OpOpenP2P, OpOpenP2PResult,
        OpOpenShell, OpOpenShellResult, OpOpenSocks, OpOpenSocksResult,
    },
    invoker_switch::{SwitchHanlder, SwitchInvoker},
    next_ch_id::NextChId,
};

pub type CtrlClientSession<H> = CtrlClient<Entity<H>>;
pub type CtrlClientInvoker<H> = CtrlInvoker<Entity<H>>;

pub struct CtrlClient<H: SwitchHanlder> {
    handle: ActorHandle<Entity<H>>,
}

impl<H: SwitchHanlder> CtrlClient<H> {
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

pub async fn make_ctrl_client<H: SwitchHanlder>(
    uid: HUId,
    pair: ChPair,
    switch: SwitchInvoker<H>,
    disable_bridge_ch: bool,
) -> Result<CtrlClient<H>> {
    // let mux_tx = switch.get_mux_tx().await?;
    let switch_watch = switch.watch().await?;
    let (ctrl_tx, ctrl_rx) = pair.split();
    let (rpc_tx, rpc_rx) = mpsc::channel(16);
    let (event_tx, event_rx) = mpsc::channel(16);

    let reader_task = spawn_with_name(format!("ctrl-client-reader-{}", uid), async move {
        ctrl_reader_loop(ctrl_rx, rpc_tx, event_tx).await
    });

    let entity = Entity {
        // gen_ch_id: ChId(0),
        // uid,
        ctrl_tx,
        rpc_rx,
        event_rx,
        reader_task,
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

    Ok(CtrlClient { handle })
}

enum CtrlRpcPacket {
    Raw(Bytes),
    Wrapped(CtrlRpcResponse),
}

enum CtrlEvent {
    HardNatControl(HardNatControlEnvelope),
    CtrlChBroken,
}

enum DecodedCtrlClientPacket {
    LegacyRpc(Bytes),
    HardNatControl(HardNatControlEnvelope),
}

fn decode_ctrl_client_packet(payload: Bytes) -> Result<DecodedCtrlClientPacket> {
    let legacy = payload.clone();
    match CtrlChannelPacket::parse_from_bytes(&payload) {
        Ok(packet) => match packet.body {
            Some(ctrl_channel_packet::Body::HardNatControl(env)) => {
                Ok(DecodedCtrlClientPacket::HardNatControl(env))
            }
            Some(ctrl_channel_packet::Body::RpcResponse(_))
            | Some(ctrl_channel_packet::Body::RpcRequest(_))
            | None => Ok(DecodedCtrlClientPacket::LegacyRpc(legacy)),
        },
        Err(_) => Ok(DecodedCtrlClientPacket::LegacyRpc(legacy)),
    }
}

async fn ctrl_reader_loop(
    mut ctrl_rx: ChReceiver,
    rpc_tx: mpsc::Sender<CtrlRpcPacket>,
    event_tx: mpsc::Sender<CtrlEvent>,
) {
    loop {
        let packet = match ctrl_rx.recv_packet().await {
            Ok(packet) => packet,
            Err(_e) => {
                let _r = event_tx.send(CtrlEvent::CtrlChBroken).await;
                return;
            }
        };

        match decode_ctrl_client_packet(packet.payload) {
            Ok(DecodedCtrlClientPacket::LegacyRpc(payload)) => {
                if rpc_tx.send(CtrlRpcPacket::Raw(payload)).await.is_err() {
                    return;
                }
            }
            Ok(DecodedCtrlClientPacket::HardNatControl(env)) => {
                if event_tx.send(CtrlEvent::HardNatControl(env)).await.is_err() {
                    return;
                }
            }
            Err(e) => {
                tracing::debug!("decode ctrl client packet failed: {e:#}");
                let _r = event_tx.send(CtrlEvent::CtrlChBroken).await;
                return;
            }
        }
    }
}

async fn send_legacy_rpc_request(ctrl_tx: &ChSender, req: C2ARequest, context: &str) -> Result<()> {
    let data = req.write_to_bytes()?;
    ctrl_tx
        .send_data(data.into())
        .await
        .map_err(|_e| anyhow!("{context}"))?;
    Ok(())
}

async fn recv_ctrl_rpc_packet(
    rpc_rx: &mut mpsc::Receiver<CtrlRpcPacket>,
    context: &str,
) -> Result<CtrlRpcPacket> {
    rpc_rx.recv().await.with_context(|| context.to_string())
}

fn decode_open_channel_response(packet: CtrlRpcPacket, context: &str) -> Result<OpenChannelResponse> {
    match packet {
        CtrlRpcPacket::Raw(payload) => {
            OpenChannelResponse::parse_from_bytes(&payload).with_context(|| context.to_string())
        }
        CtrlRpcPacket::Wrapped(rsp) => match rsp.body.with_context(|| context.to_string())? {
            ctrl_rpc_response::Body::OpenChannel(rsp) => Ok(rsp),
            _ => bail!("{context}: unexpected wrapped rpc body"),
        },
    }
}

fn decode_response_status(packet: CtrlRpcPacket, context: &str) -> Result<ResponseStatus> {
    match packet {
        CtrlRpcPacket::Raw(payload) => {
            ResponseStatus::parse_from_bytes(&payload).with_context(|| context.to_string())
        }
        CtrlRpcPacket::Wrapped(rsp) => match rsp.body.with_context(|| context.to_string())? {
            ctrl_rpc_response::Body::Status(rsp) => Ok(rsp),
            _ => bail!("{context}: unexpected wrapped rpc body"),
        },
    }
}

fn decode_pong(packet: CtrlRpcPacket, context: &str) -> Result<Pong> {
    match packet {
        CtrlRpcPacket::Raw(payload) => Pong::parse_from_bytes(&payload).with_context(|| context.to_string()),
        CtrlRpcPacket::Wrapped(rsp) => match rsp.body.with_context(|| context.to_string())? {
            ctrl_rpc_response::Body::Pong(rsp) => Ok(rsp),
            _ => bail!("{context}: unexpected wrapped rpc body"),
        },
    }
}

fn decode_open_p2p_response(packet: CtrlRpcPacket, context: &str) -> Result<OpenP2PResponse> {
    match packet {
        CtrlRpcPacket::Raw(payload) => {
            OpenP2PResponse::parse_from_bytes(&payload).with_context(|| context.to_string())
        }
        CtrlRpcPacket::Wrapped(rsp) => match rsp.body.with_context(|| context.to_string())? {
            ctrl_rpc_response::Body::OpenP2p(rsp) => Ok(rsp),
            _ => bail!("{context}: unexpected wrapped rpc body"),
        },
    }
}

fn decode_exec_agent_script_response(
    packet: CtrlRpcPacket,
    context: &str,
) -> Result<ExecAgentScriptResult> {
    match packet {
        CtrlRpcPacket::Raw(payload) => {
            ExecAgentScriptResult::parse_from_bytes(&payload).with_context(|| context.to_string())
        }
        CtrlRpcPacket::Wrapped(rsp) => match rsp.body.with_context(|| context.to_string())? {
            ctrl_rpc_response::Body::ExecAgentScript(rsp) => Ok(rsp),
            _ => bail!("{context}: unexpected wrapped rpc body"),
        },
    }
}

async fn entity_open_shell(
    ctrl_tx: &ChSender,
    rpc_rx: &mut mpsc::Receiver<CtrlRpcPacket>,
    args: OpenShellArgs,
) -> Result<ChId> {
    send_legacy_rpc_request(
        ctrl_tx,
        C2ARequest {
            c2a_req_args: Some(C2a_req_args::OpenSell(args)),
            ..Default::default()
        },
        "send open shell failed",
    )
    .await?;

    let packet = recv_ctrl_rpc_packet(rpc_rx, "recv open shell response failed").await?;
    let rsp = decode_open_channel_response(packet, "parse open shell response failed")?
        .open_ch_rsp
        .with_context(|| "has no response")?;

    match rsp {
        Open_ch_rsp::ChId(v) => Ok(ChId(v)),
        Open_ch_rsp::Status(status) => bail!("open shell response status {:?}", status),
    }
}

async fn entity_open_socks(
    ctrl_tx: &ChSender,
    rpc_rx: &mut mpsc::Receiver<CtrlRpcPacket>,
    args: OpenSocksArgs,
) -> Result<ChId> {
    send_legacy_rpc_request(
        ctrl_tx,
        C2ARequest {
            c2a_req_args: Some(C2a_req_args::OpenSocks(args)),
            ..Default::default()
        },
        "send open socks failed",
    )
    .await?;

    let packet = recv_ctrl_rpc_packet(rpc_rx, "recv open socks response failed").await?;
    let rsp = decode_open_channel_response(packet, "parse open socks response failed")?
        .open_ch_rsp
        .with_context(|| "has no response")?;

    match rsp {
        Open_ch_rsp::ChId(v) => Ok(ChId(v)),
        Open_ch_rsp::Status(status) => bail!("open socks response status {status}"),
    }
}

async fn entity_close_channel(
    ctrl_tx: &ChSender,
    rpc_rx: &mut mpsc::Receiver<CtrlRpcPacket>,
    args: CloseChannelArgs,
) -> Result<ResponseStatus> {
    let ch_id = ChId(args.ch_id);

    send_legacy_rpc_request(
        ctrl_tx,
        C2ARequest {
            c2a_req_args: Some(C2a_req_args::CloseChannel(args)),
            ..Default::default()
        },
        "send close ch failed",
    )
    .await?;

    let packet = recv_ctrl_rpc_packet(
        rpc_rx,
        &format!("recv close ch response failed {:?}", ch_id),
    )
    .await?;
    decode_response_status(packet, "parse close ch response failed")
}

async fn entity_ping(
    ctrl_tx: &ChSender,
    rpc_rx: &mut mpsc::Receiver<CtrlRpcPacket>,
    args: Ping,
) -> Result<Pong> {
    send_legacy_rpc_request(
        ctrl_tx,
        C2ARequest {
            c2a_req_args: Some(C2a_req_args::Ping(args)),
            ..Default::default()
        },
        "send ping failed",
    )
    .await?;

    let packet = recv_ctrl_rpc_packet(rpc_rx, "recv ping failed").await?;
    decode_pong(packet, "parse pong failed")
}

async fn entity_kick_down(
    ctrl_tx: &ChSender,
    rpc_rx: &mut mpsc::Receiver<CtrlRpcPacket>,
    args: KickDownArgs,
) -> Result<ResponseStatus> {
    send_legacy_rpc_request(
        ctrl_tx,
        C2ARequest {
            c2a_req_args: Some(C2a_req_args::KickDown(args)),
            ..Default::default()
        },
        "send kick down failed",
    )
    .await?;

    let packet = recv_ctrl_rpc_packet(rpc_rx, "recv kick down failed").await?;
    decode_response_status(packet, "parse kick down response failed")
}

async fn entity_open_p2p(
    ctrl_tx: &ChSender,
    rpc_rx: &mut mpsc::Receiver<CtrlRpcPacket>,
    args: P2PArgs,
) -> Result<OpenP2PResponse> {
    send_legacy_rpc_request(
        ctrl_tx,
        C2ARequest {
            c2a_req_args: Some(C2a_req_args::OpenP2p(args)),
            ..Default::default()
        },
        "send open p2p failed",
    )
    .await?;

    let packet = recv_ctrl_rpc_packet(rpc_rx, "recv open p2p response failed").await?;
    decode_open_p2p_response(packet, "parse open p2p response failed")
}

async fn entity_exec_agent_script(
    ctrl_tx: &ChSender,
    rpc_rx: &mut mpsc::Receiver<CtrlRpcPacket>,
    args: ExecAgentScriptArgs,
) -> Result<ExecAgentScriptResult> {
    send_legacy_rpc_request(
        ctrl_tx,
        C2ARequest {
            c2a_req_args: Some(C2a_req_args::ExecAgentScript(args)),
            ..Default::default()
        },
        "send exec agent script failed",
    )
    .await?;

    let packet = recv_ctrl_rpc_packet(rpc_rx, "recv exec agent script response failed").await?;
    decode_exec_agent_script_response(packet, "parse exec agent script response failed")
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

        let r = entity_close_channel(
            &self.ctrl_tx,
            &mut self.rpc_rx,
            CloseChannelArgs {
                ch_id: req.0 .0,
                ..Default::default()
            },
        )
        .await;

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

        let r = entity_open_shell(&self.ctrl_tx, &mut self.rpc_rx, req.1).await;
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

        let r = entity_open_socks(&self.ctrl_tx, &mut self.rpc_rx, req.1).await;
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
        let r = entity_kick_down(&self.ctrl_tx, &mut self.rpc_rx, req.0).await;
        match r {
            Ok(_v) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[async_trait::async_trait]
impl<H: SwitchHanlder> AsyncHandler<OpOpenP2P> for Entity<H> {
    type Response = OpOpenP2PResult;

    async fn handle(&mut self, req: OpOpenP2P) -> Self::Response {
        let r = entity_open_p2p(&self.ctrl_tx, &mut self.rpc_rx, req.0).await;
        match r {
            Ok(args) => Ok(args),
            Err(e) => Err(e),
        }
    }
}

#[async_trait::async_trait]
impl<H: SwitchHanlder> AsyncHandler<OpExecAgentScript> for Entity<H> {
    type Response = OpExecAgentScriptResult;

    async fn handle(&mut self, req: OpExecAgentScript) -> Self::Response {
        entity_exec_agent_script(&self.ctrl_tx, &mut self.rpc_rx, req.0).await
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
    ctrl_tx: ChSender,
    rpc_rx: mpsc::Receiver<CtrlRpcPacket>,
    event_rx: mpsc::Receiver<CtrlEvent>,
    reader_task: JoinHandle<()>,
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
    HardNatControl(HardNatControlEnvelope),
    Ping,
}

async fn wait_next<H: SwitchHanlder>(entity: &mut Entity<H>) -> Next {
    let duration = Duration::from_secs(10);
    tokio::select! {
        _r = entity.switch_watch.watch() => Next::SwitchGone,
        event = entity.event_rx.recv() => match event {
            Some(CtrlEvent::CtrlChBroken) | None => Next::CtrlChBroken,
            Some(CtrlEvent::HardNatControl(env)) => Next::HardNatControl(env),
        },
        _r = tokio::time::sleep(duration) => Next::Ping,
    }
}

async fn handle_next<H: SwitchHanlder>(entity: &mut Entity<H>, next: Next) -> Result<Action> {
    match next {
        Next::SwitchGone => {
            tracing::debug!("switch has gone");
        }
        Next::CtrlChBroken => {
            tracing::debug!("ctrl channel broken");
        }
        Next::HardNatControl(env) => {
            tracing::debug!("recv hardnat control before runtime hooked: {:?}", env);
            return Ok(Action::None);
        }
        Next::Ping => {
            let timeout = Duration::from_millis(90 * 1000);
            let r = tokio::time::timeout(
                timeout,
                entity_ping(
                    &entity.ctrl_tx,
                    &mut entity.rpc_rx,
                    Ping {
                        timestamp: Local::now().timestamp_millis(),
                        ..Default::default()
                    },
                ),
            )
            .await;

            match r {
                Ok(r) => {
                    let pong = r?;
                    let elapsed = Local::now().timestamp_millis() - pong.timestamp;
                    tracing::debug!("ping/pong latency {elapsed} ms\r");
                }
                Err(_elapsed) => {
                    tracing::warn!("ping/pong timeout {timeout:?}, shutdown switch\r");
                    entity.switch.shutdown().await
                }
            }

            // let pong = c2a_ping(&mut entity.pair, Ping {
            //     timestamp: Local::now().timestamp_millis(),
            //     ..Default::default()
            // }).await?;
            // let elapsed = Local::now().timestamp_millis() - pong.timestamp;
            // tracing::debug!("ping/pong elapsed {elapsed} ms\r");

            return Ok(Action::None);
        }
    }
    Ok(Action::Finished)
}

impl<H: SwitchHanlder> ActorEntity for Entity<H> {
    type Next = Next;

    type Msg = ();

    type Result = ();

    fn into_result(self, _r: Result<()>) -> Self::Result {
        self.reader_task.abort();
        ()
    }
}

pub async fn c2a_open_shell(pair: &mut ChPair, args: OpenShellArgs) -> Result<ChId> {
    let data = C2ARequest {
        c2a_req_args: Some(C2a_req_args::OpenSell(args)),
        ..Default::default()
    }
    .write_to_bytes()?;

    pair.tx
        .send_data(data.into())
        .await
        .map_err(|_e| anyhow!("send open shell failed"))?;

    let packet = pair
        .rx
        .recv_packet()
        .await
        .with_context(|| "recv open shell response failed")?;

    // let rsp = C2AResponse::parse_from_bytes(&data).with_context(||"parse open shell response failed")?;
    let rsp = OpenChannelResponse::parse_from_bytes(&packet.payload)
        .with_context(|| "parse open shell response failed")?
        .open_ch_rsp
        .with_context(|| "has no response")?;

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
    }
    .write_to_bytes()?;

    pair.tx
        .send_data(data.into())
        .await
        .map_err(|_e| anyhow!("send open socks failed"))?;

    let packet = pair
        .rx
        .recv_packet()
        .await
        .with_context(|| "recv open socks response failed")?;

    // let rsp = C2AResponse::parse_from_bytes(&data).with_context(||"parse open socks response failed")?;
    let rsp = OpenChannelResponse::parse_from_bytes(&packet.payload)
        .with_context(|| "parse open socks response failed")?
        .open_ch_rsp
        .with_context(|| "has no response")?;

    let opened_ch_id = match rsp {
        Open_ch_rsp::ChId(v) => ChId(v),
        Open_ch_rsp::Status(status) => bail!("open socks response status {status}"),
        // _ => bail!("unknown"),
    };

    // let pair = self.invoker.add_channel(shell_ch_id).await?;
    Ok(opened_ch_id)
}

pub async fn c2a_close_channel(
    pair: &mut ChPair,
    args: CloseChannelArgs,
) -> Result<ResponseStatus> {
    let ch_id = ChId(args.ch_id);

    let data = C2ARequest {
        c2a_req_args: Some(C2a_req_args::CloseChannel(args)),
        ..Default::default()
    }
    .write_to_bytes()?;

    pair.tx
        .send_data(data.into())
        .await
        .map_err(|_e| anyhow!("send close ch failed"))?;

    let packet = pair
        .rx
        .recv_packet()
        .await
        .with_context(|| format!("recv close ch response failed {:?}", ch_id))?;

    // let rsp = C2AResponse::parse_from_bytes(&data).with_context(||"parse close ch response failed")?;
    let status = ResponseStatus::parse_from_bytes(&packet.payload)
        .with_context(|| "parse close ch response failed")?;

    Ok(status)
}

pub async fn c2a_ping(pair: &mut ChPair, args: Ping) -> Result<Pong> {
    let data = C2ARequest {
        c2a_req_args: Some(C2a_req_args::Ping(args)),
        ..Default::default()
    }
    .write_to_bytes()?;

    pair.tx
        .send_data(data.into())
        .await
        .map_err(|_e| anyhow!("send close ch failed"))?;

    let packet = pair
        .rx
        .recv_packet()
        .await
        .with_context(|| "recv ping failed")?;

    let pong = Pong::parse_from_bytes(&packet.payload).with_context(|| "parse pong failed")?;

    Ok(pong)
}

pub async fn c2a_kick_down(pair: &mut ChPair, args: KickDownArgs) -> Result<ResponseStatus> {
    let data = C2ARequest {
        c2a_req_args: Some(C2a_req_args::KickDown(args)),
        ..Default::default()
    }
    .write_to_bytes()?;

    pair.tx
        .send_data(data.into())
        .await
        .map_err(|_e| anyhow!("send close ch failed"))?;

    let packet = pair
        .rx
        .recv_packet()
        .await
        .with_context(|| "recv ping failed")?;

    let status = ResponseStatus::parse_from_bytes(&packet.payload)
        .with_context(|| "parse kick down response failed")?;

    Ok(status)
}

pub async fn c2a_open_p2p(pair: &mut ChPair, args: P2PArgs) -> Result<OpenP2PResponse> {
    let data = C2ARequest {
        c2a_req_args: Some(C2a_req_args::OpenP2p(args)),
        ..Default::default()
    }
    .write_to_bytes()?;

    pair.tx
        .send_data(data.into())
        .await
        .map_err(|_e| anyhow!("send close ch failed"))?;

    let packet = pair
        .rx
        .recv_packet()
        .await
        .with_context(|| "recv ping failed")?;

    let rsp = OpenP2PResponse::parse_from_bytes(&packet.payload)
        .with_context(|| "parse open p2p response failed")?;

    Ok(rsp)
}

pub async fn c2a_exec_agent_script(
    pair: &mut ChPair,
    args: ExecAgentScriptArgs,
) -> Result<ExecAgentScriptResult> {
    let data = C2ARequest {
        c2a_req_args: Some(C2a_req_args::ExecAgentScript(args)),
        ..Default::default()
    }
    .write_to_bytes()?;

    pair.tx
        .send_data(data.into())
        .await
        .map_err(|_e| anyhow!("send exec agent script failed"))?;

    let packet = pair
        .rx
        .recv_packet()
        .await
        .with_context(|| "recv exec agent script response failed")?;

    let rsp = ExecAgentScriptResult::parse_from_bytes(&packet.payload)
        .with_context(|| "parse exec agent script response failed")?;
    Ok(rsp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        actor_service::{
            handle_first_none, handle_msg_none, handle_next_none, start_actor, wait_next_none,
            ActorEntity, AsyncHandler,
        },
        channel::{CHANNEL_SIZE, ChReceiver, ChSender, ChTx},
        huid::gen_huid::gen_huid,
        proto::{
            ctrl_channel_packet, hard_nat_control_envelope, open_p2presponse, CtrlChannelPacket,
            HardNatControlEnvelope, HardNatLeaseKeepAlive,
        },
        switch::{
            entity_watch::{CtrlGuard, OpWatch, WatchResult},
            invoker_switch::{
                AddChannelResult, ReqAddChannel, ReqGetMuxTx, ReqGetMuxTxResult, ReqRemoveChannel,
                RemoveChannelResult,
            },
        },
    };
    use tokio::{sync::mpsc, time::timeout};

    struct TestSwitchEntity {
        guard: CtrlGuard,
        mux_tx: ChTx,
    }

    impl ActorEntity for TestSwitchEntity {
        type Next = ();
        type Msg = ();
        type Result = Result<()>;

        fn into_result(self, result: Result<()>) -> Self::Result {
            result
        }
    }

    #[async_trait::async_trait]
    impl AsyncHandler<ReqAddChannel> for TestSwitchEntity {
        type Response = AddChannelResult;

        async fn handle(&mut self, req: ReqAddChannel) -> Self::Response {
            Ok(req.1)
        }
    }

    #[async_trait::async_trait]
    impl AsyncHandler<ReqRemoveChannel> for TestSwitchEntity {
        type Response = RemoveChannelResult;

        async fn handle(&mut self, _req: ReqRemoveChannel) -> Self::Response {
            Ok(true)
        }
    }

    #[async_trait::async_trait]
    impl AsyncHandler<ReqGetMuxTx> for TestSwitchEntity {
        type Response = ReqGetMuxTxResult;

        async fn handle(&mut self, _req: ReqGetMuxTx) -> Self::Response {
            Ok(self.mux_tx.clone())
        }
    }

    #[async_trait::async_trait]
    impl AsyncHandler<OpWatch> for TestSwitchEntity {
        type Response = WatchResult;

        async fn handle(&mut self, _req: OpWatch) -> Self::Response {
            Ok(self.guard.watch())
        }
    }

    impl SwitchHanlder for TestSwitchEntity {}

    fn spawn_test_switch() -> crate::actor_service::ActorHandle<TestSwitchEntity> {
        let (mux_tx, _mux_rx) = mpsc::channel(CHANNEL_SIZE);
        start_actor(
            "test-ctrl-client-switch".into(),
            TestSwitchEntity {
                guard: CtrlGuard::new(),
                mux_tx,
            },
            handle_first_none,
            wait_next_none,
            handle_next_none,
            handle_msg_none,
        )
    }

    fn make_duplex_ctrl_pair(ch_id: ChId) -> (ChPair, ChPair) {
        let (client_to_remote_tx, client_to_remote_rx) = mpsc::channel(CHANNEL_SIZE);
        let (remote_to_client_tx, remote_to_client_rx) = mpsc::channel(CHANNEL_SIZE);

        let client = ChPair {
            tx: ChSender::new(ch_id, client_to_remote_tx),
            rx: ChReceiver::new(remote_to_client_rx),
        };
        let remote = ChPair {
            tx: ChSender::new(ch_id, remote_to_client_tx),
            rx: ChReceiver::new(client_to_remote_rx),
        };
        (client, remote)
    }

    #[tokio::test]
    async fn ctrl_client_ignores_unsolicited_hardnat_control_before_open_p2p() {
        let switch_handle = spawn_test_switch();
        let switch = SwitchInvoker::new(switch_handle.invoker().clone());
        let (client_pair, mut remote_pair) = make_duplex_ctrl_pair(ChId(7));
        let mut ctrl = make_ctrl_client(gen_huid(), client_pair, switch, false)
            .await
            .unwrap();
        let invoker = ctrl.clone_invoker();

        let remote = tokio::spawn(async move {
            let control = CtrlChannelPacket {
                body: Some(ctrl_channel_packet::Body::HardNatControl(
                    HardNatControlEnvelope {
                        session_id: 9,
                        seq: 1,
                        role_from: 2,
                        msg: Some(hard_nat_control_envelope::Msg::LeaseKeepAlive(
                            HardNatLeaseKeepAlive {
                                lease_timeout_ms: 5000,
                                ..Default::default()
                            },
                        )),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            };
            remote_pair
                .tx
                .send_data(control.write_to_bytes().unwrap().into())
                .await
                .unwrap();

            tokio::time::sleep(Duration::from_millis(50)).await;

            let packet = timeout(Duration::from_secs(1), remote_pair.rx.recv_packet())
                .await
                .unwrap()
                .unwrap();
            let req = C2ARequest::parse_from_bytes(&packet.payload).unwrap();
            assert!(matches!(req.c2a_req_args, Some(C2a_req_args::OpenP2p(_))));

            let rsp = OpenP2PResponse {
                open_p2p_rsp: Some(open_p2presponse::Open_p2p_rsp::Args(P2PArgs::default())),
                ..Default::default()
            };
            remote_pair
                .tx
                .send_data(rsp.write_to_bytes().unwrap().into())
                .await
                .unwrap();
        });

        tokio::time::sleep(Duration::from_millis(100)).await;

        let rsp = timeout(Duration::from_secs(1), invoker.open_p2p(P2PArgs::default()))
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            rsp.open_p2p_rsp,
            Some(open_p2presponse::Open_p2p_rsp::Args(_))
        ));

        remote.await.unwrap();
        ctrl.shutdown_and_waitfor().await.unwrap();
    }
}
