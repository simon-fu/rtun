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
use tracing::{debug, info, warn};

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

    {
        let socket = socket.clone();
        let text = text.clone();
        let shared = shared.clone();

        tokio::spawn(async move {
            let r = recv_loop(socket, text.as_str(), &shared).await;
            info!("recv finished [{r:?}]");
        });
    }

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

    let _first = shared
        .first_connected_result(HardNatRole::Nat3, start_at)
        .with_context(|| "missing connected target")?;
    shared.send_conn(&text, interval).await
}

pub async fn run_nat4(args: Nat4RunConfig) -> Result<()> {
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

    for _ in 0..args.count {
        let listen = "0.0.0.0:0";
        let socket = UdpSocket::bind(listen)
            .await
            .with_context(|| format!("failed to bind socket addr [{}]", listen))?;

        let socket = Arc::new(socket);
        let local = socket.local_addr()?;

        {
            let socket = socket.clone();
            let text = text.clone();
            let shared = shared.clone();

            tokio::spawn(async move {
                let r = recv_loop(socket, text.as_str(), &shared).await;
                info!("recv finished [{r:?}]");
            });
        }

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

    let _first = shared
        .first_connected_result(HardNatRole::Nat4, start_at)
        .with_context(|| "missing connected target")?;
    shared.send_conn(&text, interval).await
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

    fn first_connected_result(
        &self,
        role: HardNatRole,
        start_at: Instant,
    ) -> Option<HardNatRunResult> {
        let from_addrs = self.connecteds.lock();
        let (connected_from, socket) = from_addrs.iter().next()?;
        let local_addr = socket.local_addr().ok()?;
        Some(HardNatRunResult {
            role,
            local_addr,
            connected_from: *connected_from,
            elapsed: start_at.elapsed(),
        })
    }

    async fn send_conn(&self, text: &str, interval: Duration) -> Result<()> {
        let from_addrs = { self.connecteds.lock().clone() };

        info!("connected targets {from_addrs:?}");

        let Some((target, socket)) = from_addrs.iter().next() else {
            return Ok(());
        };

        let local = socket
            .local_addr()
            .with_context(|| "get local address faield")?;
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
}
