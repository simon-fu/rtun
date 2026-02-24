use std::{
    collections::HashMap,
    io::ErrorKind,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use bytes::{BufMut, Bytes, BytesMut};
use chrono::Local;
use protobuf::MessageField;
use tokio_util::compat::{FuturesAsyncReadCompatExt, FuturesAsyncWriteCompatExt};

use crate::{
    actor_service::{
        handle_first_none, handle_msg_none, handle_next_none, start_actor, wait_next_none,
        ActorEntity, ActorHandle, AsyncHandler,
    },
    async_rt::{spawn_with_inherit, spawn_with_name},
    channel::{ChId, ChReceiver, ChSender, ChSenderWeak, CHANNEL_SIZE},
    huid::{gen_huid::gen_huid, HUId},
    hex::BinStrLine,
    ice::{
        ice_peer::{default_ice_servers, IceArgs, IceConfig, IcePeer},
        ice_quic::{QuicIceCert, QuicStream, UpgradeToQuic},
        throughput::run_throughput,
        webrtc_ice_peer::{DtlsIceArgs, WebrtcIceConfig, WebrtcIcePeer},
    },
    p2p::hard_nat::HardNatMode,
    proto::{
        open_p2presponse::Open_p2p_rsp, p2pargs::P2p_args, ExecAgentScriptArgs,
        ExecAgentScriptResult, OpenP2PResponse, P2PArgs, P2PDtlsArgs, P2PHardNatArgs,
        P2PQuicArgs, QuicSocksArgs, QuicThroughputArgs, UdpRelayArgs, WebrtcThroughputArgs,
    },
    switch::{
        agent::ch_socks::ChSocks,
        entity_watch::{CtrlGuard, OpWatch, WatchResult},
        invoker_ctrl::{
            CtrlWeak, OpExecAgentScript, OpExecAgentScriptResult, OpKickDown, OpKickDownResult,
            OpOpenP2P, OpOpenP2PResult, OpOpenShell, OpOpenShellResult, OpOpenSocks,
            OpOpenSocksResult,
        },
        next_ch_id::NextChId,
        udp_relay_codec::{
            decode_udp_relay_packet, encode_udp_relay_packet, packet_has_stun_magic,
            UdpRelayCodec, UDP_RELAY_HEARTBEAT_FLOW_ID,
        },
    },
};
use tokio::{
    io::AsyncReadExt,
    net::UdpSocket,
    process::Command,
    sync::{mpsc, oneshot, Mutex as AsyncMutex},
    time::timeout,
};

use super::{
    super::invoker_ctrl::{CloseChannelResult, CtrlHandler, CtrlInvoker, OpCloseChannel},
    ch_socks::run_socks_conn,
};

use super::ch_shell::open_shell;

pub type AgentCtrlInvoker = CtrlInvoker<Entity>;

pub struct AgentCtrl {
    handle: ActorHandle<Entity>,
}

impl AgentCtrl {
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
        udp_relay_flows: Arc::new(AsyncMutex::new(HashMap::new())),
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

    let session = AgentCtrl { handle };

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
        let p2p_args = req.0.p2p_args.take().with_context(|| "no p2p args")?;
        match p2p_args {
            P2p_args::QuicSocks(mut args) => {
                let args = args
                    .base
                    .take()
                    .with_context(|| "no base in quic socks args")?;
                return handle_quic_socks(self.socks_server.clone(), args).await;
            }
            P2p_args::UdpRelay(args) => {
                return handle_udp_relay(self.udp_relay_flows.clone(), args).await
            }
            P2p_args::QuicThrput(args) => return handle_quic_throughput(args).await,
            P2p_args::WebrtcThrput(args) => return handle_webrtc_throughput(args).await,
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

#[async_trait::async_trait]
impl AsyncHandler<OpExecAgentScript> for Entity {
    type Response = OpExecAgentScriptResult;

    async fn handle(&mut self, req: OpExecAgentScript) -> Self::Response {
        if self.disable_shell {
            bail!("shell disabled")
        }
        handle_exec_agent_script(req.0).await
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

async fn handle_quic_socks(
    socks_server: super::ch_socks::Server,
    mut remote_args: P2PQuicArgs,
) -> Result<OpenP2PResponse> {
    let remote_ice: IceArgs = remote_args
        .ice
        .take()
        .with_context(|| "no ice in P2PQuicArgs")?
        .into();
    let remote_cert_der = remote_args.cert_der;

    let mut peer = IcePeer::with_config(IceConfig {
        servers: default_ice_servers(),
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

        let r = quic_tunnel_task(peer, local_cert, remote_cert_der, socks_server).await;

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

async fn quic_tunnel_task(
    mut peer: IcePeer,
    local_cert: QuicIceCert,
    remote_cert_der: Bytes,
    socks_server: super::ch_socks::Server,
) -> Result<()> {
    let conn = peer.accept().await?;
    let peer_addr = conn.remote_addr();
    let conn = conn.upgrade_to_quic(&local_cert, remote_cert_der).await?;

    loop {
        let pair = conn.accept_bi().await.with_context(|| "accept bi failed")?;
        // tracing::debug!("accept bi ok");
        let stream = QuicStream::new(pair);

        let uid = gen_huid();
        let name = format!("{uid}");
        let server = socks_server.clone();
        spawn_with_inherit(name, async move {
            let r = run_socks_conn(stream, peer_addr, server).await;
            tracing::trace!("finished {r:?}");
            r
        });
    }
}

const UDP_RELAY_DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const UDP_RELAY_PACKET_LIMIT: usize = 1452 ;
const UDP_RELAY_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(3);
const UDP_RELAY_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(30);
const EXEC_SCRIPT_DEFAULT_TIMEOUT_SECS: u64 = 30;
const EXEC_SCRIPT_DEFAULT_STDOUT_LIMIT: usize = 256 * 1024;
const EXEC_SCRIPT_DEFAULT_STDERR_LIMIT: usize = 256 * 1024;

struct ExecScriptOutput {
    exit_code: i32,
    timed_out: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_truncated: bool,
    stderr_truncated: bool,
}

async fn handle_exec_agent_script(args: ExecAgentScriptArgs) -> Result<ExecAgentScriptResult> {
    let timeout_secs = if args.timeout_secs == 0 {
        EXEC_SCRIPT_DEFAULT_TIMEOUT_SECS
    } else {
        args.timeout_secs as u64
    };
    let timeout_duration = Duration::from_secs(timeout_secs);
    let max_stdout_bytes = if args.max_stdout_bytes == 0 {
        EXEC_SCRIPT_DEFAULT_STDOUT_LIMIT
    } else {
        args.max_stdout_bytes as usize
    };
    let max_stderr_bytes = if args.max_stderr_bytes == 0 {
        EXEC_SCRIPT_DEFAULT_STDERR_LIMIT
    } else {
        args.max_stderr_bytes as usize
    };

    if args.content.is_empty() {
        return Ok(ExecAgentScriptResult {
            status_code: -1,
            reason: "empty script content".into(),
            exit_code: -1,
            ..Default::default()
        });
    }

    let script_name_raw = String::from_utf8_lossy(args.name.as_ref());
    let script_name = sanitize_script_name(script_name_raw.as_ref());
    let script_argv: Vec<String> = args.argv.iter().map(|x| x.to_string()).collect();
    let temp_path = make_exec_script_temp_path(script_name.as_str());
    let run_result = async {
        tokio::fs::write(&temp_path, args.content.as_ref())
            .await
            .with_context(|| format!("write script file failed [{}]", temp_path.display()))?;
        set_exec_script_permissions(&temp_path).await?;
        run_script_file(
            &temp_path,
            script_argv.as_slice(),
            timeout_duration,
            max_stdout_bytes,
            max_stderr_bytes,
        )
        .await
    }
    .await;

    if let Err(e) = tokio::fs::remove_file(&temp_path).await {
        if e.kind() != ErrorKind::NotFound {
            tracing::warn!(
                "remove temp script file failed [{}], err [{}]",
                temp_path.display(),
                e
            );
        }
    }

    match run_result {
        Ok(output) => Ok(ExecAgentScriptResult {
            status_code: 0,
            reason: "ok".into(),
            exit_code: output.exit_code,
            timed_out: output.timed_out,
            stdout_truncated: output.stdout_truncated,
            stderr_truncated: output.stderr_truncated,
            stdout: output.stdout.into(),
            stderr: output.stderr.into(),
            ..Default::default()
        }),
        Err(e) => Ok(ExecAgentScriptResult {
            status_code: -1,
            reason: format!("{e:#}").into(),
            exit_code: -1,
            ..Default::default()
        }),
    }
}

fn sanitize_script_name(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            output.push(ch);
        } else {
            output.push('_');
        }
    }
    if output.is_empty() {
        "script".to_string()
    } else {
        output
    }
}

fn make_exec_script_temp_path(script_name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "rtun-agent-script-{}-{}-{:016x}.sh",
        script_name,
        Local::now().timestamp_millis(),
        rand::random::<u64>()
    ));
    path
}

async fn set_exec_script_permissions(path: &PathBuf) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = tokio::fs::metadata(path)
            .await
            .with_context(|| format!("read script file metadata failed [{}]", path.display()))?
            .permissions();
        perms.set_mode(0o700);
        tokio::fs::set_permissions(path, perms)
            .await
            .with_context(|| format!("set script file permissions failed [{}]", path.display()))?;
    }
    Ok(())
}

async fn run_script_file(
    path: &PathBuf,
    script_argv: &[String],
    timeout_duration: Duration,
    max_stdout_bytes: usize,
    max_stderr_bytes: usize,
) -> Result<ExecScriptOutput> {
    let mut child = Command::new("sh")
        .arg(path.as_os_str())
        .args(script_argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn script failed [{}]", path.display()))?;

    let stdout = child.stdout.take().with_context(|| "missing stdout pipe")?;
    let stderr = child.stderr.take().with_context(|| "missing stderr pipe")?;
    let stdout_task = tokio::spawn(read_stream_capped(stdout, max_stdout_bytes));
    let stderr_task = tokio::spawn(read_stream_capped(stderr, max_stderr_bytes));

    let mut timed_out = false;
    let status = match timeout(timeout_duration, child.wait()).await {
        Ok(wait_result) => wait_result.with_context(|| "wait script process failed")?,
        Err(_) => {
            timed_out = true;
            if let Err(e) = child.kill().await {
                tracing::debug!("kill timed out script failed [{e}]");
            }
            child
                .wait()
                .await
                .with_context(|| "wait timed out script process failed")?
        }
    };

    let (stdout, stdout_truncated) = stdout_task
        .await
        .with_context(|| "join stdout reader failed")??;
    let (stderr, stderr_truncated) = stderr_task
        .await
        .with_context(|| "join stderr reader failed")??;

    Ok(ExecScriptOutput {
        exit_code: status.code().unwrap_or(-1),
        timed_out,
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
    })
}

async fn read_stream_capped<R>(mut reader: R, max_bytes: usize) -> Result<(Vec<u8>, bool)>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut truncated = false;
    let mut data = Vec::new();
    let mut buf = [0_u8; 4096];

    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        if data.len() >= max_bytes {
            truncated = true;
            continue;
        }

        let remain = max_bytes.saturating_sub(data.len());
        let take = remain.min(n);
        data.extend_from_slice(&buf[..take]);
        if take < n {
            truncated = true;
        }
    }
    Ok((data, truncated))
}

type SharedRelayFlows = Arc<AsyncMutex<HashMap<RelayFlowKey, Arc<SharedRelayFlow>>>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct RelayFlowKey {
    flow_id: u64,
    target_addr: SocketAddr,
}

impl RelayFlowKey {
    fn new(flow_id: u64, target_addr: SocketAddr) -> Self {
        Self {
            flow_id,
            target_addr,
        }
    }
}

struct SharedRelayFlowState {
    updated_at: Instant,
    flow_tx: mpsc::Sender<RelayFlowPacket>,
    stop_tx: Option<oneshot::Sender<()>>,
}

struct SharedRelayFlow {
    socket: Arc<UdpSocket>,
    state: Mutex<SharedRelayFlowState>,
}

async fn handle_udp_relay(
    shared_flows: SharedRelayFlows,
    mut remote_args: UdpRelayArgs,
) -> Result<OpenP2PResponse> {
    let hard_nat_mode = resolve_udp_relay_hard_nat_mode(&remote_args.hard_nat);
    if hard_nat_mode != HardNatMode::Off {
        tracing::warn!(
            "[relay_agent_diag] udp relay hard-nat mode [{hard_nat_mode:?}] requested but not implemented in this build; fallback to ICE-only"
        );
    }
    let remote_ice: IceArgs = remote_args
        .ice
        .take()
        .with_context(|| "no ice in udp relay args")?
        .into();
    let target_addr: SocketAddr = remote_args
        .target_addr
        .parse()
        .with_context(|| format!("invalid udp relay target [{}]", remote_args.target_addr))?;

    let idle_timeout = if remote_args.idle_timeout_secs == 0 {
        UDP_RELAY_DEFAULT_IDLE_TIMEOUT
    } else {
        Duration::from_secs(remote_args.idle_timeout_secs as u64)
    };

    let codec = UdpRelayCodec::new(remote_args.obfs_seed);
    let max_payload = resolve_udp_relay_max_payload(remote_args.max_payload, codec)?;

    let mut peer = IcePeer::with_config(IceConfig {
        servers: default_ice_servers(),
        ..Default::default()
    });

    let uid = peer.uid();
    let local_ice = peer.server_gather(remote_ice.clone()).await?;
    tracing::warn!(
        "[relay_agent_diag] starting udp relay tunnel {uid}, local {local_ice:?}, remote {remote_ice:?}, target [{target_addr}], idle {:?}, max_payload {}, codec {}",
        idle_timeout,
        max_payload,
        codec.mode_name()
    );

    spawn_with_name(format!("udp-relay-{uid}"), async move {
        let r = udp_relay_task(
            peer,
            target_addr,
            idle_timeout,
            max_payload,
            codec,
            shared_flows,
        )
        .await;
        match r {
            Ok(()) => {
                tracing::warn!(
                    "[relay_agent_diag] udp relay tunnel closed: uid [{uid}], target [{target_addr}]"
                );
            }
            Err(e) => {
                tracing::warn!(
                    "[relay_agent_diag] udp relay tunnel failed: uid [{uid}], target [{target_addr}], err [{e:#}]"
                );
            }
        }
    });

    let rsp = OpenP2PResponse {
        open_p2p_rsp: Some(Open_p2p_rsp::Args(P2PArgs {
            p2p_args: Some(P2p_args::UdpRelay(UdpRelayArgs {
                ice: Some(local_ice.into()).into(),
                target_addr: target_addr.to_string().into(),
                idle_timeout_secs: idle_timeout.as_secs().min(u32::MAX as u64) as u32,
                max_payload: max_payload as u32,
                obfs_seed: codec.obfs_seed,
                ..Default::default()
            })),
            ..Default::default()
        })),
        ..Default::default()
    };

    Ok(rsp)
}

fn resolve_udp_relay_max_payload(input: u32, codec: UdpRelayCodec) -> Result<usize> {
    let auto = udp_relay_auto_payload(codec);
    if input == 0 {
        return Ok(auto);
    }

    let value = input as usize;
    if value > auto {
        bail!(
            "udp max payload [{}] exceed p2p limit [{}], please set <= {}",
            value,
            auto,
            auto
        );
    }
    Ok(value)
}

fn udp_relay_auto_payload(codec: UdpRelayCodec) -> usize {
    UDP_RELAY_PACKET_LIMIT.saturating_sub(codec.header_len())
}

fn resolve_udp_relay_hard_nat_mode(hard_nat: &MessageField<P2PHardNatArgs>) -> HardNatMode {
    let Some(hard_nat) = hard_nat.as_ref() else {
        return HardNatMode::Off;
    };
    match hard_nat.mode {
        1 => HardNatMode::Fallback,
        2 => HardNatMode::Assist,
        3 => HardNatMode::Force,
        _ => HardNatMode::Off,
    }
}

async fn udp_relay_task(
    mut peer: IcePeer,
    target_addr: SocketAddr,
    idle_timeout: Duration,
    max_payload: usize,
    codec: UdpRelayCodec,
    shared_flows: SharedRelayFlows,
) -> Result<()> {
    let conn = peer
        .accept()
        .await
        .with_context(|| format!("udp relay accept p2p failed, target [{}]", target_addr))?;
    let (socket, _cfg, remote_addr) = conn.into_parts();
    socket.connect(remote_addr).await.with_context(|| {
        format!(
            "udp relay connect p2p remote failed, remote [{}], target [{}]",
            remote_addr, target_addr
        )
    })?;
    run_udp_relay_server_loop(
        socket,
        remote_addr,
        target_addr,
        idle_timeout,
        max_payload,
        codec,
        shared_flows,
    )
    .await
    .with_context(|| {
        format!(
            "udp relay server loop failed, remote [{}], target [{}]",
            remote_addr, target_addr
        )
    })
}

async fn run_udp_relay_server_loop(
    tunnel: UdpSocket,
    remote_addr: SocketAddr,
    target_addr: SocketAddr,
    idle_timeout: Duration,
    max_payload: usize,
    codec: UdpRelayCodec,
    shared_flows: SharedRelayFlows,
) -> Result<()> {
    let mut tunnel_buf = vec![0_u8; 64 * 1024];
    let mut tunnel_send_buf = vec![0_u8; codec.header_len() + max_payload];
    let (flow_tx, mut flow_rx) = mpsc::channel::<RelayFlowPacket>(1024);
    let mut cleanup_interval = tokio::time::interval(Duration::from_secs(1));
    cleanup_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut heartbeat_interval = tokio::time::interval(UDP_RELAY_HEARTBEAT_INTERVAL);
    heartbeat_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut heartbeat_timeout_check = tokio::time::interval(Duration::from_secs(1));
    heartbeat_timeout_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_valid_recv_at = Instant::now();

    loop {
        tokio::select! {
            r = tunnel.recv(&mut tunnel_buf) => {
                let n = r.with_context(|| {
                    format!(
                        "udp relay tunnel recv failed, remote [{}], target [{}]",
                        remote_addr, target_addr
                    )
                })?;
                if n == 0 {
                    bail!(
                        "udp relay tunnel closed, remote [{}], target [{}]",
                        remote_addr,
                        target_addr
                    );
                }
                let (flow_id, payload) = match decode_udp_relay_packet(&tunnel_buf[..n], codec) {
                    Ok(v) => v,
                    Err(e) => {
                        let packet = &tunnel_buf[..n];
                        if packet_has_stun_magic(packet) {
                            tracing::debug!(
                                "decode udp relay packet dropped stun-like packet, codec [{}], header [{}], bytes [{}], remote [{}], target [{}], packet {}, err [{}]",
                                codec.mode_name(),
                                codec.header_len(),
                                n,
                                remote_addr,
                                target_addr,
                                packet.dump_bin_limit(24),
                                e
                            );
                        } else {
                            tracing::warn!(
                                "decode udp relay packet failed, codec [{}], header [{}], bytes [{}], remote [{}], target [{}], packet {}, err [{}]",
                                codec.mode_name(),
                                codec.header_len(),
                                n,
                                remote_addr,
                                target_addr,
                                packet.dump_bin_limit(24),
                                e
                            );
                        }
                        continue;
                    }
                };
                last_valid_recv_at = Instant::now();
                if flow_id == UDP_RELAY_HEARTBEAT_FLOW_ID && payload.is_empty() {
                    continue;
                }
                if payload.len() > max_payload {
                    tracing::debug!("drop oversized udp relay request: flow [{}], size [{}], max [{}]", flow_id, payload.len(), max_payload);
                    continue;
                }

                let key = RelayFlowKey::new(flow_id, target_addr);
                let flow = get_or_create_shared_relay_flow(
                    &shared_flows,
                    key,
                    flow_tx.clone(),
                    max_payload,
                )
                .await
                .with_context(|| {
                    format!(
                        "get/create udp relay flow failed, flow [{}], remote [{}], target [{}]",
                        flow_id, remote_addr, target_addr
                    )
                })?;
                touch_shared_relay_flow(&flow);
                if let Err(e) = flow.socket.send(payload).await {
                    if e.kind() == ErrorKind::ConnectionRefused {
                        tracing::warn!(
                            "[relay_agent_diag] udp relay target refused packet, keep flow alive: flow [{}], target [{}], err [{}]",
                            flow_id,
                            target_addr,
                            e
                        );
                    } else {
                        tracing::warn!(
                            "send udp relay payload failed, keep flow alive: flow [{}], target [{}], err [{}]",
                            flow_id,
                            target_addr,
                            e
                        );
                    }
                }
            }
            r = flow_rx.recv() => {
                let Some(packet) = r else {
                    bail!("udp relay flow recv channel closed");
                };

                let frame_len = encode_udp_relay_packet(
                    &mut tunnel_send_buf,
                    packet.flow_id,
                    &packet.payload,
                    codec,
                )
                .with_context(|| {
                    format!(
                        "encode udp relay response failed, flow [{}], payload [{}], remote [{}], target [{}]",
                        packet.flow_id,
                        packet.payload.len(),
                        remote_addr,
                        target_addr
                    )
                })?;
                tunnel
                    .send(&tunnel_send_buf[..frame_len])
                    .await
                    .with_context(|| {
                        format!(
                            "udp relay tunnel send failed, flow [{}], bytes [{}], remote [{}], target [{}]",
                            packet.flow_id, frame_len, remote_addr, target_addr
                        )
                    })?;
            }
            _ = cleanup_interval.tick() => {
                cleanup_shared_relay_flows(&shared_flows, idle_timeout).await;
            }
            _ = heartbeat_interval.tick() => {
                let frame_len = encode_udp_relay_packet(
                    &mut tunnel_send_buf,
                    UDP_RELAY_HEARTBEAT_FLOW_ID,
                    &[],
                    codec,
                )
                .with_context(|| {
                    format!(
                        "encode udp relay heartbeat failed, remote [{}], target [{}]",
                        remote_addr, target_addr
                    )
                })?;
                tunnel
                    .send(&tunnel_send_buf[..frame_len])
                    .await
                    .with_context(|| {
                        format!(
                            "udp relay heartbeat send failed, bytes [{}], remote [{}], target [{}]",
                            frame_len, remote_addr, target_addr
                        )
                    })?;
            }
            _ = heartbeat_timeout_check.tick() => {
                let idle = Instant::now().saturating_duration_since(last_valid_recv_at);
                if idle >= UDP_RELAY_HEARTBEAT_TIMEOUT {
                    bail!(
                        "udp relay heartbeat timeout: no valid packet for {}ms (timeout={}ms), remote [{}], target [{}]",
                        idle.as_millis(),
                        UDP_RELAY_HEARTBEAT_TIMEOUT.as_millis(),
                        remote_addr,
                        target_addr
                    );
                }
            }
        }
    }
}

async fn get_or_create_shared_relay_flow(
    shared_flows: &SharedRelayFlows,
    key: RelayFlowKey,
    flow_tx: mpsc::Sender<RelayFlowPacket>,
    max_payload: usize,
) -> Result<Arc<SharedRelayFlow>> {
    {
        let flows = shared_flows.lock().await;
        if let Some(flow) = flows.get(&key) {
            let flow = flow.clone();
            bind_shared_flow_sender(&flow, flow_tx);
            return Ok(flow);
        }
    }

    let bind_addr = any_addr_for(key.target_addr);
    let socket = UdpSocket::bind(bind_addr)
        .await
        .with_context(|| format!("bind udp relay flow failed [{}]", bind_addr))?;
    socket
        .connect(key.target_addr)
        .await
        .with_context(|| format!("connect udp relay target failed [{}]", key.target_addr))?;
    let socket = Arc::new(socket);
    let local_src_addr = socket.local_addr().ok();

    let (stop_tx, stop_rx) = oneshot::channel();
    let flow = Arc::new(SharedRelayFlow {
        socket,
        state: Mutex::new(SharedRelayFlowState {
            updated_at: Instant::now(),
            flow_tx: flow_tx.clone(),
            stop_tx: Some(stop_tx),
        }),
    });

    let active_flows = {
        let mut flows = shared_flows.lock().await;
        if let Some(existing) = flows.get(&key) {
            let existing = existing.clone();
            bind_shared_flow_sender(&existing, flow_tx.clone());
            return Ok(existing);
        }
        flows.insert(key, flow.clone());
        flows.len()
    };

    tracing::debug!(
        "udp relay flow created: flow [{}], route [{} -> {}], active [{}]",
        key.flow_id,
        local_src_addr
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".to_string()),
        key.target_addr,
        active_flows,
    );

    let socket2 = flow.socket.clone();
    let flow2 = flow.clone();
    let flow_key = key;
    spawn_with_name(format!("udp-relay-flow-{}", key.flow_id), async move {
        let r = relay_flow_recv_task(flow_key.flow_id, socket2, flow2, stop_rx, max_payload).await;
        match &r {
            Ok(()) => {
                tracing::debug!(
                    "udp relay flow recv task stopped: flow [{}], target [{}]",
                    flow_key.flow_id,
                    flow_key.target_addr
                );
            }
            Err(e) => {
                tracing::warn!(
                    "[relay_agent_diag] udp relay flow recv task failed: flow [{}], target [{}], err [{e:#}]",
                    flow_key.flow_id,
                    flow_key.target_addr,
                );
            }
        }
        r
    });

    Ok(flow)
}

fn bind_shared_flow_sender(flow: &Arc<SharedRelayFlow>, flow_tx: mpsc::Sender<RelayFlowPacket>) {
    if let Ok(mut state) = flow.state.lock() {
        state.updated_at = Instant::now();
        state.flow_tx = flow_tx;
    }
}

fn touch_shared_relay_flow(flow: &Arc<SharedRelayFlow>) {
    if let Ok(mut state) = flow.state.lock() {
        state.updated_at = Instant::now();
    }
}

async fn remove_shared_relay_flow(
    shared_flows: &SharedRelayFlows,
    key: RelayFlowKey,
    reason: &str,
) -> bool {
    let (removed, active_flows) = {
        let mut flows = shared_flows.lock().await;
        let removed = flows.remove(&key);
        (removed, flows.len())
    };
    let Some(flow) = removed else {
        return false;
    };
    let local_src_addr = flow.socket.local_addr().ok();
    if let Ok(mut state) = flow.state.lock() {
        if let Some(stop_tx) = state.stop_tx.take() {
            let _ = stop_tx.send(());
        }
    }
    tracing::debug!(
        "udp relay flow closed: flow [{}], route [{} -> {}], reason [{}], active [{}]",
        key.flow_id,
        local_src_addr
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".to_string()),
        key.target_addr,
        reason,
        active_flows,
    );
    true
}

async fn cleanup_shared_relay_flows(shared_flows: &SharedRelayFlows, idle_timeout: Duration) {
    let now = Instant::now();
    let expired: Vec<RelayFlowKey> = {
        let flows = shared_flows.lock().await;
        let mut out = Vec::new();
        for (key, flow) in flows.iter() {
            let idle = flow
                .state
                .lock()
                .ok()
                .map(|s| now.duration_since(s.updated_at))
                .unwrap_or_default();
            if idle >= idle_timeout {
                out.push(*key);
            }
        }
        out
    };

    for key in expired {
        if remove_shared_relay_flow(shared_flows, key, "idle-timeout").await {
            tracing::debug!("udp relay flow timeout: flow [{}]", key.flow_id);
        }
    }
}

async fn relay_flow_recv_task(
    flow_id: u64,
    socket: Arc<UdpSocket>,
    flow: Arc<SharedRelayFlow>,
    mut stop_rx: oneshot::Receiver<()>,
    max_payload: usize,
) -> Result<()> {
    let mut buf = vec![0_u8; 64 * 1024];
    loop {
        tokio::select! {
            _ = &mut stop_rx => {
                return Ok(())
            }
            r = socket.recv(&mut buf) => {
                let n = match r {
                    Ok(v) => v,
                    Err(e) if e.kind() == ErrorKind::ConnectionRefused => {
                        continue;
                    }
                    Err(e) => return Err(e.into()),
                };
                if n == 0 {
                    continue;
                }
                if n > max_payload {
                    tracing::warn!("drop oversized udp relay response: flow [{}], size [{}], max [{}]", flow_id, n, max_payload);
                    continue;
                }

                let mut payload = BytesMut::with_capacity(n);
                payload.put_slice(&buf[..n]);
                let tx = if let Ok(mut state) = flow.state.lock() {
                    state.updated_at = Instant::now();
                    state.flow_tx.clone()
                } else {
                    continue;
                };
                if tx.send(RelayFlowPacket { flow_id, payload }).await.is_err() {
                    // Tunnel receiver may have rotated; keep socket alive and wait for new binding.
                    continue;
                }
            }
        }
    }
}

fn any_addr_for(target_addr: SocketAddr) -> SocketAddr {
    match target_addr.ip() {
        IpAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
        IpAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
    }
}

struct RelayFlowPacket {
    flow_id: u64,
    payload: BytesMut,
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
        servers: default_ice_servers(),
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
            let conn = peer
                .accept()
                .await?
                .upgrade_to_quic(&local_cert, remote_cert)
                .await?;
            let (wr, rd) = conn.accept_bi().await?;
            run_throughput(rd, wr, thr_args).await
        }
        .await;

        tracing::debug!("finished {r:?}");
    });

    // peer.into_accept_and_chat(remote_args).await?;

    Ok(rsp)
}

async fn handle_webrtc_throughput(
    mut remote_args: WebrtcThroughputArgs,
) -> Result<OpenP2PResponse> {
    tracing::debug!("handle_webrtc_throughput");

    let mut base = remote_args.take_base();
    let thr_args = remote_args.take_throughput();

    // let remote_args: IceArgs = req.0.args.take().with_context(||"no remote p2p args")?.into();
    let remote_args = DtlsIceArgs {
        ice: base.take_ice().into(),
        cert_fingerprint: base.cert_fingerprint.map(|x| x.into()),
    };

    let mut peer = WebrtcIcePeer::with_config(WebrtcIceConfig {
        servers: default_ice_servers(),
        disable_dtls: remote_args.cert_fingerprint.is_none(),
        ..Default::default()
    });

    let local_args = peer.gather_until_done().await?;

    let rsp = OpenP2PResponse {
        open_p2p_rsp: Some(Open_p2p_rsp::Args(P2PArgs {
            p2p_args: Some(P2p_args::WebrtcThrput(WebrtcThroughputArgs {
                base: MessageField::some(P2PDtlsArgs {
                    ice: Some(local_args.ice.into()).into(),
                    cert_fingerprint: local_args.cert_fingerprint.map(|x| x.into()),
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
                run_throughput(rd.compat(), wr.compat_write(), thr_args),
            )
            .await?
        }
        .await;

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
    udp_relay_flows: SharedRelayFlows,
    weak: Option<CtrlWeak<Self>>,
    guard: CtrlGuard,
    disable_shell: bool,
}

impl Entity {
    fn add_channel(&mut self, ch_id: ChId, peer_tx: &ChSender) -> (ChSender, ChReceiver) {
        let (local_tx, local_rx) = mpsc::channel(CHANNEL_SIZE);
        let local_tx = ChSender::new(ch_id, local_tx);
        let local_rx = ChReceiver::new(local_rx);

        self.channels.insert(
            ch_id,
            ChItem {
                peer_tx: peer_tx.downgrade(),
                local_tx: local_tx.downgrade(),
            },
        );

        (local_tx, local_rx)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn udp_relay_hard_nat_mode_defaults_to_off_when_field_missing_or_zero() {
        let args = UdpRelayArgs::default();
        assert_eq!(resolve_udp_relay_hard_nat_mode(&args.hard_nat), HardNatMode::Off);

        let args = UdpRelayArgs {
            hard_nat: MessageField::some(P2PHardNatArgs {
                mode: 0,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(resolve_udp_relay_hard_nat_mode(&args.hard_nat), HardNatMode::Off);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn udp_relay_same_flow_should_keep_stable_target_src_port_across_tunnels() -> Result<()> {
        // UDP connect() does not require target to be online; use a fixed local address
        // so this test only validates flow/socket reuse semantics.
        let target_addr: SocketAddr = "127.0.0.1:9".parse()?;
        let shared_flows: SharedRelayFlows = Arc::new(AsyncMutex::new(HashMap::new()));
        let (flow_tx, _flow_rx) = mpsc::channel::<RelayFlowPacket>(8);

        // Simulate same flow_id arriving via two different tunnels on agent side.
        let flow_a = get_or_create_shared_relay_flow(
            &shared_flows,
            RelayFlowKey::new(1, target_addr),
            flow_tx.clone(),
            1200,
        )
        .await
        .with_context(|| "create first relay flow failed")?;
        let flow_b = get_or_create_shared_relay_flow(
            &shared_flows,
            RelayFlowKey::new(1, target_addr),
            flow_tx,
            1200,
        )
        .await
        .with_context(|| "create second relay flow failed")?;

        let src_port_a = flow_a.socket.local_addr()?.port();
        let src_port_b = flow_b.socket.local_addr()?.port();

        let _ = remove_shared_relay_flow(
            &shared_flows,
            RelayFlowKey::new(1, target_addr),
            "test-cleanup",
        )
        .await;

        assert_eq!(
            src_port_a, src_port_b,
            "same flow should preserve target-side source port across tunnel switch, but changed from [{src_port_a}] to [{src_port_b}]"
        );

        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn exec_agent_script_should_return_stdout_stderr_and_exit_code() -> Result<()> {
        let rsp = handle_exec_agent_script(ExecAgentScriptArgs {
            name: "test".into(),
            content: b"#!/usr/bin/env sh\necho hello\necho error >&2\n"
                .to_vec()
                .into(),
            timeout_secs: 3,
            max_stdout_bytes: 4096,
            max_stderr_bytes: 4096,
            ..Default::default()
        })
        .await?;

        assert_eq!(rsp.status_code, 0);
        assert_eq!(rsp.exit_code, 0);
        assert!(!rsp.timed_out);

        let stdout = String::from_utf8_lossy(rsp.stdout.as_ref());
        let stderr = String::from_utf8_lossy(rsp.stderr.as_ref());
        assert!(stdout.contains("hello"), "stdout={stdout}");
        assert!(stderr.contains("error"), "stderr={stderr}");

        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn exec_agent_script_should_receive_argv() -> Result<()> {
        let rsp = handle_exec_agent_script(ExecAgentScriptArgs {
            name: "argv".into(),
            content: b"#!/usr/bin/env sh\necho \"arg1=$1\"\necho \"arg2=$2\"\n"
                .to_vec()
                .into(),
            argv: vec![
                protobuf::Chars::from("foo"),
                protobuf::Chars::from("bar baz"),
            ],
            timeout_secs: 3,
            max_stdout_bytes: 4096,
            max_stderr_bytes: 4096,
            ..Default::default()
        })
        .await?;

        assert_eq!(rsp.status_code, 0);
        assert_eq!(rsp.exit_code, 0);
        assert!(!rsp.timed_out);

        let stdout = String::from_utf8_lossy(rsp.stdout.as_ref());
        assert!(stdout.contains("arg1=foo"), "stdout={stdout}");
        assert!(stdout.contains("arg2=bar baz"), "stdout={stdout}");

        Ok(())
    }
}
