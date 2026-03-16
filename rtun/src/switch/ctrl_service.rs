use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

use crate::{
    async_rt::spawn_with_name,
    channel::{ChId, ChPair, ChSender},
    huid::HUId,
    proto::{
        c2arequest::C2a_req_args, make_open_p2p_response_error, make_open_shell_response_error,
        make_open_shell_response_ok, make_response_status_ok, ctrl_channel_packet,
        ctrl_rpc_response, C2ARequest, CtrlChannelPacket, CtrlRpcResponse,
        ExecAgentScriptResult, HardNatControlEnvelope, KickDownArgs, Pong,
    },
};
use bytes::Bytes;
use protobuf::Message;
use tokio::{sync::mpsc, task::JoinHandle};

use super::{
    invoker_ctrl::{CtrlHandler, CtrlInvoker},
    invoker_switch::{SwitchHanlder, SwitchInvoker},
    next_ch_id::NextChId,
};

fn hard_nat_control_debug_label(env: &HardNatControlEnvelope) -> String {
    let msg = match env.msg.as_ref() {
        Some(crate::proto::hard_nat_control_envelope::Msg::StartBatch(msg)) => {
            format!("StartBatch(batch_id={}, ports={})", msg.batch_id, msg.ports.len())
        }
        Some(crate::proto::hard_nat_control_envelope::Msg::NextBatch(msg)) => {
            format!(
                "NextBatch(next_batch_id={}, nat3_addr_index={}, nat4_ip_index={}, ports={})",
                msg.next_batch_id,
                msg.nat3_addr_index,
                msg.nat4_ip_index,
                msg.ports.len()
            )
        }
        Some(crate::proto::hard_nat_control_envelope::Msg::LeaseKeepAlive(msg)) => {
            format!("LeaseKeepAlive(timeout_ms={})", msg.lease_timeout_ms)
        }
        Some(crate::proto::hard_nat_control_envelope::Msg::AdvanceNat4Ip(msg)) => {
            format!(
                "AdvanceNat4Ip(batch_id={}, next_index={})",
                msg.batch_id, msg.next_nat4_ip_index
            )
        }
        Some(crate::proto::hard_nat_control_envelope::Msg::AdvanceNat3Addr(msg)) => {
            format!(
                "AdvanceNat3Addr(batch_id={}, next_index={})",
                msg.batch_id, msg.next_nat3_addr_index
            )
        }
        Some(crate::proto::hard_nat_control_envelope::Msg::Connected(msg)) => format!(
            "Connected(nat3_addr={}, nat4_ip={}, port={}, restore_ttl={})",
            msg.selected_nat3_addr, msg.selected_nat4_ip, msg.selected_port, msg.restore_ttl
        ),
        Some(crate::proto::hard_nat_control_envelope::Msg::Abort(msg)) => {
            format!("Abort(reason={})", msg.reason)
        }
        Some(crate::proto::hard_nat_control_envelope::Msg::Ack(msg)) => {
            format!("Ack(acked_seq={}, state={})", msg.acked_seq, msg.state)
        }
        None => "None".to_string(),
    };
    format!(
        "session_id={}, seq={}, role_from={}, msg={}",
        env.session_id, env.seq, env.role_from, msg
    )
}

pub fn spawn_ctrl_service<H1: CtrlHandler, H2: SwitchHanlder>(
    uid: HUId,
    agent: CtrlInvoker<H1>,
    switch: SwitchInvoker<H2>,
    mut chpair: ChPair,
    mut hard_nat_rx: mpsc::Receiver<HardNatControlEnvelope>,
) -> JoinHandle<Result<ExitReason>> {
    let task = spawn_with_name(format!("ctrl-service-{}", uid), async move {
        let r = ctrl_loop_full(&agent, &switch, &mut chpair, &mut hard_nat_rx).await;
        tracing::debug!("ctrl_service loop finished with [{r:?}]");
        switch.shutdown().await;
        r
    });
    task
}

#[derive(Debug)]
pub enum ExitReason {
    KickDown(KickDownArgs),
}

#[derive(Debug)]
enum DecodedCtrlServicePacket {
    LegacyRpc(C2ARequest),
    WrappedRpc(C2ARequest),
    HardNatControl(HardNatControlEnvelope),
}

fn decode_ctrl_service_packet(payload: &[u8]) -> Result<DecodedCtrlServicePacket> {
    let wrapped = CtrlChannelPacket::parse_from_bytes(payload)?;
    match wrapped.body {
        Some(ctrl_channel_packet::Body::RpcRequest(req)) => Ok(DecodedCtrlServicePacket::WrappedRpc(req)),
        Some(ctrl_channel_packet::Body::HardNatControl(env)) => {
            Ok(DecodedCtrlServicePacket::HardNatControl(env))
        }
        Some(ctrl_channel_packet::Body::RpcResponse(_)) => {
            bail!("unexpected rpc response on ctrl service")
        }
        None => {
            let legacy = C2ARequest::parse_from_bytes(payload)?;
            Ok(DecodedCtrlServicePacket::LegacyRpc(legacy))
        }
    }
}

fn encode_ctrl_rpc_response_packet(rsp: CtrlRpcResponse, wrapped: bool) -> Result<Bytes> {
    if wrapped {
        return Ok(CtrlChannelPacket {
            body: Some(ctrl_channel_packet::Body::RpcResponse(rsp)),
            ..Default::default()
        }
        .write_to_bytes()?
        .into());
    }

    let bytes = match rsp.body.with_context(|| "no ctrl rpc response body")? {
        ctrl_rpc_response::Body::OpenChannel(msg) => msg.write_to_bytes()?,
        ctrl_rpc_response::Body::Status(msg) => msg.write_to_bytes()?,
        ctrl_rpc_response::Body::Pong(msg) => msg.write_to_bytes()?,
        ctrl_rpc_response::Body::OpenP2p(msg) => msg.write_to_bytes()?,
        ctrl_rpc_response::Body::ExecAgentScript(msg) => msg.write_to_bytes()?,
    };
    Ok(bytes.into())
}

async fn send_ctrl_rpc_response(
    ctrl_tx: &ChSender,
    rsp: CtrlRpcResponse,
    wrapped: bool,
) -> Result<()> {
    let data = encode_ctrl_rpc_response_packet(rsp, wrapped)?;
    ctrl_tx
        .send_data(data)
        .await
        .map_err(|_x| anyhow!("send data fail"))?;
    Ok(())
}

async fn send_hard_nat_control(ctrl_tx: &ChSender, env: HardNatControlEnvelope) -> Result<()> {
    let data = CtrlChannelPacket {
        body: Some(ctrl_channel_packet::Body::HardNatControl(env)),
        ..Default::default()
    }
    .write_to_bytes()?;
    ctrl_tx
        .send_data(data.into())
        .await
        .map_err(|_x| anyhow!("send hardnat control fail"))?;
    Ok(())
}

async fn ctrl_loop_full<H1: CtrlHandler, H2: SwitchHanlder>(
    agent: &CtrlInvoker<H1>,
    switch: &SwitchInvoker<H2>,
    ctrl_pair: &mut ChPair,
    hard_nat_rx: &mut mpsc::Receiver<HardNatControlEnvelope>,
) -> Result<ExitReason> {
    let ctrl_tx = &mut ctrl_pair.tx;
    let ctrl_rx = &mut ctrl_pair.rx;

    let mux_tx = switch.get_mux_tx().await?;
    let mut agent_watch = agent.watch().await?;

    let mut next_ch_id = NextChId::default();
    let alive_timeout = Duration::from_secs(60);

    loop {
        // let packet = ctrl_rx.recv_packet().await?;

        let packet = tokio::select! {
            _r = agent_watch.watch() => bail!("agent has gone"),
            outbound = hard_nat_rx.recv() => {
                let env = match outbound {
                    Some(env) => env,
                    None => {
                        tracing::debug!("ctrl_service: hard-nat outbound channel closed");
                        bail!("hardnat outbound closed");
                    }
                };
                tracing::debug!(
                    "ctrl_service: sending outbound hard-nat control {}",
                    hard_nat_control_debug_label(&env)
                );
                send_hard_nat_control(ctrl_tx, env).await?;
                continue;
            }
            r = ctrl_rx.recv_packet() => match r {
                Ok(packet) => packet,
                Err(e) => {
                    tracing::debug!("ctrl_service: recv ctrl packet failed: {e:#}");
                    return Err(e.into());
                }
            },
            _r = tokio::time::sleep(alive_timeout) => {
                bail!("wait for ping timeout")
            }
        };

        let (req, wrapped) = match decode_ctrl_service_packet(&packet.payload)? {
            DecodedCtrlServicePacket::LegacyRpc(req) => (req, false),
            DecodedCtrlServicePacket::WrappedRpc(req) => (req, true),
            DecodedCtrlServicePacket::HardNatControl(env) => {
                tracing::debug!(
                    "ctrl_service: received inbound hard-nat control {}",
                    hard_nat_control_debug_label(&env)
                );
                agent.recv_hard_nat_control(env).await?;
                continue;
            }
        };

        let cmd = req.c2a_req_args.with_context(|| "no c2a_req_args")?;
        match cmd {
            C2a_req_args::OpenSell(args) => {
                let ch_id = args
                    .ch_id
                    .map(|x| ChId(x))
                    .unwrap_or_else(|| next_ch_id.next_ch_id());

                let r = {
                    let mux_tx = ChSender::new(ch_id, mux_tx.clone());
                    agent.open_shell(mux_tx, args).await
                };

                let (rsp, ch_tx) = match r {
                    Ok(v) => (make_open_shell_response_ok(ch_id.0), Some(v)),
                    Err(e) => (make_open_shell_response_error(e), None),
                };

                send_ctrl_rpc_response(
                    ctrl_tx,
                    CtrlRpcResponse {
                        body: Some(ctrl_rpc_response::Body::OpenChannel(rsp)),
                        ..Default::default()
                    },
                    wrapped,
                )
                .await?;

                if let Some(ch_tx) = ch_tx {
                    switch.add_channel(ch_id, ch_tx).await?;
                }
            }

            C2a_req_args::OpenSocks(args) => {
                let ch_id = args
                    .ch_id
                    .map(|x| ChId(x))
                    .unwrap_or_else(|| next_ch_id.next_ch_id());

                let r = {
                    let mux_tx = ChSender::new(ch_id, mux_tx.clone());
                    agent.open_socks(mux_tx, args).await
                };

                let (rsp, ch_tx) = match r {
                    Ok(v) => (make_open_shell_response_ok(ch_id.0), Some(v)),
                    Err(e) => (make_open_shell_response_error(e), None),
                };

                send_ctrl_rpc_response(
                    ctrl_tx,
                    CtrlRpcResponse {
                        body: Some(ctrl_rpc_response::Body::OpenChannel(rsp)),
                        ..Default::default()
                    },
                    wrapped,
                )
                .await?;

                if let Some(ch_tx) = ch_tx {
                    switch.add_channel(ch_id, ch_tx).await?;
                }
            }

            C2a_req_args::CloseChannel(args) => {
                // tracing::debug!("recv closing ch req {}", args);
                let _r = switch.remove_channel(ChId(args.ch_id)).await?;

                send_ctrl_rpc_response(
                    ctrl_tx,
                    CtrlRpcResponse {
                        body: Some(ctrl_rpc_response::Body::Status(make_response_status_ok())),
                        ..Default::default()
                    },
                    wrapped,
                )
                .await?;
                // tracing::debug!("send closing ch ok");
            }

            C2a_req_args::Ping(args) => {
                let rsp = Pong {
                    timestamp: args.timestamp,
                    ..Default::default()
                };

                send_ctrl_rpc_response(
                    ctrl_tx,
                    CtrlRpcResponse {
                        body: Some(ctrl_rpc_response::Body::Pong(rsp)),
                        ..Default::default()
                    },
                    wrapped,
                )
                .await?;
            }

            C2a_req_args::KickDown(args) => {
                send_ctrl_rpc_response(
                    ctrl_tx,
                    CtrlRpcResponse {
                        body: Some(ctrl_rpc_response::Body::Status(make_response_status_ok())),
                        ..Default::default()
                    },
                    wrapped,
                )
                .await?;

                tracing::warn!("recv kick down {args}");
                return Ok(ExitReason::KickDown(args));
            }

            C2a_req_args::OpenP2p(args) => {
                tracing::debug!("ctrl_service: received open_p2p request");
                let r = agent.open_p2p(args).await;

                // let r = handle_open_p2p(args).await;
                let rsp = match r {
                    Ok(rsp) => rsp,
                    Err(e) => make_open_p2p_response_error(e),
                };
                tracing::debug!("ctrl_service: agent open_p2p finished, sending response");

                send_ctrl_rpc_response(
                    ctrl_tx,
                    CtrlRpcResponse {
                        body: Some(ctrl_rpc_response::Body::OpenP2p(rsp)),
                        ..Default::default()
                    },
                    wrapped,
                )
                .await?;
                tracing::debug!("ctrl_service: open_p2p response sent");
            }

            C2a_req_args::ExecAgentScript(args) => {
                let rsp = match agent.exec_agent_script(args).await {
                    Ok(v) => v,
                    Err(e) => ExecAgentScriptResult {
                        status_code: -1,
                        reason: format!("{e:#}").into(),
                        exit_code: -1,
                        ..Default::default()
                    },
                };
                send_ctrl_rpc_response(
                    ctrl_tx,
                    CtrlRpcResponse {
                        body: Some(ctrl_rpc_response::Body::ExecAgentScript(rsp)),
                        ..Default::default()
                    },
                    wrapped,
                )
                .await?;
            }
        }
    }
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
        switch::{
            entity_watch::{CtrlGuard, OpWatch, WatchResult},
            invoker_ctrl::{
                CloseChannelResult, OpCloseChannel, OpExecAgentScript, OpExecAgentScriptResult,
                OpKickDown, OpKickDownResult, OpOpenP2P, OpOpenP2PResult, OpOpenShell,
                OpOpenShellResult, OpOpenSocks, OpOpenSocksResult, OpRecvHardNatControl,
                OpRecvHardNatControlResult, OpSendHardNatControl, OpSendHardNatControlResult,
                OpSubscribeHardNatControl, OpSubscribeHardNatControlResult,
            },
            invoker_switch::{AddChannelResult, ReqAddChannel, ReqGetMuxTx, ReqGetMuxTxResult, ReqRemoveChannel, RemoveChannelResult},
        },
    };
    use crate::proto::{
        c2arequest::C2a_req_args, ctrl_channel_packet, ctrl_rpc_response,
        hard_nat_control_envelope, CtrlChannelPacket, CtrlRpcResponse, OpenP2PResponse, P2PArgs,
        ResponseStatus,
    };
    use protobuf::Message;
    use tokio::{sync::{broadcast, mpsc}, time::timeout};

    struct TestCtrlEntity {
        guard: CtrlGuard,
        hard_nat_tx: broadcast::Sender<HardNatControlEnvelope>,
    }

    impl ActorEntity for TestCtrlEntity {
        type Next = ();
        type Msg = ();
        type Result = Result<()>;

        fn into_result(self, result: Result<()>) -> Self::Result {
            result
        }
    }

    #[async_trait::async_trait]
    impl AsyncHandler<OpWatch> for TestCtrlEntity {
        type Response = WatchResult;

        async fn handle(&mut self, _req: OpWatch) -> Self::Response {
            Ok(self.guard.watch())
        }
    }

    #[async_trait::async_trait]
    impl AsyncHandler<OpCloseChannel> for TestCtrlEntity {
        type Response = CloseChannelResult;

        async fn handle(&mut self, _req: OpCloseChannel) -> Self::Response {
            anyhow::bail!("unexpected close_channel")
        }
    }

    #[async_trait::async_trait]
    impl AsyncHandler<OpOpenShell> for TestCtrlEntity {
        type Response = OpOpenShellResult;

        async fn handle(&mut self, _req: OpOpenShell) -> Self::Response {
            anyhow::bail!("unexpected open_shell")
        }
    }

    #[async_trait::async_trait]
    impl AsyncHandler<OpOpenSocks> for TestCtrlEntity {
        type Response = OpOpenSocksResult;

        async fn handle(&mut self, _req: OpOpenSocks) -> Self::Response {
            anyhow::bail!("unexpected open_socks")
        }
    }

    #[async_trait::async_trait]
    impl AsyncHandler<OpKickDown> for TestCtrlEntity {
        type Response = OpKickDownResult;

        async fn handle(&mut self, _req: OpKickDown) -> Self::Response {
            anyhow::bail!("unexpected kick_down")
        }
    }

    #[async_trait::async_trait]
    impl AsyncHandler<OpOpenP2P> for TestCtrlEntity {
        type Response = OpOpenP2PResult;

        async fn handle(&mut self, _req: OpOpenP2P) -> Self::Response {
            anyhow::bail!("unexpected open_p2p")
        }
    }

    #[async_trait::async_trait]
    impl AsyncHandler<OpExecAgentScript> for TestCtrlEntity {
        type Response = OpExecAgentScriptResult;

        async fn handle(&mut self, _req: OpExecAgentScript) -> Self::Response {
            anyhow::bail!("unexpected exec_agent_script")
        }
    }

    #[async_trait::async_trait]
    impl AsyncHandler<OpSendHardNatControl> for TestCtrlEntity {
        type Response = OpSendHardNatControlResult;

        async fn handle(&mut self, _req: OpSendHardNatControl) -> Self::Response {
            anyhow::bail!("unexpected send_hardnat_control")
        }
    }

    #[async_trait::async_trait]
    impl AsyncHandler<OpSubscribeHardNatControl> for TestCtrlEntity {
        type Response = OpSubscribeHardNatControlResult;

        async fn handle(&mut self, _req: OpSubscribeHardNatControl) -> Self::Response {
            Ok(self.hard_nat_tx.subscribe())
        }
    }

    #[async_trait::async_trait]
    impl AsyncHandler<OpRecvHardNatControl> for TestCtrlEntity {
        type Response = OpRecvHardNatControlResult;

        async fn handle(&mut self, req: OpRecvHardNatControl) -> Self::Response {
            let _r = self.hard_nat_tx.send(req.0);
            Ok(())
        }
    }

    impl CtrlHandler for TestCtrlEntity {}

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

    fn spawn_test_ctrl() -> crate::actor_service::ActorHandle<TestCtrlEntity> {
        let (hard_nat_tx, _) = broadcast::channel(4);
        start_actor(
            "test-ctrl-service-agent".into(),
            TestCtrlEntity {
                guard: CtrlGuard::new(),
                hard_nat_tx,
            },
            handle_first_none,
            wait_next_none,
            handle_next_none,
            handle_msg_none,
        )
    }

    fn spawn_test_switch() -> crate::actor_service::ActorHandle<TestSwitchEntity> {
        let (mux_tx, _mux_rx) = mpsc::channel(CHANNEL_SIZE);
        start_actor(
            "test-ctrl-service-switch".into(),
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
        let (client_to_service_tx, client_to_service_rx) = mpsc::channel(CHANNEL_SIZE);
        let (service_to_client_tx, service_to_client_rx) = mpsc::channel(CHANNEL_SIZE);

        let service = ChPair {
            tx: ChSender::new(ch_id, service_to_client_tx),
            rx: ChReceiver::new(client_to_service_rx),
        };
        let client = ChPair {
            tx: ChSender::new(ch_id, client_to_service_tx),
            rx: ChReceiver::new(service_to_client_rx),
        };
        (service, client)
    }

    #[test]
    fn decode_ctrl_service_packet_accepts_legacy_and_wrapped_rpc_request() {
        let legacy_req = C2ARequest {
            c2a_req_args: Some(C2a_req_args::Ping(crate::proto::Ping {
                timestamp: 7,
                ..Default::default()
            })),
            ..Default::default()
        };
        let decoded = decode_ctrl_service_packet(&legacy_req.write_to_bytes().unwrap()).unwrap();
        assert!(matches!(decoded, DecodedCtrlServicePacket::LegacyRpc(_)));

        let wrapped_req = CtrlChannelPacket {
            body: Some(ctrl_channel_packet::Body::RpcRequest(legacy_req)),
            ..Default::default()
        };
        let decoded = decode_ctrl_service_packet(&wrapped_req.write_to_bytes().unwrap()).unwrap();
        assert!(matches!(decoded, DecodedCtrlServicePacket::WrappedRpc(_)));
    }

    #[test]
    fn encode_ctrl_rpc_response_packet_preserves_wrapped_mode() {
        let rsp = CtrlRpcResponse {
            body: Some(ctrl_rpc_response::Body::OpenP2p(OpenP2PResponse {
                open_p2p_rsp: Some(crate::proto::open_p2presponse::Open_p2p_rsp::Args(
                    P2PArgs::default(),
                )),
                ..Default::default()
            })),
            ..Default::default()
        };

        let wrapped = encode_ctrl_rpc_response_packet(rsp.clone(), true).unwrap();
        let wrapped = CtrlChannelPacket::parse_from_bytes(&wrapped).unwrap();
        assert!(matches!(
            wrapped.body,
            Some(ctrl_channel_packet::Body::RpcResponse(_))
        ));

        let raw = encode_ctrl_rpc_response_packet(
            CtrlRpcResponse {
                body: Some(ctrl_rpc_response::Body::Status(ResponseStatus {
                    code: 0,
                    reason: "ok".into(),
                    ..Default::default()
                })),
                ..Default::default()
            },
            false,
        )
        .unwrap();
        let raw = ResponseStatus::parse_from_bytes(&raw).unwrap();
        assert_eq!(raw.code, 0);
        assert_eq!(raw.reason, "ok".into());
    }

    #[tokio::test]
    async fn ctrl_loop_full_replies_raw_pong_to_legacy_ping() {
        let ctrl_handle = spawn_test_ctrl();
        let switch_handle = spawn_test_switch();
        let ctrl = CtrlInvoker::new(ctrl_handle.invoker().clone());
        let switch = SwitchInvoker::new(switch_handle.invoker().clone());
        let (mut service_pair, mut client_pair) = make_duplex_ctrl_pair(ChId(1));
        let (hard_nat_tx, mut hard_nat_rx) = mpsc::channel(1);

        let service = tokio::spawn(async move {
            ctrl_loop_full(&ctrl, &switch, &mut service_pair, &mut hard_nat_rx).await
        });

        let req = C2ARequest {
            c2a_req_args: Some(C2a_req_args::Ping(crate::proto::Ping {
                timestamp: 123,
                ..Default::default()
            })),
            ..Default::default()
        };
        client_pair
            .tx
            .send_data(req.write_to_bytes().unwrap().into())
            .await
            .unwrap();

        let rsp = timeout(Duration::from_secs(1), client_pair.rx.recv_packet())
            .await
            .unwrap()
            .unwrap();
        let pong = Pong::parse_from_bytes(&rsp.payload).unwrap();
        assert_eq!(pong.timestamp, 123);

        drop(client_pair.tx);
        drop(hard_nat_tx);
        assert!(service.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn ctrl_loop_full_replies_wrapped_pong_to_wrapped_ping() {
        let ctrl_handle = spawn_test_ctrl();
        let switch_handle = spawn_test_switch();
        let ctrl = CtrlInvoker::new(ctrl_handle.invoker().clone());
        let switch = SwitchInvoker::new(switch_handle.invoker().clone());
        let (mut service_pair, mut client_pair) = make_duplex_ctrl_pair(ChId(1));
        let (hard_nat_tx, mut hard_nat_rx) = mpsc::channel(1);

        let service = tokio::spawn(async move {
            ctrl_loop_full(&ctrl, &switch, &mut service_pair, &mut hard_nat_rx).await
        });

        let req = CtrlChannelPacket {
            body: Some(ctrl_channel_packet::Body::RpcRequest(C2ARequest {
                c2a_req_args: Some(C2a_req_args::Ping(crate::proto::Ping {
                    timestamp: 456,
                    ..Default::default()
                })),
                ..Default::default()
            })),
            ..Default::default()
        };
        client_pair
            .tx
            .send_data(req.write_to_bytes().unwrap().into())
            .await
            .unwrap();

        let rsp = timeout(Duration::from_secs(1), client_pair.rx.recv_packet())
            .await
            .unwrap()
            .unwrap();
        let rsp = CtrlChannelPacket::parse_from_bytes(&rsp.payload).unwrap();
        let pong = match rsp.body {
            Some(ctrl_channel_packet::Body::RpcResponse(CtrlRpcResponse {
                body: Some(ctrl_rpc_response::Body::Pong(pong)),
                ..
            })) => pong,
            other => panic!("unexpected wrapped response: {other:?}"),
        };
        assert_eq!(pong.timestamp, 456);

        drop(client_pair.tx);
        drop(hard_nat_tx);
        assert!(service.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn ctrl_loop_full_forwards_hardnat_control_to_agent_subscriber() {
        let ctrl_handle = spawn_test_ctrl();
        let switch_handle = spawn_test_switch();
        let ctrl = CtrlInvoker::new(ctrl_handle.invoker().clone());
        let mut hard_nat_rx_sub = ctrl.subscribe_hard_nat_control().await.unwrap();
        let switch = SwitchInvoker::new(switch_handle.invoker().clone());
        let (mut service_pair, client_pair) = make_duplex_ctrl_pair(ChId(1));
        let (hard_nat_tx, mut hard_nat_rx) = mpsc::channel(1);

        let service = tokio::spawn(async move {
            ctrl_loop_full(&ctrl, &switch, &mut service_pair, &mut hard_nat_rx).await
        });

        let control = CtrlChannelPacket {
            body: Some(ctrl_channel_packet::Body::HardNatControl(HardNatControlEnvelope {
                session_id: 42,
                seq: 7,
                role_from: 2,
                msg: Some(hard_nat_control_envelope::Msg::LeaseKeepAlive(
                    crate::proto::HardNatLeaseKeepAlive {
                        lease_timeout_ms: 7000,
                        ..Default::default()
                    },
                )),
                ..Default::default()
            })),
            ..Default::default()
        };
        client_pair
            .tx
            .send_data(control.write_to_bytes().unwrap().into())
            .await
            .unwrap();

        let got = timeout(Duration::from_secs(1), hard_nat_rx_sub.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.session_id, 42);

        drop(client_pair.tx);
        drop(hard_nat_tx);
        assert!(service.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn ctrl_loop_full_sends_outbound_hardnat_control_packets() {
        let ctrl_handle = spawn_test_ctrl();
        let switch_handle = spawn_test_switch();
        let ctrl = CtrlInvoker::new(ctrl_handle.invoker().clone());
        let switch = SwitchInvoker::new(switch_handle.invoker().clone());
        let (mut service_pair, mut client_pair) = make_duplex_ctrl_pair(ChId(1));
        let (hard_nat_tx, mut hard_nat_rx) = mpsc::channel(1);

        let service = tokio::spawn(async move {
            ctrl_loop_full(&ctrl, &switch, &mut service_pair, &mut hard_nat_rx).await
        });

        let env = HardNatControlEnvelope {
            session_id: 77,
            seq: 3,
            role_from: 1,
            msg: Some(hard_nat_control_envelope::Msg::LeaseKeepAlive(
                crate::proto::HardNatLeaseKeepAlive {
                    lease_timeout_ms: 9000,
                    ..Default::default()
                },
            )),
            ..Default::default()
        };
        hard_nat_tx.send(env.clone()).await.unwrap();

        let packet = timeout(Duration::from_secs(1), client_pair.rx.recv_packet())
            .await
            .unwrap()
            .unwrap();
        let packet = CtrlChannelPacket::parse_from_bytes(&packet.payload).unwrap();
        let got = match packet.body {
            Some(ctrl_channel_packet::Body::HardNatControl(env)) => env,
            other => panic!("unexpected packet: {other:?}"),
        };
        assert_eq!(got, env);

        drop(client_pair.tx);
        drop(hard_nat_tx);
        assert!(service.await.unwrap().is_err());
    }
}

// async fn handle_open_p2p(mut args: OpenP2PArgs) -> Result<(String, SocketAddr)> {
//     let (peer, nat) = PunchPeer::bind_and_detect("0.0.0.0:0").await?;

//     let local_ufrag = peer.local_ufrag().to_string();

//     let nat_type = nat.nat_type();
//     if nat_type != Some(NatType::Cone) {
//         tracing::warn!("nat type {nat_type:?}");
//     } else {
//         tracing::debug!("nat type {nat_type:?}");
//     }

//     let mapped = nat.into_mapped().with_context(||"empty mapped address")?;

//     let args = args.args.take().with_context(||"no p2p args")?;

//     let mut tun = launch_tun_peer(peer, args.ufrag.into(), args.addr.parse()?, false);
//     spawn_with_name("", async move {
//         tun.wait_for_completed().await
//     });

//     Ok((local_ufrag, mapped))

// }

// async fn ctrl_loop_pure_ch<H1: CtrlHandler, H2: SwitchHanlder>(
//     agent: &CtrlInvoker<H1>,
//     switch: &SwitchInvoker<H2>,
//     ctrl_pair: ChPair,
// ) -> Result<()> {

//     let tx = ctrl_pair.tx;
//     let mut rx = ctrl_pair.rx;
//     let mut next_ch_id = NextChId::default();

//     let mux_tx = switch.get_mux_tx().await?;

//     loop {
//         let r = rx.recv_data().await;
//         let packet = match r {
//             Some(v) => v,
//             None => break,
//         };

//         let req = OpenChannelRequest::parse_from_bytes(&packet.payload)?;
//         let ch_id = match req.open_ch_req {
//             Some(v) => {
//                 match v {
//                     Open_ch_req::ChId(v) => ChId(v),
//                 }
//             },
//             None => {
//                 next_ch_id.next_ch_id()
//             },
//         };

//         let r = {
//             let mux_tx = ChSender::new(ch_id, mux_tx.clone());
//             agent.open_channel(mux_tx).await
//         };

//         let (rsp, ch_tx) = match r {
//             Ok(v) => (make_open_channel_response_ok(ch_id.0), Some(v)),
//             Err(e) => (make_open_channel_response_error(e), None),
//         };

//         tx.send_data(rsp.write_to_bytes()?.into()).await
//         .map_err(|_x|anyhow!("send data fail"))?;

//         if let Some(ch_tx) = ch_tx {
//             switch.add_channel(ch_id, ch_tx).await?;
//         }

//     }
//     Ok(())
// }
