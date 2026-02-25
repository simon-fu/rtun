use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{bail, Context as _, Result};
use futures::StreamExt;
use parking_lot::Mutex;
use rand::Rng as _;
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::{
    ice::{
        ice_candidate::{parse_candidate, CandidateKind},
        ice_peer::IceArgs,
    },
    proto::P2PHardNatArgs,
};

pub const DEFAULT_PROBE_TEXT: &str = "nat hello";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardNatRole {
    Nat3,
    Nat4,
}

impl HardNatRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Nat3 => "nat3",
            Self::Nat4 => "nat4",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardNatMode {
    Off,
    Fallback,
    Assist,
    Force,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardNatRoleHint {
    Auto,
    Nat3,
    Nat4,
}

impl HardNatRoleHint {
    pub fn from_proto_u32(v: u32) -> Self {
        match v {
            1 => Self::Nat3,
            2 => Self::Nat4,
            _ => Self::Auto,
        }
    }

    pub fn from_proto_args(args: &P2PHardNatArgs) -> Self {
        Self::from_proto_u32(args.role)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HardNatRolePlan {
    pub initiator_role: HardNatRole,
    pub responder_role: HardNatRole,
    pub local_role: HardNatRole,
    pub remote_role: HardNatRole,
}

pub fn resolve_role_plan(role_hint: HardNatRoleHint, is_initiator: bool) -> HardNatRolePlan {
    // For `auto`, default to initiator=nat3 and responder=nat4.
    let initiator_role = match role_hint {
        HardNatRoleHint::Auto | HardNatRoleHint::Nat3 => HardNatRole::Nat3,
        HardNatRoleHint::Nat4 => HardNatRole::Nat4,
    };
    let responder_role = match initiator_role {
        HardNatRole::Nat3 => HardNatRole::Nat4,
        HardNatRole::Nat4 => HardNatRole::Nat3,
    };

    let (local_role, remote_role) = if is_initiator {
        (initiator_role, responder_role)
    } else {
        (responder_role, initiator_role)
    };

    HardNatRolePlan {
        initiator_role,
        responder_role,
        local_role,
        remote_role,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HardNatTargetPlan {
    pub nat3_target_ip: Option<IpAddr>,
    pub nat4_target: Option<SocketAddr>,
    pub parsed_candidates: usize,
    pub usable_udp_candidates: usize,
}

pub fn derive_target_plan_from_ice(remote: &IceArgs) -> HardNatTargetPlan {
    let mut parsed_candidates = 0usize;
    let mut usable = Vec::new();

    for c in &remote.candidates {
        let Ok(cand) = parse_candidate(c) else {
            continue;
        };
        parsed_candidates += 1;
        if !cand.proto().eq_ignore_ascii_case("udp") {
            continue;
        }
        usable.push(cand);
    }

    usable.sort_by_key(|c| candidate_priority_key(c.kind(), c.addr()));
    let selected = usable.first().map(|c| c.addr());

    HardNatTargetPlan {
        nat3_target_ip: selected.map(|x| x.ip()),
        nat4_target: selected,
        parsed_candidates,
        usable_udp_candidates: usable.len(),
    }
}

fn candidate_priority_key(kind: CandidateKind, addr: SocketAddr) -> (u8, u8) {
    let public_rank = if is_public_ip(addr.ip()) { 0 } else { 1 };
    let kind_rank = match kind {
        CandidateKind::ServerReflexive | CandidateKind::PeerReflexive => 0,
        CandidateKind::Host => 1,
        CandidateKind::Relayed => 2,
    };
    (public_rank, kind_rank)
}

fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_unspecified())
        }
        IpAddr::V6(v6) => {
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || v6.is_unique_local()
                || v6.is_unicast_link_local())
        }
    }
}

#[derive(Debug, Clone)]
pub struct Nat3RunConfig {
    pub content: Option<String>,
    pub target_ip: IpAddr,
    pub count: usize,
    pub listen: String,
    pub ttl: Option<u32>,
    pub interval: Duration,
    pub batch_interval: Duration,
}

impl Nat3RunConfig {
    pub fn validate(&self) -> Result<()> {
        if self.count == 0 {
            bail!("count must be > 0");
        }
        if self.interval.is_zero() {
            bail!("interval must be > 0");
        }
        if self.batch_interval.is_zero() {
            bail!("batch_interval must be > 0");
        }
        if let Some(ttl) = self.ttl {
            if ttl == 0 {
                bail!("ttl must be > 0");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct Nat4RunConfig {
    pub content: Option<String>,
    pub target: SocketAddr,
    pub count: usize,
    pub ttl: Option<u32>,
    pub interval: Duration,
}

impl Nat4RunConfig {
    pub fn validate(&self) -> Result<()> {
        if self.count == 0 {
            bail!("count must be > 0");
        }
        if self.interval.is_zero() {
            bail!("interval must be > 0");
        }
        if let Some(ttl) = self.ttl {
            if ttl == 0 {
                bail!("ttl must be > 0");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct HardNatRunResult {
    pub role: HardNatRole,
    pub local_addr: SocketAddr,
    pub connected_from: SocketAddr,
    pub elapsed: Duration,
}

pub struct HardNatConnectedSocket {
    pub role: HardNatRole,
    pub socket: Arc<UdpSocket>,
    pub local_addr: SocketAddr,
    pub remote_addr: SocketAddr,
    pub elapsed: Duration,
}

impl HardNatConnectedSocket {
    pub fn as_result(&self) -> HardNatRunResult {
        HardNatRunResult {
            role: self.role,
            local_addr: self.local_addr,
            connected_from: self.remote_addr,
            elapsed: self.elapsed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardNatRunFailureReason {
    InvalidConfig,
    Timeout,
    PingFailed,
    SendFailed,
    RecvFailed,
    Canceled,
    Unknown,
}

#[derive(Debug, Clone)]
pub enum HardNatRunOutcome {
    Success(HardNatRunResult),
    Failed {
        role: HardNatRole,
        reason: HardNatRunFailureReason,
        elapsed: Duration,
    },
}

pub fn resolve_probe_text(content: Option<&str>) -> String {
    content.unwrap_or(DEFAULT_PROBE_TEXT).to_string()
}

pub fn encode_probe_token(role: HardNatRole, id: u64) -> String {
    format!("{}:{id:x}", role.as_str())
}

pub fn decode_probe_token(token: &str) -> Option<(HardNatRole, u64)> {
    let (role, id) = token.split_once(':')?;
    let role = match role {
        "nat3" => HardNatRole::Nat3,
        "nat4" => HardNatRole::Nat4,
        _ => return None,
    };
    let id = u64::from_str_radix(id, 16).ok()?;
    Some((role, id))
}

pub fn half_hops_from_ping_ttl(ttl: u32) -> Option<u32> {
    let hops = if ttl <= 64 {
        64 - ttl
    } else if ttl <= 128 {
        128 - ttl
    } else {
        return None;
    };

    let half = hops / 2;
    if half == 0 { None } else { Some(half) }
}

pub async fn run_nat3(args: Nat3RunConfig) -> Result<()> {
    let interval = args.interval;
    let text = resolve_probe_text(args.content.as_deref());
    let conn = run_nat3_once(args).await?;
    send_conn_loop(conn.socket, conn.remote_addr, &text, interval).await
}

pub async fn run_nat3_once(args: Nat3RunConfig) -> Result<HardNatConnectedSocket> {
    args.validate()?;

    let interval = args.interval;
    let batch_interval = args.batch_interval;
    let ttl = args.ttl;
    let target_ip = args.target_ip;
    let start_at = Instant::now();

    let socket = UdpSocket::bind(&args.listen)
        .await
        .with_context(|| format!("failed to bind socket addr [{}]", args.listen))?;

    if let Some(ttl) = ttl {
        socket.set_ttl(ttl).with_context(|| "set ttl failed")?;
        info!("set ttl [{ttl}]");
    }

    let socket = Arc::new(socket);
    let local = socket
        .local_addr()
        .with_context(|| "get local address failed")?;

    let text = Arc::new(resolve_probe_text(args.content.as_deref()));

    let shared = Arc::new(Shared {
        connecteds: Default::default(),
    });

    let recv_task = {
        let socket = socket.clone();
        let text = text.clone();
        let shared = shared.clone();

        tokio::spawn(async move {
            let r = recv_loop(socket, text.as_str(), &shared).await;
            info!("recv finished [{r:?}]");
        })
    };

    let mut has_recv = false;
    let mut num = 0_usize;
    let max_ports = 50000;
    let mut try_ports = HashSet::with_capacity(max_ports);

    while !has_recv {
        let start_time = Instant::now();
        let mut targets = Vec::with_capacity(args.count);

        for _ in 0..args.count {
            loop {
                let port = rand::thread_rng().gen_range(1024..=u16::MAX);

                if try_ports.len() >= max_ports {
                    try_ports.clear();
                }

                if !try_ports.contains(&port) {
                    try_ports.insert(port);
                    let target = SocketAddr::new(target_ip, port);
                    targets.push(target);
                    break;
                }
            }
            num += 1;
        }

        while start_time.elapsed() < batch_interval {
            for target in &targets {
                has_recv = shared.has_connected();
                if has_recv {
                    break;
                }

                let sent_bytes = socket
                    .send_to(text.as_bytes(), target)
                    .await
                    .with_context(|| "send failed")?;
                if sent_bytes == text.as_bytes().len() {
                    debug!("=> [{target}, {sent_bytes}]: [{text}]");
                } else {
                    warn!(
                        "No.{num}: sent partial {sent_bytes} < {}",
                        text.as_bytes().len()
                    );
                }
            }

            if has_recv {
                break;
            }

            info!("sent num [{num}]: [{local}] => [{target_ip}]");
            tokio::time::sleep(interval).await;
        }
    }

    let first = shared
        .first_connected_conn(HardNatRole::Nat3, start_at)
        .with_context(|| "missing connected target")?;
    abort_recv_tasks(vec![recv_task]).await;
    Ok(first)
}

pub async fn run_nat4(args: Nat4RunConfig) -> Result<()> {
    let interval = args.interval;
    let text = resolve_probe_text(args.content.as_deref());
    let conn = run_nat4_once(args).await?;
    send_conn_loop(conn.socket, conn.remote_addr, &text, interval).await
}

pub async fn run_nat4_once(args: Nat4RunConfig) -> Result<HardNatConnectedSocket> {
    args.validate()?;

    let target = args.target;
    let interval = args.interval;
    let start_at = Instant::now();

    let text = Arc::new(resolve_probe_text(args.content.as_deref()));

    let shared = Arc::new(Shared {
        connecteds: Default::default(),
    });

    let ttl = match args.ttl {
        Some(ttl) => Some(ttl),
        None => ping_and_half_hops(target.ip())
            .await
            .with_context(|| "ping_and_get_hops failed")?,
    };

    let mut senders = Vec::with_capacity(args.count);
    let mut recv_tasks = Vec::with_capacity(args.count);

    for _ in 0..args.count {
        let listen = "0.0.0.0:0";
        let socket = UdpSocket::bind(listen)
            .await
            .with_context(|| format!("failed to bind socket addr [{}]", listen))?;

        let socket = Arc::new(socket);
        let local = socket.local_addr()?;

        let recv_task = {
            let socket = socket.clone();
            let text = text.clone();
            let shared = shared.clone();

            tokio::spawn(async move {
                let r = recv_loop(socket, text.as_str(), &shared).await;
                info!("recv finished [{r:?}]");
            })
        };
        recv_tasks.push(recv_task);

        let sender = UdpSender {
            socket: socket.clone(),
            text: text.clone(),
            target,
            local,
        };

        if let Some(ttl) = ttl {
            sender
                .prepare_ttl(ttl)
                .await
                .with_context(|| "prepare_ttl failed")?;
        }

        senders.push(sender);
    }

    let mut has_recv = false;

    while !has_recv {
        for sender in &mut senders {
            if shared.has_connected() {
                has_recv = true;
                break;
            }

            sender.send_one().await?;
        }

        info!(
            "send target [{}], num [{}], ttl [{:?}]",
            target,
            senders.len(),
            ttl
        );

        tokio::time::sleep(interval).await;
    }

    let first = shared
        .first_connected_conn(HardNatRole::Nat4, start_at)
        .with_context(|| "missing connected target")?;
    abort_recv_tasks(recv_tasks).await;
    Ok(first)
}

async fn ping_and_half_hops(host: IpAddr) -> Result<Option<u32>> {
    let Some(ttl) = ping_host(host).await? else {
        return Ok(None);
    };

    info!("ping return ttl [{ttl}]");

    let Some(half) = half_hops_from_ping_ttl(ttl) else {
        return Ok(None);
    };

    info!("ping return half hops [{half}]");
    Ok(Some(half))
}

async fn ping_host(host: IpAddr) -> Result<Option<u32>> {
    info!("try ping host [{host}]...");

    let payload = [0; 8];

    let config = match host {
        IpAddr::V4(_) => surge_ping::Config::default(),
        IpAddr::V6(_) => surge_ping::Config::builder()
            .kind(surge_ping::ICMP::V6)
            .build(),
    };
    let client = surge_ping::Client::new(&config)?;

    let mut futs = futures::stream::FuturesUnordered::new();
    for seq in 0..3 {
        let client = client.clone();
        futs.push(async move {
            let mut pinger = client
                .pinger(host, surge_ping::PingIdentifier(rand::random()))
                .await;
            pinger.ping(surge_ping::PingSequence(seq), &payload).await
        });
    }

    while let Some(r) = futs.next().await {
        info!("ping result: {r:?}");
        match r {
            Ok((packet, _d)) => {
                let ttl = match packet {
                    surge_ping::IcmpPacket::V4(p) => p.get_ttl().map(|x| x as u32),
                    surge_ping::IcmpPacket::V6(p) => Some(p.get_max_hop_limit() as u32),
                };
                if let Some(ttl) = ttl {
                    return Ok(Some(ttl));
                }
            }
            Err(_) => {}
        }
    }

    Ok(None)
}

async fn recv_loop(socket: Arc<UdpSocket>, text: &str, shared: &Arc<Shared>) -> Result<()> {
    let local = socket
        .local_addr()
        .with_context(|| "get local address failed")?;

    let mut buf = vec![0_u8; 1700];
    loop {
        let (len, from) = socket
            .recv_from(&mut buf)
            .await
            .with_context(|| "recv_from failed")?;
        let packet = &buf[..len];
        if packet == text.as_bytes() {
            let old = shared.connecteds.lock().insert(from, socket.clone());
            info!("recv text [{local}] <= [{from}], text [{text}]");
            if old.is_none() {
                info!("connected from target [{from:?}]");
            }
        } else {
            info!("recv unknown [{local}] <= [{from}], bytes [{len}]");
        }
    }
}

async fn abort_recv_tasks(tasks: Vec<JoinHandle<()>>) {
    for task in tasks {
        task.abort();
        let _ = task.await;
    }
}

struct UdpSender {
    socket: Arc<UdpSocket>,
    target: SocketAddr,
    text: Arc<String>,
    local: SocketAddr,
}

impl UdpSender {
    async fn send_one(&self) -> Result<()> {
        let len = self
            .socket
            .send_to(self.text.as_bytes(), self.target)
            .await
            .with_context(|| "send_to failed")?;
        debug!(
            "sent to [{}] => [{}]: bytes [{len}]",
            self.local, self.target
        );
        Ok(())
    }

    async fn prepare_ttl(&self, max_ttl: u32) -> Result<()> {
        for ttl in 1..=max_ttl {
            self.socket
                .set_ttl(ttl)
                .with_context(|| format!("failed to set_ttl [{ttl}]"))?;
            self.send_one().await?;
        }
        Ok(())
    }
}

struct Shared {
    connecteds: Mutex<HashMap<SocketAddr, Arc<UdpSocket>>>,
}

impl Shared {
    fn has_connected(&self) -> bool {
        !self.connecteds.lock().is_empty()
    }

    fn first_connected_conn(
        &self,
        role: HardNatRole,
        start_at: Instant,
    ) -> Option<HardNatConnectedSocket> {
        let from_addrs = self.connecteds.lock();
        let (remote_addr, socket) = from_addrs.iter().next()?;
        let local_addr = socket.local_addr().ok()?;
        Some(HardNatConnectedSocket {
            role,
            socket: socket.clone(),
            local_addr,
            remote_addr: *remote_addr,
            elapsed: start_at.elapsed(),
        })
    }
}

async fn send_conn_loop(
    socket: Arc<UdpSocket>,
    target: SocketAddr,
    text: &str,
    interval: Duration,
) -> Result<()> {
    let local = socket
        .local_addr()
        .with_context(|| "get local address faield")?;
    info!("connected target selected [{local} => {target}]");
    socket.set_ttl(64).with_context(|| "set_ttl 64 failed")?;

    loop {
        let sent_bytes = socket
            .send_to(text.as_bytes(), target)
            .await
            .with_context(|| "send failed")?;

        info!("send conn: [{local} => {target}, {sent_bytes}]: [{text}]");

        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4};

    #[test]
    fn nat3_validate_rejects_zero_count() {
        let cfg = Nat3RunConfig {
            content: None,
            target_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            count: 0,
            listen: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)).to_string(),
            ttl: None,
            interval: Duration::from_millis(100),
            batch_interval: Duration::from_millis(1000),
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("count"));
    }

    #[test]
    fn nat4_validate_rejects_zero_interval() {
        let cfg = Nat4RunConfig {
            content: None,
            target: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 12345)),
            count: 4,
            ttl: None,
            interval: Duration::ZERO,
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("interval"));
    }

    #[test]
    fn probe_token_roundtrip() {
        let s = encode_probe_token(HardNatRole::Nat4, 0x1234_abcd);
        let (role, id) = decode_probe_token(&s).unwrap();
        assert_eq!(role, HardNatRole::Nat4);
        assert_eq!(id, 0x1234_abcd);
    }

    #[test]
    fn half_hops_from_ttl_matches_existing_logic() {
        assert_eq!(half_hops_from_ping_ttl(64), None);
        assert_eq!(half_hops_from_ping_ttl(63), None);
        assert_eq!(half_hops_from_ping_ttl(62), Some(1));
        assert_eq!(half_hops_from_ping_ttl(120), Some(4));
        assert_eq!(half_hops_from_ping_ttl(129), None);
    }

    #[test]
    fn resolve_probe_text_uses_default() {
        assert_eq!(resolve_probe_text(None), DEFAULT_PROBE_TEXT);
        assert_eq!(resolve_probe_text(Some("x")), "x");
    }

    #[test]
    fn resolve_role_plan_auto_defaults_to_initiator_nat3() {
        let p = resolve_role_plan(HardNatRoleHint::Auto, true);
        assert_eq!(p.initiator_role, HardNatRole::Nat3);
        assert_eq!(p.responder_role, HardNatRole::Nat4);
        assert_eq!(p.local_role, HardNatRole::Nat3);
        assert_eq!(p.remote_role, HardNatRole::Nat4);

        let p = resolve_role_plan(HardNatRoleHint::Auto, false);
        assert_eq!(p.local_role, HardNatRole::Nat4);
        assert_eq!(p.remote_role, HardNatRole::Nat3);
    }

    #[test]
    fn resolve_role_plan_respects_explicit_initiator_role_hint() {
        let p = resolve_role_plan(HardNatRoleHint::Nat4, true);
        assert_eq!(p.initiator_role, HardNatRole::Nat4);
        assert_eq!(p.responder_role, HardNatRole::Nat3);
        assert_eq!(p.local_role, HardNatRole::Nat4);

        let p = resolve_role_plan(HardNatRoleHint::Nat4, false);
        assert_eq!(p.local_role, HardNatRole::Nat3);
        assert_eq!(p.remote_role, HardNatRole::Nat4);
    }

    #[test]
    fn derive_target_plan_prefers_public_srflx_udp() {
        let args = IceArgs {
            ufrag: "u".into(),
            pwd: "p".into(),
            candidates: vec![
                "candidate:1 1 udp 2130706175 192.168.1.10 50000 typ host".into(),
                "candidate:2 1 tcp 2130706175 8.8.8.8 50001 typ host".into(),
                "candidate:3 1 udp 1694498559 114.249.237.39 65140 typ srflx raddr 0.0.0.0 rport 64271".into(),
            ],
        };

        let plan = derive_target_plan_from_ice(&args);
        assert_eq!(plan.parsed_candidates, 3);
        assert_eq!(plan.usable_udp_candidates, 2);
        assert_eq!(plan.nat3_target_ip, Some("114.249.237.39".parse().unwrap()));
        assert_eq!(plan.nat4_target, Some("114.249.237.39:65140".parse().unwrap()));
    }

    #[test]
    fn derive_target_plan_ignores_invalid_candidates_and_non_udp() {
        let args = IceArgs {
            ufrag: "u".into(),
            pwd: "p".into(),
            candidates: vec![
                "garbage".into(),
                "candidate:1 1 tcp 2130706175 203.0.113.10 40000 typ host".into(),
                "candidate:2 1 udp 2130706175 127.0.0.1 40001 typ host".into(),
            ],
        };

        let plan = derive_target_plan_from_ice(&args);
        assert_eq!(plan.parsed_candidates, 2);
        assert_eq!(plan.usable_udp_candidates, 1);
        assert_eq!(plan.nat4_target, Some("127.0.0.1:40001".parse().unwrap()));
        assert_eq!(plan.nat3_target_ip, Some("127.0.0.1".parse().unwrap()));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn first_connected_conn_returns_socket_handle_and_meta() -> Result<()> {
        let shared = Shared {
            connecteds: Default::default(),
        };
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let local_addr = socket.local_addr()?;
        let remote_addr: SocketAddr = "127.0.0.1:23456".parse()?;
        shared.connecteds.lock().insert(remote_addr, socket.clone());

        let start_at = Instant::now();
        let conn = shared
            .first_connected_conn(HardNatRole::Nat4, start_at)
            .with_context(|| "missing conn")?;

        assert_eq!(conn.role, HardNatRole::Nat4);
        assert_eq!(conn.local_addr, local_addr);
        assert_eq!(conn.remote_addr, remote_addr);
        assert_eq!(conn.socket.local_addr()?, local_addr);

        let meta = conn.as_result();
        assert_eq!(meta.connected_from, remote_addr);
        assert_eq!(meta.local_addr, local_addr);
        assert_eq!(meta.role, HardNatRole::Nat4);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_nat4_once_local_udp_echo_returns_reusable_socket() -> Result<()> {
        let echo_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let echo_addr = echo_socket.local_addr()?;
        let echo_task = {
            let echo_socket = echo_socket.clone();
            tokio::spawn(async move {
                let mut buf = [0_u8; 2048];
                loop {
                    let (len, from) = match echo_socket.recv_from(&mut buf).await {
                        Ok(v) => v,
                        Err(_) => return,
                    };
                    let _ = echo_socket.send_to(&buf[..len], from).await;
                }
            })
        };

        let conn = tokio::time::timeout(
            Duration::from_secs(3),
            run_nat4_once(Nat4RunConfig {
                content: None,
                target: echo_addr,
                count: 1,
                ttl: Some(1), // avoid ping dependency in tests
                interval: Duration::from_millis(20),
            }),
        )
        .await
        .with_context(|| "run_nat4_once timeout")??;

        assert_eq!(conn.role, HardNatRole::Nat4);
        assert_eq!(conn.remote_addr, echo_addr);
        assert_eq!(conn.socket.local_addr()?, conn.local_addr);

        // Drain any queued "nat hello" echoes from probing, then verify a fresh payload
        // is still receivable by the caller (i.e. probe recv task is not stealing packets).
        let mut drain_buf = [0_u8; 2048];
        loop {
            match tokio::time::timeout(
                Duration::from_millis(20),
                conn.socket.recv_from(&mut drain_buf),
            )
            .await
            {
                Ok(Ok((_len, _from))) => {}
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => break,
            }
        }

        let payload = b"after-return";
        conn.socket.send_to(payload, echo_addr).await?;
        let mut buf = [0_u8; 128];
        let (len, from) = tokio::time::timeout(Duration::from_millis(300), conn.socket.recv_from(&mut buf))
            .await
            .with_context(|| "recv after run_nat4_once return timed out (possible probe recv task still reading)")??;
        assert_eq!(from, echo_addr);
        assert_eq!(&buf[..len], payload);

        echo_task.abort();
        let _ = echo_task.await;
        Ok(())
    }
}
