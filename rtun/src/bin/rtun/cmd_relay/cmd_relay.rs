use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use chrono::Local;
use clap::Parser;
use regex::Regex;
use rtun::{
    async_rt::spawn_with_name,
    ice::ice_peer::{IceArgs, IceConfig, IcePeer},
    proto::{open_p2presponse::Open_p2p_rsp, p2pargs::P2p_args, P2PArgs, UdpRelayArgs},
    switch::{
        invoker_ctrl::{CtrlHandler, CtrlInvoker},
        session_stream::make_stream_session,
    },
    ws::client::ws_connect_to,
};
use tokio::{
    net::UdpSocket,
    time::{self, MissedTickBehavior},
};

use crate::{
    client_utils::get_agents,
    init_log_and_run, quic_signal,
    rest_proto::{make_sub_url, make_ws_scheme, AgentInfo},
    secret::token_gen,
};

const DEFAULT_UDP_IDLE_TIMEOUT_SECS: u64 = 120;
const DEFAULT_P2P_PACKET_LIMIT: usize = 1400;
const UDP_RELAY_META_LEN_LEGACY: usize = 8;
const UDP_RELAY_META_LEN_OBFS: usize = 9;
const UDP_RELAY_FLOW_ID_MASK: u64 = (1_u64 << 48) - 1;
const LOOP_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const FLOW_CLEANUP_INTERVAL: Duration = Duration::from_secs(1);
const UDP_RELAY_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(3);
const UDP_RELAY_HEARTBEAT_FLOW_ID: u64 = 0;

pub fn run(args: CmdArgs) -> Result<()> {
    init_log_and_run(do_run(args))?
}

async fn do_run(args: CmdArgs) -> Result<()> {
    let signal_url = url::Url::parse(&args.url).with_context(|| "invalid url")?;
    if !signal_url.scheme().eq_ignore_ascii_case("http")
        && !signal_url.scheme().eq_ignore_ascii_case("https")
        && !signal_url.scheme().eq_ignore_ascii_case("quic")
    {
        bail!("unsupported protocol [{}]", signal_url.scheme());
    }

    let agent_regex = Regex::new(&args.agent).with_context(|| "invalid agent regex")?;
    let idle_timeout_secs = if args.udp_idle_timeout == 0 {
        DEFAULT_UDP_IDLE_TIMEOUT_SECS
    } else {
        args.udp_idle_timeout
    };
    let idle_timeout = Duration::from_secs(idle_timeout_secs);
    let max_payload = resolve_udp_max_payload(args.udp_max_payload)?;

    let mut rules = Vec::with_capacity(args.local_rules.len());
    for rule in args.local_rules {
        let rule = RelayRule::parse(rule.as_str())?;
        if !rule.is_udp() {
            bail!("only udp relay is supported");
        }
        rules.push(rule);
    }

    tracing::info!(
        "relay defaults: udp_idle_timeout={}s, udp_max_payload={} bytes",
        idle_timeout_secs,
        max_payload
    );

    for (idx, rule) in rules.into_iter().enumerate() {
        let worker = RelayWorker {
            signal_url: signal_url.clone(),
            secret: args.secret.clone(),
            quic_insecure: args.quic_insecure,
            agent_regex: agent_regex.clone(),
            idle_timeout,
            max_payload,
            rule,
        };

        let task_name = format!("relay-udp-{idx}");
        spawn_with_name(task_name, async move {
            let r = run_worker(worker).await;
            tracing::warn!("relay worker exited [{r:?}]");
            r
        });
    }

    futures::future::pending::<()>().await;
    #[allow(unreachable_code)]
    Ok(())
}

async fn run_worker(worker: RelayWorker) -> Result<()> {
    let local = UdpSocket::bind(worker.rule.listen)
        .await
        .with_context(|| format!("bind local udp failed [{}]", worker.rule.listen))?;

    tracing::info!(
        "relay(udp) listen on [{}] -> [{}]",
        worker.rule.listen,
        worker.rule.target
    );

    loop {
        let selected = select_latest_agent(
            &worker.signal_url,
            &worker.agent_regex,
            worker.quic_insecure,
        )
        .await;

        let selected = match selected {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!("select agent failed [{e}]");
                time::sleep(LOOP_RETRY_INTERVAL).await;
                continue;
            }
        };

        tracing::info!(
            "selected agent [{}], instance [{:?}], expire_at [{}]",
            selected.name,
            selected.instance_id,
            selected.expire_at
        );

        let r = if worker.signal_url.scheme().eq_ignore_ascii_case("quic") {
            run_with_quic_signal(&worker, &local, &selected).await
        } else {
            run_with_ws_signal(&worker, &local, &selected).await
        };

        match r {
            Ok(()) => {
                tracing::warn!("relay session closed");
            }
            Err(e) => {
                tracing::warn!("relay session failed [{e}]");
            }
        }

        time::sleep(LOOP_RETRY_INTERVAL).await;
    }
}

async fn run_with_ws_signal(
    worker: &RelayWorker,
    local: &UdpSocket,
    selected: &AgentInfo,
) -> Result<()> {
    let sub_url = make_ws_sub_url(&worker.signal_url, selected, worker.secret.as_deref())?;
    let (stream, _rsp) = ws_connect_to(sub_url.as_str())
        .await
        .with_context(|| format!("connect to agent failed [{}]", sub_url))?;
    let mut session = make_stream_session(stream.split(), false).await?;
    tracing::info!("relay session connected, agent [{}]", selected.name);

    let ctrl = session.ctrl_client().clone_invoker();
    tokio::select! {
        r = run_relay_session(ctrl, local, worker.rule.target, worker.idle_timeout, worker.max_payload) => r,
        r = session.wait_for_completed() => {
            r?;
            bail!("signal session closed")
        }
    }
}

async fn run_with_quic_signal(
    worker: &RelayWorker,
    local: &UdpSocket,
    selected: &AgentInfo,
) -> Result<()> {
    let sub_url = make_quic_sub_url(&worker.signal_url, selected, worker.secret.as_deref())?;
    let stream = quic_signal::connect_sub_with_opts(&sub_url, worker.quic_insecure)
        .await
        .with_context(|| format!("connect to agent failed [{}]", sub_url))?;
    let mut session = make_stream_session(stream.split(), false).await?;
    tracing::info!("relay session connected, agent [{}]", selected.name);

    let ctrl = session.ctrl_client().clone_invoker();
    tokio::select! {
        r = run_relay_session(ctrl, local, worker.rule.target, worker.idle_timeout, worker.max_payload) => r,
        r = session.wait_for_completed() => {
            r?;
            bail!("signal session closed")
        }
    }
}

async fn run_relay_session<H: CtrlHandler>(
    ctrl: CtrlInvoker<H>,
    local: &UdpSocket,
    target: SocketAddr,
    idle_timeout: Duration,
    max_payload: usize,
) -> Result<()> {
    let mut peer = IcePeer::with_config(IceConfig {
        servers: default_ice_servers(),
        ..Default::default()
    });

    let local_ice = peer.client_gather().await?;
    let idle_timeout_secs = u32::try_from(idle_timeout.as_secs())
        .with_context(|| format!("udp idle timeout too large [{}s]", idle_timeout.as_secs()))?;

    let local_codec = UdpRelayCodec::new(gen_udp_relay_obfs_seed());
    let rsp = ctrl
        .open_p2p(P2PArgs {
            p2p_args: Some(P2p_args::UdpRelay(UdpRelayArgs {
                ice: Some(local_ice.into()).into(),
                target_addr: target.to_string().into(),
                idle_timeout_secs,
                max_payload: max_payload as u32,
                obfs_seed: local_codec.obfs_seed,
                ..Default::default()
            })),
            ..Default::default()
        })
        .await?;

    let rsp = rsp.open_p2p_rsp.with_context(|| "no open_p2p_rsp")?;
    let (remote_ice, codec) = match rsp {
        Open_p2p_rsp::Args(mut args) => {
            if !args.has_udp_relay() {
                bail!("no udp relay args");
            }
            let mut relay_args = args.take_udp_relay();
            let codec = UdpRelayCodec::new(relay_args.obfs_seed);
            let remote_ice: IceArgs = relay_args
                .ice
                .take()
                .with_context(|| "no ice in udp relay args")?
                .into();
            (remote_ice, codec)
        }
        Open_p2p_rsp::Status(s) => {
            bail!("open p2p but {s:?}");
        }
        _ => {
            bail!("unknown Open_p2p_rsp {rsp:?}");
        }
    };

    let conn = peer.dial(remote_ice).await?;
    let (socket, _cfg, remote_addr) = conn.into_parts();
    socket.connect(remote_addr).await?;
    tracing::info!("relay udp codec mode [{}]", codec.mode_name());
    relay_loop(local, socket, target, idle_timeout, max_payload, codec).await
}

async fn relay_loop(
    local: &UdpSocket,
    tunnel: UdpSocket,
    target: SocketAddr,
    idle_timeout: Duration,
    max_payload: usize,
    codec: UdpRelayCodec,
) -> Result<()> {
    let mut local_buf = vec![0_u8; 64 * 1024];
    let mut tun_buf = vec![0_u8; 64 * 1024];
    let mut tunnel_send_buf = vec![0_u8; codec.header_len() + max_payload];
    let mut src_to_flow: HashMap<SocketAddr, ClientFlow> = HashMap::new();
    let mut flow_to_src: HashMap<u64, SocketAddr> = HashMap::new();
    let mut next_flow = 1_u64;

    let mut cleanup = time::interval(FLOW_CLEANUP_INTERVAL);
    cleanup.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut heartbeat = time::interval(UDP_RELAY_HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            r = local.recv_from(&mut local_buf) => {
                let (n, from) = r?;
                if n == 0 {
                    continue;
                }
                if n > max_payload {
                    tracing::warn!("drop oversized local udp packet: from [{}], size [{}], max [{}]", from, n, max_payload);
                    continue;
                }

                let now = Instant::now();
                let flow_id = if let Some(flow) = src_to_flow.get_mut(&from) {
                    flow.updated_at = now;
                    flow.flow_id
                } else {
                    let flow_id = next_flow;
                    next_flow = next_nonzero_flow_id(next_flow);
                    src_to_flow.insert(from, ClientFlow {
                        flow_id,
                        updated_at: now,
                    });
                    flow_to_src.insert(flow_id, from);
                    tracing::debug!(
                        "relay flow created: id [{}], src [{}], target [{}]",
                        flow_id,
                        from,
                        target
                    );
                    flow_id
                };

                let packet_len = encode_udp_relay_packet(
                    &mut tunnel_send_buf,
                    flow_id,
                    &local_buf[..n],
                    codec,
                )?;
                if let Err(e) = tunnel.send(&tunnel_send_buf[..packet_len]).await {
                    tracing::warn!(
                        "relay flow closed(error): id [{}], src [{}], reason [{}]",
                        flow_id,
                        from,
                        e
                    );
                    return Err(e.into());
                }
            }
            r = tunnel.recv(&mut tun_buf) => {
                let n = r?;
                if n == 0 {
                    bail!("relay tunnel closed");
                }

                let (flow_id, payload) = decode_udp_relay_packet(&tun_buf[..n], codec)?;
                if flow_id == UDP_RELAY_HEARTBEAT_FLOW_ID && payload.is_empty() {
                    continue;
                }
                if payload.len() > max_payload {
                    tracing::warn!("drop oversized tunnel udp packet: flow [{}], size [{}], max [{}]", flow_id, payload.len(), max_payload);
                    continue;
                }

                if let Some(from) = flow_to_src.get(&flow_id).copied() {
                    if let Some(flow) = src_to_flow.get_mut(&from) {
                        flow.updated_at = Instant::now();
                    }
                    if let Err(e) = local.send_to(payload, from).await {
                        tracing::warn!(
                            "relay flow closed(error): id [{}], src [{}], reason [{}]",
                            flow_id,
                            from,
                            e
                        );
                    }
                } else {
                    tracing::debug!("drop tunnel packet for unknown flow [{}]", flow_id);
                }
            }
            _ = cleanup.tick() => {
                cleanup_client_flows(&mut src_to_flow, &mut flow_to_src, idle_timeout);
            }
            _ = heartbeat.tick() => {
                let packet_len = encode_udp_relay_packet(
                    &mut tunnel_send_buf,
                    UDP_RELAY_HEARTBEAT_FLOW_ID,
                    &[],
                    codec,
                )?;
                tunnel.send(&tunnel_send_buf[..packet_len]).await?;
            }
        }
    }
}

fn cleanup_client_flows(
    src_to_flow: &mut HashMap<SocketAddr, ClientFlow>,
    flow_to_src: &mut HashMap<u64, SocketAddr>,
    idle_timeout: Duration,
) {
    let now = Instant::now();
    let mut expired = Vec::new();
    for (src, flow) in src_to_flow.iter() {
        let idle_elapsed = now.duration_since(flow.updated_at);
        if idle_elapsed >= idle_timeout {
            expired.push((*src, flow.flow_id, idle_elapsed));
        }
    }
    for (src, flow_id, idle_elapsed) in expired {
        tracing::debug!(
            "relay flow closed(timeout): id [{}], src [{}], idle={}s",
            flow_id,
            src,
            idle_elapsed.as_secs()
        );
        src_to_flow.remove(&src);
        flow_to_src.remove(&flow_id);
    }
}

fn next_nonzero_flow_id(curr: u64) -> u64 {
    let mut next = curr.wrapping_add(1);
    if next == 0 {
        next = 1;
    }
    next
}

fn resolve_udp_max_payload(input: Option<usize>) -> Result<usize> {
    let auto = max_udp_payload_auto();
    match input {
        Some(v) => {
            if v == 0 {
                bail!("udp max payload must be > 0");
            }
            if v > auto {
                bail!(
                    "udp max payload [{}] exceed p2p limit [{}], please set <= {}",
                    v,
                    auto,
                    auto
                );
            }
            Ok(v)
        }
        None => Ok(auto),
    }
}

fn max_udp_payload_auto() -> usize {
    DEFAULT_P2P_PACKET_LIMIT.saturating_sub(UDP_RELAY_META_LEN_OBFS)
}

async fn select_latest_agent(
    signal_url: &url::Url,
    agent_regex: &Regex,
    quic_insecure: bool,
) -> Result<AgentInfo> {
    let mut agents = if signal_url.scheme().eq_ignore_ascii_case("quic") {
        quic_signal::query_sessions_with_opts(signal_url, quic_insecure).await?
    } else {
        get_agents(signal_url).await?
    };

    agents.retain(|x| agent_regex.is_match(&x.name));
    if agents.is_empty() {
        bail!("no matched agent");
    }

    agents.sort_by(|a, b| b.expire_at.cmp(&a.expire_at));
    Ok(agents.swap_remove(0))
}

fn make_ws_sub_url(
    signal_url: &url::Url,
    agent: &AgentInfo,
    secret: Option<&str>,
) -> Result<url::Url> {
    let mut sub_url = signal_url.clone();
    make_sub_url(&mut sub_url, Some(agent.name.as_str()), secret)?;
    if let Some(instance_id) = agent.instance_id.as_deref() {
        sub_url
            .query_pairs_mut()
            .append_pair("instance_id", instance_id);
    }
    make_ws_scheme(&mut sub_url)?;
    Ok(sub_url)
}

fn make_quic_sub_url(
    signal_url: &url::Url,
    agent: &AgentInfo,
    secret: Option<&str>,
) -> Result<url::Url> {
    let mut sub_url = signal_url.clone();
    sub_url
        .query_pairs_mut()
        .append_pair("agent", agent.name.as_str());
    if let Some(instance_id) = agent.instance_id.as_deref() {
        sub_url
            .query_pairs_mut()
            .append_pair("instance_id", instance_id);
    }
    let token = token_gen(secret, Local::now().timestamp_millis() as u64)?;
    sub_url
        .query_pairs_mut()
        .append_pair("token", token.as_str());
    Ok(sub_url)
}

fn default_ice_servers() -> Vec<String> {
    vec![
        "stun:stun.miwifi.com:3478".into(),
        "stun:stun.chat.bilibili.com:3478".into(),
        "stun:stun.cloudflare.com:3478".into(),
        "stun:stun1.l.google.com:19302".into(),
        "stun:stun2.l.google.com:19302".into(),
        "stun:stun.qq.com:3478".into(),
    ]
}

fn encode_udp_relay_packet(
    buf: &mut [u8],
    flow_id: u64,
    payload: &[u8],
    codec: UdpRelayCodec,
) -> Result<usize> {
    if payload.len() > u16::MAX as usize {
        bail!("payload too large [{}]", payload.len());
    }
    let header_len = codec.header_len();
    let packet_len = header_len + payload.len();
    if packet_len > buf.len() {
        bail!("packet buffer too small [{}] < [{}]", buf.len(), packet_len);
    }

    let flow_id = flow_id & UDP_RELAY_FLOW_ID_MASK;
    if codec.is_obfs() {
        let nonce = rand::random::<u8>();
        let meta = (flow_id << 16) | payload.len() as u64;
        let obfs_meta = meta ^ udp_relay_obfs_mask(codec.obfs_seed, nonce);
        buf[0] = nonce;
        buf[1..9].copy_from_slice(&obfs_meta.to_be_bytes());
    } else {
        buf[..6].copy_from_slice(&flow_id.to_be_bytes()[2..]);
        buf[6..8].copy_from_slice(&(payload.len() as u16).to_be_bytes());
    }

    buf[header_len..packet_len].copy_from_slice(payload);
    Ok(packet_len)
}

fn decode_udp_relay_packet(packet: &[u8], codec: UdpRelayCodec) -> Result<(u64, &[u8])> {
    if codec.is_obfs() {
        if packet.len() < UDP_RELAY_META_LEN_OBFS {
            bail!(
                "packet as least [{}] but [{}]",
                UDP_RELAY_META_LEN_OBFS,
                packet.len()
            );
        }

        let nonce = packet[0];
        let mut meta_raw = [0_u8; 8];
        meta_raw.copy_from_slice(&packet[1..UDP_RELAY_META_LEN_OBFS]);
        let meta = u64::from_be_bytes(meta_raw) ^ udp_relay_obfs_mask(codec.obfs_seed, nonce);
        let flow_id = (meta >> 16) & UDP_RELAY_FLOW_ID_MASK;
        let len = (meta & 0xffff) as usize;
        if len > packet.len() - UDP_RELAY_META_LEN_OBFS {
            bail!(
                "meta.len [{}] exceed [{}]",
                len,
                packet.len() - UDP_RELAY_META_LEN_OBFS
            );
        }
        let payload = &packet[UDP_RELAY_META_LEN_OBFS..UDP_RELAY_META_LEN_OBFS + len];
        return Ok((flow_id, payload));
    }

    if packet.len() < UDP_RELAY_META_LEN_LEGACY {
        bail!(
            "packet as least [{}] but [{}]",
            UDP_RELAY_META_LEN_LEGACY,
            packet.len()
        );
    }

    let id = u64::from_be_bytes([
        0, 0, packet[0], packet[1], packet[2], packet[3], packet[4], packet[5],
    ]);
    let len = u16::from_be_bytes([packet[6], packet[7]]) as usize;
    if len > packet.len() - UDP_RELAY_META_LEN_LEGACY {
        bail!(
            "meta.len [{}] exceed [{}]",
            len,
            packet.len() - UDP_RELAY_META_LEN_LEGACY
        );
    }

    let payload = &packet[UDP_RELAY_META_LEN_LEGACY..UDP_RELAY_META_LEN_LEGACY + len];
    Ok((id, payload))
}

fn gen_udp_relay_obfs_seed() -> u32 {
    let seed = rand::random::<u32>();
    if seed == 0 {
        1
    } else {
        seed
    }
}

fn udp_relay_obfs_mask(seed: u32, nonce: u8) -> u64 {
    let mut z = ((seed as u64) << 8) | nonce as u64;
    z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

#[derive(Debug, Clone, Copy)]
struct UdpRelayCodec {
    obfs_seed: u32,
}

impl UdpRelayCodec {
    fn new(obfs_seed: u32) -> Self {
        Self { obfs_seed }
    }

    fn is_obfs(self) -> bool {
        self.obfs_seed != 0
    }

    fn header_len(self) -> usize {
        if self.is_obfs() {
            UDP_RELAY_META_LEN_OBFS
        } else {
            UDP_RELAY_META_LEN_LEGACY
        }
    }

    fn mode_name(self) -> &'static str {
        if self.is_obfs() {
            "obfs-v1"
        } else {
            "legacy"
        }
    }
}

#[derive(Debug, Clone)]
struct RelayWorker {
    signal_url: url::Url,
    secret: Option<String>,
    quic_insecure: bool,
    agent_regex: Regex,
    idle_timeout: Duration,
    max_payload: usize,
    rule: RelayRule,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelayProto {
    Udp,
}

#[derive(Debug, Clone, Copy)]
struct RelayRule {
    proto: RelayProto,
    listen: SocketAddr,
    target: SocketAddr,
}

impl RelayRule {
    fn parse(input: &str) -> Result<Self> {
        let raw = if input.contains("://") {
            input.to_string()
        } else {
            format!("udp://{input}")
        };

        let url = url::Url::parse(raw.as_str())
            .with_context(|| format!("invalid relay rule [{input}]"))?;

        let proto = if url.scheme().eq_ignore_ascii_case("udp") {
            RelayProto::Udp
        } else {
            bail!("unsupported relay protocol [{}]", url.scheme());
        };

        let listen_ip: IpAddr = url
            .host_str()
            .with_context(|| format!("missing listen host in [{input}]"))?
            .parse()
            .with_context(|| format!("listen host must be IP in [{input}]"))?;
        let listen_port = url
            .port()
            .with_context(|| format!("missing listen port in [{input}]"))?;
        let listen = SocketAddr::new(listen_ip, listen_port);

        let target_str = url
            .query_pairs()
            .find(|(k, _)| k.eq_ignore_ascii_case("to"))
            .map(|(_, v)| v.to_string())
            .with_context(|| format!("missing query `to` in [{input}]"))?;
        let target: SocketAddr = target_str
            .parse()
            .with_context(|| format!("target must be IP:PORT in [{input}]"))?;

        Ok(Self {
            proto,
            listen,
            target,
        })
    }

    fn is_udp(&self) -> bool {
        self.proto == RelayProto::Udp
    }
}

#[derive(Debug, Clone, Copy)]
struct ClientFlow {
    flow_id: u64,
    updated_at: Instant,
}

#[derive(Parser, Debug, Clone)]
#[clap(name = "relay", author, about, version)]
pub struct CmdArgs {
    #[clap(
        short = 'L',
        long = "local",
        long_help = "relay rule: [proto://]<listen_addr>?to=<target_addr>",
        required = true
    )]
    local_rules: Vec<String>,

    #[clap(help = "signal server url, eg. https://127.0.0.1:8888 or quic://127.0.0.1:8888")]
    url: String,

    #[clap(
        short = 'a',
        long = "agent",
        long_help = "agent name regex",
        default_value = ".*"
    )]
    agent: String,

    #[clap(long = "secret", long_help = "authentication secret")]
    secret: Option<String>,

    #[clap(
        long = "quic-insecure",
        long_help = "skip quic tls certificate verification (quic:// only)"
    )]
    quic_insecure: bool,

    #[clap(
        long = "udp-idle-timeout",
        long_help = "udp flow idle timeout in seconds",
        default_value_t = DEFAULT_UDP_IDLE_TIMEOUT_SECS,
    )]
    udp_idle_timeout: u64,

    #[clap(
        long = "udp-max-payload",
        long_help = "max udp payload in bytes (default auto from p2p limit)"
    )]
    udp_max_payload: Option<usize>,
}

#[cfg(test)]
mod tests {
    use super::{
        decode_udp_relay_packet, encode_udp_relay_packet, max_udp_payload_auto,
        resolve_udp_max_payload, RelayRule, UdpRelayCodec,
    };

    #[test]
    fn parse_rule_with_proto() {
        let rule = RelayRule::parse("udp://0.0.0.0:15354?to=8.8.4.4:53").unwrap();
        assert_eq!(rule.listen.to_string(), "0.0.0.0:15354");
        assert_eq!(rule.target.to_string(), "8.8.4.4:53");
    }

    #[test]
    fn parse_rule_default_udp() {
        let rule = RelayRule::parse("0.0.0.0:15353?to=8.8.8.8:53").unwrap();
        assert_eq!(rule.listen.to_string(), "0.0.0.0:15353");
        assert_eq!(rule.target.to_string(), "8.8.8.8:53");
    }

    #[test]
    fn parse_rule_invalid_target() {
        let r = RelayRule::parse("udp://0.0.0.0:15353?to=dns.google:53");
        assert!(r.is_err(), "{r:?}");
    }

    #[test]
    fn payload_limit_validation() {
        let auto = max_udp_payload_auto();
        assert_eq!(resolve_udp_max_payload(None).unwrap(), auto);
        assert_eq!(resolve_udp_max_payload(Some(auto)).unwrap(), auto);
        assert!(resolve_udp_max_payload(Some(auto + 1)).is_err());
    }

    #[test]
    fn relay_packet_codec_legacy() {
        let mut buf = vec![0_u8; 64];
        let codec = UdpRelayCodec::new(0);
        let n = encode_udp_relay_packet(&mut buf, 12345, b"hello", codec).unwrap();
        let (id, payload) = decode_udp_relay_packet(&buf[..n], codec).unwrap();
        assert_eq!(id, 12345);
        assert_eq!(payload, b"hello");
    }

    #[test]
    fn relay_packet_codec_obfs() {
        let mut buf = vec![0_u8; 64];
        let codec = UdpRelayCodec::new(123456789);
        let n = encode_udp_relay_packet(&mut buf, 12345, b"hello", codec).unwrap();
        let (id, payload) = decode_udp_relay_packet(&buf[..n], codec).unwrap();
        assert_eq!(id, 12345);
        assert_eq!(payload, b"hello");
    }
}
