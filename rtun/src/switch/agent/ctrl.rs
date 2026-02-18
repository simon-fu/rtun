use std::{
    collections::HashMap,
    io::ErrorKind,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use bytes::{Buf, BufMut, Bytes, BytesMut};
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
    ice::{
        ice_peer::{IceArgs, IceConfig, IcePeer},
        ice_quic::{QuicIceCert, QuicStream, UpgradeToQuic},
        throughput::run_throughput,
        webrtc_ice_peer::{DtlsIceArgs, WebrtcIceConfig, WebrtcIcePeer},
    },
    proto::{
        open_p2presponse::Open_p2p_rsp, p2pargs::P2p_args, OpenP2PResponse, P2PArgs, P2PDtlsArgs,
        P2PQuicArgs, QuicSocksArgs, QuicThroughputArgs, UdpRelayArgs, WebrtcThroughputArgs,
    },
    switch::{
        agent::ch_socks::ChSocks,
        entity_watch::{CtrlGuard, OpWatch, WatchResult},
        invoker_ctrl::{
            CtrlWeak, OpKickDown, OpKickDownResult, OpOpenP2P, OpOpenP2PResult, OpOpenShell,
            OpOpenShellResult, OpOpenSocks, OpOpenSocksResult,
        },
        next_ch_id::NextChId,
    },
};
use tokio::{
    net::UdpSocket,
    sync::{mpsc, oneshot},
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
            P2p_args::UdpRelay(args) => return handle_udp_relay(args).await,
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

const UDP_RELAY_META_LEN: usize = 8;
const UDP_RELAY_DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const UDP_RELAY_PACKET_LIMIT: usize = 1400;
const UDP_RELAY_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(3);
const UDP_RELAY_HEARTBEAT_FLOW_ID: u64 = 0;

async fn handle_udp_relay(mut remote_args: UdpRelayArgs) -> Result<OpenP2PResponse> {
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

    let max_payload = resolve_udp_relay_max_payload(remote_args.max_payload)?;

    let mut peer = IcePeer::with_config(IceConfig {
        servers: vec![
            "stun:stun1.l.google.com:19302".into(),
            "stun:stun2.l.google.com:19302".into(),
            "stun:stun.qq.com:3478".into(),
        ],
        ..Default::default()
    });

    let uid = peer.uid();
    let local_ice = peer.server_gather(remote_ice.clone()).await?;
    tracing::debug!(
        "starting udp relay tunnel {uid}, local {local_ice:?}, remote {remote_ice:?}, target [{target_addr}], idle {:?}, max_payload {}",
        idle_timeout,
        max_payload
    );

    spawn_with_name(format!("udp-relay-{uid}"), async move {
        let r = udp_relay_task(peer, target_addr, idle_timeout, max_payload).await;
        tracing::debug!("finished {r:?}");
    });

    let rsp = OpenP2PResponse {
        open_p2p_rsp: Some(Open_p2p_rsp::Args(P2PArgs {
            p2p_args: Some(P2p_args::UdpRelay(UdpRelayArgs {
                ice: Some(local_ice.into()).into(),
                target_addr: target_addr.to_string().into(),
                idle_timeout_secs: idle_timeout.as_secs().min(u32::MAX as u64) as u32,
                max_payload: max_payload as u32,
                ..Default::default()
            })),
            ..Default::default()
        })),
        ..Default::default()
    };

    Ok(rsp)
}

fn resolve_udp_relay_max_payload(input: u32) -> Result<usize> {
    let auto = udp_relay_auto_payload();
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

fn udp_relay_auto_payload() -> usize {
    UDP_RELAY_PACKET_LIMIT.saturating_sub(UDP_RELAY_META_LEN)
}

async fn udp_relay_task(
    mut peer: IcePeer,
    target_addr: SocketAddr,
    idle_timeout: Duration,
    max_payload: usize,
) -> Result<()> {
    let conn = peer.accept().await?;
    let (socket, _cfg, remote_addr) = conn.into_parts();
    socket.connect(remote_addr).await?;
    run_udp_relay_server_loop(socket, target_addr, idle_timeout, max_payload).await
}

async fn run_udp_relay_server_loop(
    tunnel: UdpSocket,
    target_addr: SocketAddr,
    idle_timeout: Duration,
    max_payload: usize,
) -> Result<()> {
    let mut tunnel_buf = vec![0_u8; 64 * 1024];
    let mut flows: HashMap<u64, RelayFlow> = HashMap::new();
    let (flow_tx, mut flow_rx) = mpsc::channel::<RelayFlowPacket>(1024);
    let mut cleanup_interval = tokio::time::interval(Duration::from_secs(1));
    cleanup_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut heartbeat_interval = tokio::time::interval(UDP_RELAY_HEARTBEAT_INTERVAL);
    heartbeat_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            r = tunnel.recv(&mut tunnel_buf) => {
                let n = r?;
                if n == 0 {
                    bail!("udp relay tunnel closed");
                }
                let (flow_id, payload) = decode_udp_relay_packet(&tunnel_buf[..n])?;
                if flow_id == UDP_RELAY_HEARTBEAT_FLOW_ID && payload.is_empty() {
                    continue;
                }
                if payload.len() > max_payload {
                    tracing::warn!("drop oversized udp relay request: flow [{}], size [{}], max [{}]", flow_id, payload.len(), max_payload);
                    continue;
                }

                if !flows.contains_key(&flow_id) {
                    let flow = new_relay_flow(flow_id, target_addr, flow_tx.clone(), max_payload).await?;
                    flows.insert(flow_id, flow);
                }

                if let Some(flow) = flows.get_mut(&flow_id) {
                    flow.updated_at = Instant::now();
                    if let Err(e) = flow.socket.send(payload).await {
                        tracing::warn!("send udp relay payload failed for flow [{}]: {e}", flow_id);
                        if let Some(mut removed) = flows.remove(&flow_id) {
                            if let Some(stop_tx) = removed.stop_tx.take() {
                                let _ = stop_tx.send(());
                            }
                        }
                    }
                }
            }
            r = flow_rx.recv() => {
                let Some(packet) = r else {
                    bail!("udp relay flow recv channel closed");
                };

                let mut frame = BytesMut::with_capacity(UDP_RELAY_META_LEN + packet.payload.len());
                encode_udp_relay_packet(&mut frame, packet.flow_id, &packet.payload)?;
                tunnel.send(&frame[..]).await?;

                if let Some(flow) = flows.get_mut(&packet.flow_id) {
                    flow.updated_at = Instant::now();
                }
            }
            _ = cleanup_interval.tick() => {
                cleanup_relay_flows(&mut flows, idle_timeout);
            }
            _ = heartbeat_interval.tick() => {
                let mut frame = BytesMut::with_capacity(UDP_RELAY_META_LEN);
                encode_udp_relay_packet(&mut frame, UDP_RELAY_HEARTBEAT_FLOW_ID, &[])?;
                tunnel.send(&frame[..]).await?;
            }
        }
    }
}

async fn new_relay_flow(
    flow_id: u64,
    target_addr: SocketAddr,
    flow_tx: mpsc::Sender<RelayFlowPacket>,
    max_payload: usize,
) -> Result<RelayFlow> {
    let bind_addr = any_addr_for(target_addr);
    let socket = UdpSocket::bind(bind_addr)
        .await
        .with_context(|| format!("bind udp relay flow failed [{}]", bind_addr))?;
    socket
        .connect(target_addr)
        .await
        .with_context(|| format!("connect udp relay target failed [{}]", target_addr))?;
    let socket = Arc::new(socket);

    let (stop_tx, stop_rx) = oneshot::channel();
    let socket2 = socket.clone();
    spawn_with_name(format!("udp-relay-flow-{flow_id}"), async move {
        let r = relay_flow_recv_task(flow_id, socket2, flow_tx, stop_rx, max_payload).await;
        tracing::trace!("finished {r:?}");
        r
    });

    Ok(RelayFlow {
        socket,
        updated_at: Instant::now(),
        stop_tx: Some(stop_tx),
    })
}

fn cleanup_relay_flows(flows: &mut HashMap<u64, RelayFlow>, idle_timeout: Duration) {
    let now = Instant::now();
    let mut expired = Vec::new();
    for (flow_id, flow) in flows.iter() {
        if now.duration_since(flow.updated_at) >= idle_timeout {
            expired.push(*flow_id);
        }
    }

    for flow_id in expired {
        if let Some(mut flow) = flows.remove(&flow_id) {
            if let Some(stop_tx) = flow.stop_tx.take() {
                let _ = stop_tx.send(());
            }
        }
    }
}

async fn relay_flow_recv_task(
    flow_id: u64,
    socket: Arc<UdpSocket>,
    tx: mpsc::Sender<RelayFlowPacket>,
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
                if tx.send(RelayFlowPacket { flow_id, payload }).await.is_err() {
                    return Ok(());
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

fn encode_udp_relay_packet(buf: &mut BytesMut, flow_id: u64, payload: &[u8]) -> Result<()> {
    if payload.len() > u16::MAX as usize {
        bail!("payload too large [{}]", payload.len());
    }
    buf.put_slice(&flow_id.to_be_bytes()[2..]);
    buf.put_u16(payload.len() as u16);
    buf.put_slice(payload);
    Ok(())
}

fn decode_udp_relay_packet(packet: &[u8]) -> Result<(u64, &[u8])> {
    if packet.len() < UDP_RELAY_META_LEN {
        bail!(
            "packet as least [{}] but [{}]",
            UDP_RELAY_META_LEN,
            packet.len()
        );
    }

    let mut meta = &packet[..UDP_RELAY_META_LEN];
    let flow_id = u64::from_be_bytes([0, 0, meta[0], meta[1], meta[2], meta[3], meta[4], meta[5]]);
    meta.advance(6);
    let len = meta.get_u16() as usize;
    if len > packet.len() - UDP_RELAY_META_LEN {
        bail!(
            "meta.len [{}] exceed [{}]",
            len,
            packet.len() - UDP_RELAY_META_LEN
        );
    }

    let payload = &packet[UDP_RELAY_META_LEN..UDP_RELAY_META_LEN + len];
    Ok((flow_id, payload))
}

struct RelayFlow {
    socket: Arc<UdpSocket>,
    updated_at: Instant,
    stop_tx: Option<oneshot::Sender<()>>,
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
