use std::{
    collections::{HashMap, HashSet},
    fmt::Write as FmtWrite,
    future::Future,
    net::{IpAddr, SocketAddr},
    os::fd::AsRawFd,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use anyhow::{bail, Context as _, Result};
use futures::{stream::FuturesUnordered, StreamExt};
use nix::poll::{poll, PollFd, PollFlags};
use parking_lot::Mutex;
use rand::Rng as _;
use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::{
    ice::{
        ice_candidate::{parse_candidate, CandidateKind},
        ice_peer::{default_ice_servers, IceArgs},
    },
    proto::{
        hard_nat_control_envelope, HardNatAbort, HardNatAck, HardNatAdvanceNat3Addr,
        HardNatAdvanceNat4Ip, HardNatConnected, HardNatControlEnvelope, HardNatLeaseKeepAlive,
        HardNatNextBatch, HardNatStartBatch, P2PHardNatArgs,
    },
    stun::{
        async_udp::{tokio_socket_bind, AsyncUdpSocket, TokioUdpSocket},
        stun::{
            detect_nat_type3, detect_nat_type3_with_recovery, BindingOutput, Config as StunConfig,
            NatType,
        },
    },
};

pub const DEFAULT_PROBE_TEXT: &str = "nat hello";
pub const HARD_NAT_MAX_SOCKET_COUNT: u32 = 1024;
pub const HARD_NAT_MAX_SCAN_COUNT: u32 = 4096;
pub const HARD_NAT_MAX_INTERVAL_MS: u32 = 60_000;
pub const HARD_NAT_MAX_BATCH_INTERVAL_MS: u32 = 300_000;
pub const HARD_NAT_MAX_ASSIST_DELAY_MS: u32 = 10_000;
pub const HARD_NAT_MAX_TTL: u32 = 255;
pub const HARD_NAT_PROTO_VERSION: u32 = 1;
pub const HARD_NAT_DEFAULT_CONNECTED_TTL: u32 = 64;
pub const HARD_NAT_DEFAULT_LEASE_TIMEOUT_MS: u32 = 10_000;
pub const HARD_NAT_DEFAULT_KEEPALIVE_INTERVAL_MS: u32 = 1_000;
pub const HARD_NAT_DEFAULT_IP_TRY_TIMEOUT_MS: u32 = 1_000;
#[cfg(test)]
const HARD_NAT_KEEP_RECV_PROMOTED_TTL: u32 = 64;
static NEXT_HARD_NAT_SESSION_ID: AtomicU64 = AtomicU64::new(1);
pub const HARD_NAT_MANUAL_CONVERGE_DEFAULT_WARM_DRAIN_MS: u64 = 300;
pub const HARD_NAT_MANUAL_CONVERGE_DEFAULT_PROBING_WINDOW_MS: u64 = 1_500;
pub const HARD_NAT_MANUAL_CONVERGE_DEFAULT_PROBE_HIT_N1: u32 = 2;
pub const HARD_NAT_MANUAL_CONVERGE_DEFAULT_SILENT_BARRIER_MS: u64 = 500;
pub const HARD_NAT_MANUAL_CONVERGE_DEFAULT_QUIET_DRAIN_MS: u64 = 300;
pub const HARD_NAT_MANUAL_CONVERGE_DEFAULT_VALIDATION_HIT_N2: u32 = 2;
pub const HARD_NAT_MANUAL_CONVERGE_DEFAULT_VALIDATION_WINDOW_MS: u64 = 2_500;
pub const HARD_NAT_MANUAL_CONVERGE_DEFAULT_COOLDOWN_MS: u64 = 1_000;
pub const HARD_NAT_UDP_JSON_VERSION: &str = "hn1";
pub const HARD_NAT_NAT4_CANDIDATE_SAMPLE_SOCKET_COUNT: usize = 8;
pub const HARD_NAT_NAT4_CANDIDATE_SAMPLE_TIMEOUT_SECS: u64 = 5;
const NAT3_STUN_TRANSACTION_TIMEOUT: Duration = Duration::from_millis(800);
const NAT3_PAUSE_AFTER_DISCOVERY_PROMPT: &str =
    "nat3 discovery finished, press Enter to start probing";
const NAT3_HOLD_BATCH_UNTIL_ENTER_PROMPT: &str =
    "nat3 batch probing active, press Enter to reroll target ports";
const NAT3_HOLD_BATCH_STDIN_POLL_TIMEOUT_MS: i32 = 50;
const NAT3_HOLD_BATCH_STDIN_EOF_RETRY: Duration = Duration::from_millis(10);
const NAT3_HOLD_BATCH_STDIN_ERROR_RETRY: Duration = Duration::from_millis(50);

fn allocate_hard_nat_session_id() -> u64 {
    loop {
        let session_id = NEXT_HARD_NAT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
        if session_id != 0 {
            return session_id;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
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
pub struct ProbeToken {
    pub role: HardNatRole,
    pub socket_id: u64,
    pub generation: u64,
    pub seq: u64,
}

pub fn encode_probe_token(token: ProbeToken) -> String {
    format!(
        "hn1 {} {:x} {:x} {:x}",
        token.role.as_str(),
        token.socket_id,
        token.generation,
        token.seq
    )
}

pub fn decode_probe_token(token: &str) -> Option<ProbeToken> {
    let mut parts = token.split_ascii_whitespace();
    if parts.next()? != "hn1" {
        return None;
    }

    let role = match parts.next()? {
        "nat3" => HardNatRole::Nat3,
        "nat4" => HardNatRole::Nat4,
        _ => return None,
    };
    let socket_id = u64::from_str_radix(parts.next()?, 16).ok()?;
    let generation = u64::from_str_radix(parts.next()?, 16).ok()?;
    let seq = u64::from_str_radix(parts.next()?, 16).ok()?;
    if parts.next().is_some() {
        return None;
    }

    Some(ProbeToken {
        role,
        socket_id,
        generation,
        seq,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum HardNatUdpPacketType {
    ProbeReq,
    ProbeAck,
    HandshakeReq,
    HandshakeAck,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum HardNatHandshakeStage {
    PreCandidate,
    Candidate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct HardNatUdpSender {
    role: HardNatRole,
    socket_id: u64,
    generation: u64,
    seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct HardNatTuple {
    nat3_addr: SocketAddr,
    nat4_addr: SocketAddr,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct HardNatUdpPacket {
    v: String,
    packet_type: HardNatUdpPacketType,
    session_id: u64,
    sender: HardNatUdpSender,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    handshake_stage: Option<HardNatHandshakeStage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ack_of: Option<HardNatUdpSender>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tuple: Option<HardNatTuple>,
}

impl HardNatUdpPacket {
    fn validate(&self) -> Result<()> {
        if self.v != HARD_NAT_UDP_JSON_VERSION {
            bail!("unsupported hard nat udp json version [{}]", self.v);
        }
        match self.packet_type {
            HardNatUdpPacketType::ProbeReq | HardNatUdpPacketType::ProbeAck => {
                if self.handshake_stage.is_some() || self.ack_of.is_some() || self.tuple.is_some() {
                    bail!("probe packet must not carry handshake fields");
                }
            }
            HardNatUdpPacketType::HandshakeReq => {
                if self.handshake_stage.is_none() {
                    bail!("handshake_req missing handshake_stage");
                }
                if self.ack_of.is_some() || self.tuple.is_some() {
                    bail!("handshake_req must not carry ack_of/tuple");
                }
            }
            HardNatUdpPacketType::HandshakeAck => {
                if self.handshake_stage.is_none() {
                    bail!("handshake_ack missing handshake_stage");
                }
                if self.ack_of.is_none() || self.tuple.is_none() {
                    bail!("handshake_ack missing ack_of or tuple");
                }
            }
        }
        Ok(())
    }
}

// Phase 1 only wires decode/classification into runtime path; encode is used by tests
// and upcoming nat3/nat4 JSON send-path work.
#[allow(dead_code)]
fn encode_hard_nat_udp_packet(packet: &HardNatUdpPacket) -> Result<String> {
    packet.validate()?;
    serde_json::to_string(packet).with_context(|| "serialize hard nat udp json packet failed")
}

fn decode_hard_nat_udp_packet(raw: &str) -> Result<HardNatUdpPacket> {
    let packet: HardNatUdpPacket =
        serde_json::from_str(raw).with_context(|| "parse hard nat udp json packet failed")?;
    packet.validate()?;
    Ok(packet)
}

fn build_hard_nat_udp_sender(
    role: HardNatRole,
    socket_id: u64,
    generation: u64,
    seq: u64,
) -> HardNatUdpSender {
    HardNatUdpSender {
        role,
        socket_id,
        generation,
        seq,
    }
}

fn build_hard_nat_probe_req_packet(session_id: u64, seq: u64) -> HardNatUdpPacket {
    HardNatUdpPacket {
        v: HARD_NAT_UDP_JSON_VERSION.to_string(),
        packet_type: HardNatUdpPacketType::ProbeReq,
        session_id,
        sender: build_hard_nat_udp_sender(HardNatRole::Nat3, 0, 0, seq),
        handshake_stage: None,
        ack_of: None,
        tuple: None,
    }
}

fn build_hard_nat_probe_ack_packet(session_id: u64, socket_id: u64, seq: u64) -> HardNatUdpPacket {
    HardNatUdpPacket {
        v: HARD_NAT_UDP_JSON_VERSION.to_string(),
        packet_type: HardNatUdpPacketType::ProbeAck,
        session_id,
        sender: build_hard_nat_udp_sender(HardNatRole::Nat4, socket_id, 0, seq),
        handshake_stage: None,
        ack_of: None,
        tuple: None,
    }
}

fn build_hard_nat_handshake_req_packet_from_token(
    session_id: u64,
    token: ProbeToken,
) -> HardNatUdpPacket {
    HardNatUdpPacket {
        v: HARD_NAT_UDP_JSON_VERSION.to_string(),
        packet_type: HardNatUdpPacketType::HandshakeReq,
        session_id,
        sender: build_hard_nat_udp_sender(
            HardNatRole::Nat4,
            token.socket_id,
            token.generation,
            token.seq,
        ),
        handshake_stage: Some(if token.generation == 0 {
            HardNatHandshakeStage::PreCandidate
        } else {
            HardNatHandshakeStage::Candidate
        }),
        ack_of: None,
        tuple: None,
    }
}

fn build_hard_nat_handshake_ack_packet(
    session_id: u64,
    seq: u64,
    stage: HardNatHandshakeStage,
    ack_of: HardNatUdpSender,
    nat3_addr: SocketAddr,
    nat4_addr: SocketAddr,
) -> HardNatUdpPacket {
    HardNatUdpPacket {
        v: HARD_NAT_UDP_JSON_VERSION.to_string(),
        packet_type: HardNatUdpPacketType::HandshakeAck,
        session_id,
        sender: build_hard_nat_udp_sender(HardNatRole::Nat3, 0, 0, seq),
        handshake_stage: Some(stage),
        ack_of: Some(ack_of),
        tuple: Some(HardNatTuple {
            nat3_addr,
            nat4_addr,
        }),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProbePacketKind {
    Content,
    Token(ProbeToken),
    Json(HardNatUdpPacket),
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RecvProbeAction {
    Ignore,
    AcceptContent,
    AcceptNat4TokenEcho(ProbeToken),
    EchoNat4Token(ProbeToken),
    AcceptJsonProbeAck(HardNatUdpPacket),
    ReplyJsonProbeAck {
        session_id: u64,
    },
    AcceptJsonHandshakeAck(HardNatUdpPacket),
    ReplyJsonHandshakeAck {
        session_id: u64,
        ack_of: HardNatUdpSender,
        handshake_stage: HardNatHandshakeStage,
    },
}

#[derive(Clone)]
enum JsonProbeMode {
    Disabled,
    Nat3ProbeOnly,
    Nat3HandshakeResponder {
        next_seq: Arc<AtomicU64>,
    },
    Nat4ProbeResponder {
        socket_id: u64,
        next_seq: Arc<AtomicU64>,
    },
    Nat4HandshakeResponder {
        socket_id: u64,
        next_seq: Arc<AtomicU64>,
    },
}

enum Nat3BatchSendMode {
    Plain,
    JsonProbe {
        session_id: u64,
        next_seq: AtomicU64,
    },
}

impl Nat3BatchSendMode {
    fn next_payload(&self, text: &str) -> Result<String> {
        match self {
            Self::Plain => Ok(text.to_string()),
            Self::JsonProbe {
                session_id,
                next_seq,
            } => encode_hard_nat_udp_packet(&build_hard_nat_probe_req_packet(
                *session_id,
                next_seq.fetch_add(1, Ordering::Relaxed),
            )),
        }
    }
}

fn nat3_default_send_mode(session_id: u64, debug_converge_lease: bool) -> Nat3BatchSendMode {
    if debug_converge_lease {
        Nat3BatchSendMode::JsonProbe {
            session_id,
            next_seq: AtomicU64::new(0),
        }
    } else {
        Nat3BatchSendMode::Plain
    }
}

fn format_packet_preview(packet: &[u8]) -> String {
    const MAX_PREVIEW_BYTES: usize = 8;
    let mut preview = String::new();
    for (idx, byte) in packet.iter().take(MAX_PREVIEW_BYTES).enumerate() {
        if idx > 0 {
            preview.push(' ');
        }
        write!(&mut preview, "{:02X}", byte).expect("write preview failed");
    }
    preview
}

fn packet_kind_label(kind: &ProbePacketKind) -> &'static str {
    match kind {
        ProbePacketKind::Content => "Content",
        ProbePacketKind::Token(_) => "Token",
        ProbePacketKind::Json(_) => "Json",
        ProbePacketKind::Unknown => "Unknown",
    }
}

fn log_debug_converge_recv(
    role: HardNatRole,
    classification: &ProbePacketKind,
    local: SocketAddr,
    from: SocketAddr,
    len: usize,
    action: &RecvProbeAction,
    packet: &[u8],
    expected_text: &str,
) {
    let classification_label = packet_kind_label(classification);
    let classification_msg = format!("classification [{classification_label}]");
    let decision_msg = format!("decision [{action:?}]");
    match classification {
        ProbePacketKind::Content => {
            info!(
                "manual converge recv content {classification_msg} {decision_msg} role [{role:?}] local [{local}] from [{from}] len [{len}] expected [{expected_text}]"
            );
        }
        ProbePacketKind::Token(token) => {
            info!(
                "manual converge recv token {classification_msg} {decision_msg} role [{role:?}] local [{local}] from [{from}] len [{len}] token [{token:?}]"
            );
        }
        ProbePacketKind::Json(packet) => {
            info!(
                "manual converge recv json {classification_msg} {decision_msg} role [{role:?}] local [{local}] from [{from}] len [{len}] packet_type [{:?}] session_id [{}] sender [{:?}]",
                packet.packet_type,
                packet.session_id,
                packet.sender,
            );
        }
        ProbePacketKind::Unknown => {
            let preview = format_packet_preview(packet);
            info!(
                "manual converge recv unknown {classification_msg} {decision_msg} role [{role:?}] local [{local}] from [{from}] len [{len}] preview [{preview}]"
            );
        }
    }
}

fn log_manual_converge_token_send(local: SocketAddr, target: SocketAddr, token: ProbeToken) {
    info!(
        "manual converge send token local [{local}] target [{target}] socket [{socket_id}] generation [{generation}] seq [{seq}]",
        socket_id = token.socket_id,
        generation = token.generation,
        seq = token.seq
    );
}

fn log_manual_converge_validation_failure(
    owner: u64,
    generation: u64,
    socket: &Nat4ProbeSocketState,
    cfg: &ManualConvergeConfig,
) {
    info!(
        "manual converge validation failed owner [{owner}] generation [{generation}] echo [{}/{}] last_sent [{:?}] last_matched [{:?}] validation_window [{:?}] cooldown [{:?}]",
        socket.validation_echo_count,
        cfg.validation_hit_n2,
        socket.last_validation_sent_seq,
        socket.last_validation_matched_seq,
        cfg.validation_window,
        cfg.cooldown,
    );
}

fn maybe_log_connected_from_target(
    should_record_connected: bool,
    old_was_none: bool,
    from: SocketAddr,
) {
    if should_record_connected && old_was_none {
        info!("connected from target [{from:?}]");
    }
}

impl RecvProbeAction {
    fn is_valid_probe(&self) -> bool {
        !matches!(self, Self::Ignore)
    }

    fn should_echo(&self) -> bool {
        matches!(self, Self::EchoNat4Token(_))
    }
}

fn classify_probe_packet(packet: &[u8], text: &str) -> ProbePacketKind {
    if packet == text.as_bytes() {
        return ProbePacketKind::Content;
    }

    let Some(packet_text) = std::str::from_utf8(packet).ok() else {
        return ProbePacketKind::Unknown;
    };

    if let Ok(packet) = decode_hard_nat_udp_packet(packet_text) {
        return ProbePacketKind::Json(packet);
    }

    decode_probe_token(packet_text)
        .map(ProbePacketKind::Token)
        .unwrap_or(ProbePacketKind::Unknown)
}

fn decide_recv_probe_action(
    role: HardNatRole,
    token_handshake_enabled: bool,
    json_probe_mode: &JsonProbeMode,
    kind: &ProbePacketKind,
) -> RecvProbeAction {
    match kind {
        ProbePacketKind::Content => RecvProbeAction::AcceptContent,
        ProbePacketKind::Token(token) if !token_handshake_enabled => RecvProbeAction::Ignore,
        ProbePacketKind::Token(token) => match (role, token.role) {
            (HardNatRole::Nat3, HardNatRole::Nat4) => RecvProbeAction::EchoNat4Token(*token),
            (HardNatRole::Nat4, HardNatRole::Nat4) => RecvProbeAction::AcceptNat4TokenEcho(*token),
            _ => RecvProbeAction::Ignore,
        },
        ProbePacketKind::Json(packet) => match (
            json_probe_mode,
            role,
            packet.packet_type,
            packet.sender.role,
        ) {
            (JsonProbeMode::Disabled, _, _, _) => RecvProbeAction::Ignore,
            (
                JsonProbeMode::Nat4ProbeResponder { .. }
                | JsonProbeMode::Nat4HandshakeResponder { .. },
                HardNatRole::Nat4,
                HardNatUdpPacketType::ProbeReq,
                HardNatRole::Nat3,
            ) => RecvProbeAction::ReplyJsonProbeAck {
                session_id: packet.session_id,
            },
            (
                JsonProbeMode::Nat3ProbeOnly | JsonProbeMode::Nat3HandshakeResponder { .. },
                HardNatRole::Nat3,
                HardNatUdpPacketType::ProbeAck,
                HardNatRole::Nat4,
            ) => RecvProbeAction::AcceptJsonProbeAck(packet.clone()),
            (
                JsonProbeMode::Nat4HandshakeResponder { .. },
                HardNatRole::Nat4,
                HardNatUdpPacketType::HandshakeAck,
                HardNatRole::Nat3,
            ) => RecvProbeAction::AcceptJsonHandshakeAck(packet.clone()),
            (
                JsonProbeMode::Nat3HandshakeResponder { .. },
                HardNatRole::Nat3,
                HardNatUdpPacketType::HandshakeReq,
                HardNatRole::Nat4,
            ) => {
                let Some(handshake_stage) = packet.handshake_stage else {
                    return RecvProbeAction::Ignore;
                };
                RecvProbeAction::ReplyJsonHandshakeAck {
                    session_id: packet.session_id,
                    ack_of: packet.sender,
                    handshake_stage,
                }
            }
            _ => RecvProbeAction::Ignore,
        },
        ProbePacketKind::Unknown => RecvProbeAction::Ignore,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManualConvergePhase {
    Warming,
    Probing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nat4SocketPhase {
    Warming,
    Probing,
    Candidate,
    PendingQuiet { generation: u64 },
    LeaseOwnerValidating { generation: u64 },
    Silent { generation: u64 },
    Cooldown,
    Connected { generation: u64 },
}

#[derive(Debug, Clone)]
pub struct ManualConvergeConfig {
    pub enabled: bool,
    pub interval: Duration,
    pub warm_drain: Duration,
    pub probe_hit_n1: u32,
    pub probe_window: Duration,
    pub validation_hit_n2: u32,
    pub validation_window: Duration,
    pub cooldown: Duration,
    pub silent_barrier: Duration,
    pub quiet_drain: Duration,
}

impl ManualConvergeConfig {
    pub fn disabled(interval: Duration) -> Self {
        Self {
            enabled: false,
            interval,
            warm_drain: Duration::from_millis(HARD_NAT_MANUAL_CONVERGE_DEFAULT_WARM_DRAIN_MS),
            probe_hit_n1: HARD_NAT_MANUAL_CONVERGE_DEFAULT_PROBE_HIT_N1,
            probe_window: Duration::from_millis(HARD_NAT_MANUAL_CONVERGE_DEFAULT_PROBING_WINDOW_MS)
                .max(interval.saturating_mul(3)),
            validation_hit_n2: HARD_NAT_MANUAL_CONVERGE_DEFAULT_VALIDATION_HIT_N2,
            validation_window: Duration::from_millis(
                HARD_NAT_MANUAL_CONVERGE_DEFAULT_VALIDATION_WINDOW_MS,
            )
            .max(interval.saturating_mul(3)),
            cooldown: Duration::from_millis(HARD_NAT_MANUAL_CONVERGE_DEFAULT_COOLDOWN_MS)
                .max(interval),
            silent_barrier: Duration::from_millis(
                HARD_NAT_MANUAL_CONVERGE_DEFAULT_SILENT_BARRIER_MS,
            )
            .min(interval),
            quiet_drain: Duration::from_millis(HARD_NAT_MANUAL_CONVERGE_DEFAULT_QUIET_DRAIN_MS)
                .min(interval),
        }
    }

    pub fn for_controlled(interval: Duration) -> Self {
        Self {
            enabled: true,
            interval,
            warm_drain: Duration::from_millis(HARD_NAT_MANUAL_CONVERGE_DEFAULT_WARM_DRAIN_MS)
                .min(interval),
            probe_hit_n1: HARD_NAT_MANUAL_CONVERGE_DEFAULT_PROBE_HIT_N1,
            probe_window: Duration::from_millis(HARD_NAT_MANUAL_CONVERGE_DEFAULT_PROBING_WINDOW_MS)
                .max(interval.saturating_mul(3)),
            validation_hit_n2: HARD_NAT_MANUAL_CONVERGE_DEFAULT_VALIDATION_HIT_N2,
            validation_window: Duration::from_millis(
                HARD_NAT_MANUAL_CONVERGE_DEFAULT_VALIDATION_WINDOW_MS,
            )
            .max(interval.saturating_mul(3)),
            cooldown: Duration::from_millis(HARD_NAT_MANUAL_CONVERGE_DEFAULT_COOLDOWN_MS)
                .max(interval),
            silent_barrier: Duration::from_millis(
                HARD_NAT_MANUAL_CONVERGE_DEFAULT_SILENT_BARRIER_MS,
            )
            .min(interval),
            quiet_drain: Duration::from_millis(HARD_NAT_MANUAL_CONVERGE_DEFAULT_QUIET_DRAIN_MS)
                .min(interval),
        }
    }

    pub fn for_debug_lease(interval: Duration) -> Self {
        Self::for_controlled(interval)
    }
}

#[derive(Debug, Clone)]
pub struct Nat4ProbeSocketState {
    pub socket_id: u64,
    pub phase: Nat4SocketPhase,
    pub json_probe_session_id: Option<u64>,
    pub probe_hit_count: u32,
    pub probe_window_started_at: Option<Instant>,
    pub last_probe_hit_at: Option<Instant>,
    pub validating_started_at: Option<Instant>,
    pub validation_echo_count: u32,
    pub last_validation_sent_seq: Option<u64>,
    pub last_validation_matched_seq: Option<u64>,
    pub cooldown_until: Option<Instant>,
    pub next_seq: u64,
}

impl Nat4ProbeSocketState {
    fn new(socket_id: u64) -> Self {
        Self {
            socket_id,
            phase: Nat4SocketPhase::Warming,
            json_probe_session_id: None,
            probe_hit_count: 0,
            probe_window_started_at: None,
            last_probe_hit_at: None,
            validating_started_at: None,
            validation_echo_count: 0,
            last_validation_sent_seq: None,
            last_validation_matched_seq: None,
            cooldown_until: None,
            next_seq: 0,
        }
    }

    fn reset_probe_window(&mut self) {
        self.probe_hit_count = 0;
        self.probe_window_started_at = None;
        self.last_probe_hit_at = None;
    }

    fn reset_validation_state(&mut self) {
        self.validating_started_at = None;
        self.validation_echo_count = 0;
        self.last_validation_sent_seq = None;
        self.last_validation_matched_seq = None;
    }
}

#[derive(Debug)]
pub struct ManualConvergeCoordinator {
    phase: ManualConvergePhase,
    total_sockets: usize,
    cfg: ManualConvergeConfig,
    warm_ready: HashSet<u64>,
    warm_ready_at: Option<Instant>,
    sockets: HashMap<u64, Nat4ProbeSocketState>,
    current_generation: u64,
    lease_owner: Option<u64>,
    quiet_expected: HashSet<u64>,
    quiet_ready: HashSet<u64>,
    quiet_started_at: Option<Instant>,
    quiet_completed_at: Option<Instant>,
}

impl ManualConvergeCoordinator {
    pub fn new(total_sockets: usize, interval: Duration, warm_drain: Duration) -> Self {
        Self {
            phase: ManualConvergePhase::Warming,
            total_sockets,
            cfg: ManualConvergeConfig {
                enabled: true,
                interval,
                warm_drain,
                probe_hit_n1: HARD_NAT_MANUAL_CONVERGE_DEFAULT_PROBE_HIT_N1,
                probe_window: Duration::from_millis(
                    HARD_NAT_MANUAL_CONVERGE_DEFAULT_PROBING_WINDOW_MS,
                )
                .max(interval.saturating_mul(3)),
                validation_hit_n2: HARD_NAT_MANUAL_CONVERGE_DEFAULT_VALIDATION_HIT_N2,
                validation_window: Duration::from_millis(
                    HARD_NAT_MANUAL_CONVERGE_DEFAULT_VALIDATION_WINDOW_MS,
                )
                .max(interval.saturating_mul(3)),
                cooldown: Duration::from_millis(HARD_NAT_MANUAL_CONVERGE_DEFAULT_COOLDOWN_MS)
                    .max(interval),
                silent_barrier: Duration::from_millis(
                    HARD_NAT_MANUAL_CONVERGE_DEFAULT_SILENT_BARRIER_MS,
                )
                .min(interval),
                quiet_drain: Duration::from_millis(HARD_NAT_MANUAL_CONVERGE_DEFAULT_QUIET_DRAIN_MS)
                    .min(interval),
            },
            warm_ready: HashSet::with_capacity(total_sockets),
            warm_ready_at: None,
            sockets: HashMap::with_capacity(total_sockets),
            current_generation: 0,
            lease_owner: None,
            quiet_expected: HashSet::with_capacity(total_sockets.saturating_sub(1)),
            quiet_ready: HashSet::with_capacity(total_sockets.saturating_sub(1)),
            quiet_started_at: None,
            quiet_completed_at: None,
        }
    }

    pub fn phase(&self) -> ManualConvergePhase {
        self.phase
    }

    pub fn mark_warm_done(&mut self, socket_id: u64, now: Instant) -> Option<Instant> {
        self.ensure_socket(socket_id);
        self.warm_ready.insert(socket_id);
        if self.warm_ready.len() == self.total_sockets && self.warm_ready_at.is_none() {
            self.warm_ready_at = Some(now);
        }
        self.warm_ready_at
    }

    pub fn finish_warming(&mut self, now: Instant) -> bool {
        if self.phase == ManualConvergePhase::Probing {
            return true;
        }

        let Some(ready_at) = self.warm_ready_at else {
            return false;
        };
        if self.warm_ready.len() != self.total_sockets {
            return false;
        }
        if now.duration_since(ready_at) < self.cfg.warm_drain {
            return false;
        }

        self.phase = ManualConvergePhase::Probing;
        for socket in self.sockets.values_mut() {
            if socket.phase == Nat4SocketPhase::Warming {
                socket.phase = Nat4SocketPhase::Probing;
            }
        }
        true
    }

    pub fn record_probe_hit(&mut self, socket_id: u64, now: Instant) -> bool {
        let global_phase = self.phase;
        let cfg = self.cfg.clone();
        let socket = self.ensure_socket(socket_id);
        let phase_before = socket.phase;
        if global_phase != ManualConvergePhase::Probing {
            info!(
                "manual converge probe hit ignored: socket [{socket_id}], global_phase [{global_phase:?}], socket_phase [{phase_before:?}], probe_hit_count [{}]",
                socket.probe_hit_count
            );
            return false;
        }
        if !matches!(
            socket.phase,
            Nat4SocketPhase::Probing | Nat4SocketPhase::Candidate
        ) {
            info!(
                "manual converge probe hit ignored: socket [{socket_id}], global_phase [{global_phase:?}], socket_phase [{phase_before:?}], probe_hit_count [{}]",
                socket.probe_hit_count
            );
            return false;
        }

        let should_reset_window = socket
            .probe_window_started_at
            .map(|started_at| now.duration_since(started_at) > cfg.probe_window)
            .unwrap_or(true);
        if should_reset_window {
            socket.probe_hit_count = 0;
            socket.probe_window_started_at = Some(now);
        }

        let count_before = socket.probe_hit_count;
        socket.last_probe_hit_at = Some(now);
        socket.probe_hit_count += 1;
        let phase_after_hit = socket.phase;
        info!(
            "manual converge probe hit: socket [{socket_id}], global_phase [{global_phase:?}], socket_phase [{phase_before:?}] -> [{phase_after_hit:?}], probe_hit_count [{count_before} -> {}], window_reset [{should_reset_window}]",
            socket.probe_hit_count
        );
        if socket.phase == Nat4SocketPhase::Probing && socket.probe_hit_count >= cfg.probe_hit_n1 {
            socket.phase = Nat4SocketPhase::Candidate;
            debug!(
                "manual converge socket [{socket_id}] candidate ready: probe_hit_count [{}], probe_window [{:?}]",
                socket.probe_hit_count, cfg.probe_window
            );
        }
        true
    }

    pub fn socket_phase(&self, socket_id: u64) -> Option<Nat4SocketPhase> {
        self.sockets.get(&socket_id).map(|x| x.phase)
    }

    pub fn socket_probe_hit_count(&self, socket_id: u64) -> Option<u32> {
        self.sockets.get(&socket_id).map(|x| x.probe_hit_count)
    }

    pub fn record_json_probe_session_id(&mut self, socket_id: u64, session_id: u64) {
        let socket = self.ensure_socket(socket_id);
        socket.json_probe_session_id = Some(session_id);
    }

    pub fn json_probe_session_id(&self, socket_id: u64) -> Option<u64> {
        self.sockets
            .get(&socket_id)
            .and_then(|socket| socket.json_probe_session_id)
    }

    pub fn lease_owner(&self) -> Option<u64> {
        self.lease_owner
    }

    pub fn connected_socket_id(&self) -> Option<u64> {
        let owner = self.lease_owner?;
        matches!(
            self.socket_phase(owner),
            Some(Nat4SocketPhase::Connected { .. })
        )
        .then_some(owner)
    }

    pub fn connected_socket_candidate(&self) -> Option<(u64, u64)> {
        let owner = self.lease_owner?;
        match self.socket_phase(owner) {
            Some(Nat4SocketPhase::Connected { generation }) => Some((owner, generation)),
            _ => None,
        }
    }

    pub fn try_acquire_lease(&mut self, socket_id: u64, now: Instant) -> Option<u64> {
        if self.lease_owner.is_some() {
            return None;
        }
        if self.phase != ManualConvergePhase::Probing {
            return None;
        }
        if self.socket_phase(socket_id) != Some(Nat4SocketPhase::Candidate) {
            return None;
        }

        self.current_generation += 1;
        let generation = self.current_generation;
        self.lease_owner = Some(socket_id);
        self.quiet_expected.clear();
        self.quiet_ready.clear();
        self.quiet_started_at = Some(now);
        self.quiet_completed_at = None;

        for (id, socket) in self.sockets.iter_mut() {
            if *id == socket_id {
                socket.reset_validation_state();
                socket.phase = Nat4SocketPhase::PendingQuiet { generation };
            } else {
                socket.reset_validation_state();
                socket.phase = Nat4SocketPhase::Silent { generation };
                self.quiet_expected.insert(*id);
            }
        }

        if self.quiet_expected.is_empty() {
            self.quiet_completed_at = Some(now);
        }

        debug!(
            "manual converge lease granted: owner [{socket_id}], generation [{generation}], quiet_expected [{}]",
            self.quiet_expected.len()
        );
        Some(generation)
    }

    pub fn mark_silent_ready(&mut self, socket_id: u64, generation: u64, now: Instant) -> bool {
        if self.current_generation != generation {
            return false;
        }
        if self.lease_owner == Some(socket_id) {
            return false;
        }
        if self.socket_phase(socket_id) != Some(Nat4SocketPhase::Silent { generation }) {
            return false;
        }
        if !self.quiet_expected.contains(&socket_id) {
            return false;
        }

        let inserted = self.quiet_ready.insert(socket_id);
        if inserted {
            debug!(
                "manual converge silent ready: socket [{socket_id}], generation [{generation}], ready [{}/{}]",
                self.quiet_ready.len(),
                self.quiet_expected.len()
            );
        }
        if self.quiet_ready.len() == self.quiet_expected.len() && self.quiet_completed_at.is_none()
        {
            self.quiet_completed_at = Some(now);
            debug!(
                "manual converge quiet barrier ready: generation [{generation}], sockets [{}]",
                self.quiet_ready.len()
            );
        }
        inserted
    }

    pub fn advance_pending_quiet(&mut self, now: Instant) -> Option<u64> {
        let owner = self.lease_owner?;
        let generation = self.current_generation;
        if self.socket_phase(owner) == Some(Nat4SocketPhase::LeaseOwnerValidating { generation }) {
            return Some(generation);
        }
        if self.socket_phase(owner) != Some(Nat4SocketPhase::PendingQuiet { generation }) {
            return None;
        }

        if self.quiet_completed_at.is_none() {
            let quiet_started_at = self.quiet_started_at?;
            if now.duration_since(quiet_started_at) < self.cfg.silent_barrier {
                return None;
            }
            self.quiet_completed_at = Some(now);
            debug!(
                "manual converge quiet barrier timeout: generation [{generation}], ready [{}/{}]",
                self.quiet_ready.len(),
                self.quiet_expected.len()
            );
        }

        let quiet_completed_at = self.quiet_completed_at?;
        if now.duration_since(quiet_completed_at) < self.cfg.quiet_drain {
            return None;
        }

        if let Some(owner_socket) = self.sockets.get_mut(&owner) {
            owner_socket.phase = Nat4SocketPhase::LeaseOwnerValidating { generation };
            owner_socket.validating_started_at = Some(now);
            owner_socket.validation_echo_count = 0;
            owner_socket.last_validation_sent_seq = None;
            owner_socket.last_validation_matched_seq = None;
        }
        debug!(
            "manual converge owner enter validating: owner [{owner}], generation [{generation}]"
        );
        Some(generation)
    }

    pub fn record_validation_echo(
        &mut self,
        socket_id: u64,
        token: ProbeToken,
        now: Instant,
    ) -> bool {
        let cfg = self.cfg.clone();
        let socket = self.ensure_socket(socket_id);
        let generation = match socket.phase {
            Nat4SocketPhase::LeaseOwnerValidating { generation } => generation,
            _ => return false,
        };
        let Some(validating_started_at) = socket.validating_started_at else {
            return false;
        };
        if token.role != HardNatRole::Nat4
            || token.socket_id != socket_id
            || token.generation != generation
        {
            return false;
        }
        if now.duration_since(validating_started_at) > cfg.validation_window {
            return false;
        }
        let Some(last_validation_sent_seq) = socket.last_validation_sent_seq else {
            return false;
        };
        if token.seq > last_validation_sent_seq {
            return false;
        }
        if socket
            .last_validation_matched_seq
            .map(|last| token.seq <= last)
            .unwrap_or(false)
        {
            return false;
        }

        socket.last_validation_matched_seq = Some(token.seq);
        socket.validation_echo_count += 1;
        debug!(
            "manual converge validation echo matched: socket [{socket_id}], generation [{generation}], seq [{}], matched [{}/{}]",
            token.seq, socket.validation_echo_count, cfg.validation_hit_n2
        );
        if socket.validation_echo_count >= cfg.validation_hit_n2 {
            socket.phase = Nat4SocketPhase::Connected { generation };
            debug!(
                "manual converge final connected selected: owner [{socket_id}], generation [{generation}]"
            );
        }
        true
    }

    fn advance_runtime(&mut self, now: Instant) {
        self.advance_cooldowns(now);
        self.advance_validating(now);
        let _ = self.advance_pending_quiet(now);
    }

    fn advance_validating(&mut self, now: Instant) {
        let Some(owner) = self.lease_owner else {
            return;
        };
        let Some(socket) = self.sockets.get(&owner) else {
            return;
        };
        let generation = match socket.phase {
            Nat4SocketPhase::LeaseOwnerValidating { generation } => generation,
            Nat4SocketPhase::Connected { .. } => return,
            _ => return,
        };
        let Some(validating_started_at) = socket.validating_started_at else {
            return;
        };
        if now.duration_since(validating_started_at) < self.cfg.validation_window {
            return;
        }
        if socket.validation_echo_count >= self.cfg.validation_hit_n2 {
            return;
        }

        self.release_failed_lease(owner, generation, now);
    }

    fn advance_cooldowns(&mut self, now: Instant) {
        for socket in self.sockets.values_mut() {
            if socket.phase != Nat4SocketPhase::Cooldown {
                continue;
            }
            let Some(cooldown_until) = socket.cooldown_until else {
                continue;
            };
            if now < cooldown_until {
                continue;
            }

            socket.phase = Nat4SocketPhase::Probing;
            socket.cooldown_until = None;
            socket.reset_probe_window();
            socket.reset_validation_state();
            debug!(
                "manual converge cooldown expired: socket [{}]",
                socket.socket_id
            );
        }
    }

    fn release_failed_lease(&mut self, owner: u64, generation: u64, now: Instant) {
        self.lease_owner = None;
        self.quiet_expected.clear();
        self.quiet_ready.clear();
        self.quiet_started_at = None;
        self.quiet_completed_at = None;

        if let Some(owner_socket) = self.sockets.get(&owner) {
            log_manual_converge_validation_failure(owner, generation, owner_socket, &self.cfg);
        }

        for (socket_id, socket) in self.sockets.iter_mut() {
            socket.reset_validation_state();
            if *socket_id == owner {
                socket.phase = Nat4SocketPhase::Cooldown;
                socket.cooldown_until = Some(now + self.cfg.cooldown);
                socket.reset_probe_window();
            } else if matches!(
                socket.phase,
                Nat4SocketPhase::Silent { .. }
                    | Nat4SocketPhase::PendingQuiet { .. }
                    | Nat4SocketPhase::LeaseOwnerValidating { .. }
            ) {
                socket.phase = Nat4SocketPhase::Probing;
                socket.reset_probe_window();
            }
        }
        debug!(
            "manual converge owner validating failed: owner [{owner}], generation [{generation}], cooldown [{:?}]",
            self.cfg.cooldown
        );
    }

    fn next_send_token(&mut self, socket_id: u64) -> Option<ProbeToken> {
        let socket = self.ensure_socket(socket_id);
        let generation = match socket.phase {
            Nat4SocketPhase::Probing | Nat4SocketPhase::Candidate => 0,
            Nat4SocketPhase::LeaseOwnerValidating { generation }
            | Nat4SocketPhase::Connected { generation } => generation,
            _ => return None,
        };
        let seq = socket.next_seq;
        socket.next_seq += 1;
        if matches!(
            socket.phase,
            Nat4SocketPhase::LeaseOwnerValidating { .. } | Nat4SocketPhase::Connected { .. }
        ) {
            socket.last_validation_sent_seq = Some(seq);
        }

        Some(ProbeToken {
            role: HardNatRole::Nat4,
            socket_id,
            generation,
            seq,
        })
    }

    fn ensure_socket(&mut self, socket_id: u64) -> &mut Nat4ProbeSocketState {
        self.sockets
            .entry(socket_id)
            .or_insert_with(|| Nat4ProbeSocketState::new(socket_id))
    }
}

fn validate_manual_json_handshake_ack_packet(
    state: &ManualConvergeCoordinator,
    socket_id: u64,
    packet: &HardNatUdpPacket,
    local: SocketAddr,
    from: SocketAddr,
) -> Option<ProbeToken> {
    let ack_of = packet.ack_of?;
    if ack_of.role != HardNatRole::Nat4 || ack_of.socket_id != socket_id {
        return None;
    }

    let expected_session_id = state.json_probe_session_id(socket_id)?;
    if packet.session_id != expected_session_id {
        return None;
    }

    let expected_stage = if ack_of.generation == 0 {
        HardNatHandshakeStage::PreCandidate
    } else {
        HardNatHandshakeStage::Candidate
    };
    if packet.handshake_stage != Some(expected_stage) {
        return None;
    }

    let expected_tuple = HardNatTuple {
        nat3_addr: from,
        nat4_addr: local,
    };
    if packet.tuple.as_ref() != Some(&expected_tuple) {
        return None;
    }

    Some(ProbeToken {
        role: ack_of.role,
        socket_id: ack_of.socket_id,
        generation: ack_of.generation,
        seq: ack_of.seq,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Nat4SendPlan {
    socket_id: u64,
    should_send: bool,
    token: Option<ProbeToken>,
}

fn plan_nat4_manual_converge_send_step(
    coordinator: &mut ManualConvergeCoordinator,
    socket_ids: &[u64],
    now: Instant,
) -> Vec<Nat4SendPlan> {
    coordinator.advance_runtime(now);

    if coordinator.lease_owner().is_none() {
        for socket_id in socket_ids {
            if coordinator.socket_phase(*socket_id) == Some(Nat4SocketPhase::Candidate)
                && coordinator.try_acquire_lease(*socket_id, now).is_some()
            {
                break;
            }
        }
    }

    for socket_id in socket_ids {
        if let Some(Nat4SocketPhase::Silent { generation }) = coordinator.socket_phase(*socket_id) {
            let _ = coordinator.mark_silent_ready(*socket_id, generation, now);
        }
    }

    coordinator.advance_runtime(now);

    socket_ids
        .iter()
        .map(|socket_id| {
            let token = coordinator.next_send_token(*socket_id);
            Nat4SendPlan {
                socket_id: *socket_id,
                should_send: token.is_some(),
                token,
            }
        })
        .collect()
}

fn plan_nat4_controlled_converge_send_step(
    coordinator: &mut ManualConvergeCoordinator,
    socket_ids: &[u64],
    now: Instant,
) -> Vec<Nat4SendPlan> {
    coordinator.advance_runtime(now);

    if coordinator.lease_owner().is_none() {
        for socket_id in socket_ids {
            if coordinator.socket_phase(*socket_id) == Some(Nat4SocketPhase::Candidate)
                && coordinator.try_acquire_lease(*socket_id, now).is_some()
            {
                break;
            }
        }
    }

    for socket_id in socket_ids {
        if let Some(Nat4SocketPhase::Silent { generation }) = coordinator.socket_phase(*socket_id) {
            let _ = coordinator.mark_silent_ready(*socket_id, generation, now);
        }
    }

    coordinator.advance_runtime(now);

    socket_ids
        .iter()
        .map(|socket_id| {
            let should_send = match coordinator.socket_phase(*socket_id) {
                Some(Nat4SocketPhase::Probing) => {
                    coordinator.socket_probe_hit_count(*socket_id).unwrap_or(0) > 0
                }
                Some(Nat4SocketPhase::Candidate)
                | Some(Nat4SocketPhase::LeaseOwnerValidating { .. })
                | Some(Nat4SocketPhase::Connected { .. }) => true,
                _ => false,
            };
            let token = should_send
                .then(|| coordinator.next_send_token(*socket_id))
                .flatten();
            Nat4SendPlan {
                socket_id: *socket_id,
                should_send: token.is_some(),
                token,
            }
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssistWinner {
    Ice,
    HardNat,
}

impl AssistWinner {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ice => "ice",
            Self::HardNat => "hardnat",
        }
    }
}

#[derive(Debug)]
pub struct AssistRaceBothFailed<E> {
    pub ice_error: E,
    pub hard_nat_error: E,
}

pub async fn race_assist<T, E, FI, FH>(
    hard_nat_delay: Duration,
    ice_fut: FI,
    hard_nat_fut: FH,
) -> std::result::Result<(AssistWinner, T), AssistRaceBothFailed<E>>
where
    FI: Future<Output = std::result::Result<T, E>>,
    FH: Future<Output = std::result::Result<T, E>>,
{
    let delayed_hard_nat = async move {
        if !hard_nat_delay.is_zero() {
            tokio::time::sleep(hard_nat_delay).await;
        }
        hard_nat_fut.await
    };

    tokio::pin!(ice_fut);
    tokio::pin!(delayed_hard_nat);

    let mut ice_pending = true;
    let mut hard_nat_pending = true;
    let mut ice_error = None;
    let mut hard_nat_error = None;

    loop {
        tokio::select! {
            r = &mut ice_fut, if ice_pending => {
                ice_pending = false;
                match r {
                    Ok(v) => return Ok((AssistWinner::Ice, v)),
                    Err(e) => {
                        ice_error = Some(e);
                        if !hard_nat_pending {
                            return Err(AssistRaceBothFailed {
                                ice_error: ice_error.expect("ice error set"),
                                hard_nat_error: hard_nat_error.expect("hard-nat error set"),
                            });
                        }
                    }
                }
            }
            r = &mut delayed_hard_nat, if hard_nat_pending => {
                hard_nat_pending = false;
                match r {
                    Ok(v) => return Ok((AssistWinner::HardNat, v)),
                    Err(e) => {
                        hard_nat_error = Some(e);
                        if !ice_pending {
                            return Err(AssistRaceBothFailed {
                                ice_error: ice_error.expect("ice error set"),
                                hard_nat_error: hard_nat_error.expect("hard-nat error set"),
                            });
                        }
                    }
                }
            }
            else => unreachable!("assist race reached no-pending state without result"),
        }
    }
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

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HardNatSessionParams {
    pub proto_version: u32,
    pub session_id: u64,
    pub lease_timeout_ms: u32,
    pub keepalive_interval_ms: u32,
    pub batch_port_count: u32,
    pub ip_try_timeout_ms: u32,
    pub batch_timeout_ms: u32,
    pub connected_ttl: u32,
    pub nat4_candidate_ips: Vec<String>,
    pub nat3_public_addrs: Vec<String>,
}

impl HardNatSessionParams {
    pub fn placeholder_defaults() -> Self {
        Self {
            proto_version: HARD_NAT_PROTO_VERSION,
            connected_ttl: HARD_NAT_DEFAULT_CONNECTED_TTL,
            ..Default::default()
        }
    }

    pub fn from_proto(args: &P2PHardNatArgs) -> Self {
        Self {
            proto_version: args.proto_version,
            session_id: args.session_id,
            lease_timeout_ms: args.lease_timeout_ms,
            keepalive_interval_ms: args.keepalive_interval_ms,
            batch_port_count: args.batch_port_count,
            ip_try_timeout_ms: args.ip_try_timeout_ms,
            batch_timeout_ms: args.batch_timeout_ms,
            connected_ttl: args.connected_ttl,
            nat4_candidate_ips: args
                .nat4_candidate_ips
                .iter()
                .map(|value| value.to_string())
                .collect(),
            nat3_public_addrs: args
                .nat3_public_addrs
                .iter()
                .map(|value| value.to_string())
                .collect(),
        }
    }

    pub fn apply_defaults_if_missing(&mut self) {
        if self.proto_version == 0 {
            self.proto_version = HARD_NAT_PROTO_VERSION;
        }
        if self.connected_ttl == 0 {
            self.connected_ttl = HARD_NAT_DEFAULT_CONNECTED_TTL;
        }
    }

    pub fn with_batch_port_count(mut self, batch_port_count: u32) -> Self {
        if self.batch_port_count == 0 {
            self.batch_port_count = batch_port_count;
        }
        self
    }

    pub fn lease_timeout(&self) -> Duration {
        duration_from_ms_or_default(
            self.lease_timeout_ms,
            Duration::from_millis(HARD_NAT_DEFAULT_LEASE_TIMEOUT_MS as u64),
        )
    }

    pub fn keepalive_interval(&self) -> Duration {
        duration_from_ms_or_default(
            self.keepalive_interval_ms,
            Duration::from_millis(HARD_NAT_DEFAULT_KEEPALIVE_INTERVAL_MS as u64),
        )
    }

    pub fn ip_try_timeout(&self) -> Duration {
        duration_from_ms_or_default(
            self.ip_try_timeout_ms,
            Duration::from_millis(HARD_NAT_DEFAULT_IP_TRY_TIMEOUT_MS as u64),
        )
    }

    pub fn write_to_proto(&self, args: &mut P2PHardNatArgs) {
        args.proto_version = self.proto_version;
        args.session_id = self.session_id;
        args.lease_timeout_ms = self.lease_timeout_ms;
        args.keepalive_interval_ms = self.keepalive_interval_ms;
        args.batch_port_count = self.batch_port_count;
        args.ip_try_timeout_ms = self.ip_try_timeout_ms;
        args.batch_timeout_ms = self.batch_timeout_ms;
        args.connected_ttl = self.connected_ttl;
        args.nat4_candidate_ips = self
            .nat4_candidate_ips
            .iter()
            .cloned()
            .map(Into::into)
            .collect();
        args.nat3_public_addrs = self
            .nat3_public_addrs
            .iter()
            .cloned()
            .map(Into::into)
            .collect();
    }
}

pub fn apply_local_hard_nat_session_inputs(
    args: &mut P2PHardNatArgs,
    batch_port_count_hint: u32,
    local_ice: &IceArgs,
    local_nat3_public_addrs: &[SocketAddr],
) {
    let local_nat4_candidate_ips = collect_public_udp_candidate_ips_from_ice(local_ice);
    apply_local_hard_nat_session_candidates(
        args,
        batch_port_count_hint,
        &local_nat4_candidate_ips,
        local_nat3_public_addrs,
    );
}

pub fn apply_local_hard_nat_session_candidates(
    args: &mut P2PHardNatArgs,
    batch_port_count_hint: u32,
    local_nat4_candidate_ips: &[String],
    local_nat3_public_addrs: &[SocketAddr],
) {
    let mut session = HardNatSessionParams::from_proto(args);
    session.apply_defaults_if_missing();
    if session.session_id == 0 {
        session.session_id = allocate_hard_nat_session_id();
    }
    session = session.with_batch_port_count(batch_port_count_hint);
    session.nat4_candidate_ips = local_nat4_candidate_ips.to_vec();
    session.nat3_public_addrs = local_nat3_public_addrs
        .iter()
        .map(SocketAddr::to_string)
        .collect();
    session.write_to_proto(args);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HardNatBatchCursor {
    pub batch_id: u64,
    pub nat3_addr_index: u32,
    pub nat4_ip_index: u32,
    pub ports: Vec<u32>,
}

impl HardNatBatchCursor {
    fn from_start_batch(msg: &HardNatStartBatch) -> Self {
        Self {
            batch_id: msg.batch_id,
            nat3_addr_index: msg.nat3_addr_index,
            nat4_ip_index: msg.nat4_ip_index,
            ports: msg.ports.clone(),
        }
    }

    fn from_next_batch(msg: &HardNatNextBatch) -> Self {
        Self {
            batch_id: msg.next_batch_id,
            nat3_addr_index: msg.nat3_addr_index,
            nat4_ip_index: msg.nat4_ip_index,
            ports: msg.ports.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HardNatSchedulerConfig {
    pub session_id: u64,
    pub nat4_ip_count: usize,
    pub nat3_addr_count: usize,
    pub lease_timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HardNatSchedulerPhase {
    Init,
    ProbingBatch,
    Connected,
    Aborted,
    Expired,
}

#[derive(Debug, Clone, PartialEq)]
pub enum HardNatSchedulerAdvance {
    Send(HardNatControlEnvelope),
    NeedNextBatch { next_batch_id: u64 },
}

#[derive(Debug, Clone)]
pub struct HardNatScheduler {
    config: HardNatSchedulerConfig,
    phase: HardNatSchedulerPhase,
    cursor: Option<HardNatBatchCursor>,
    next_seq: u64,
    last_acked_seq: u64,
}

impl HardNatScheduler {
    pub fn new(config: HardNatSchedulerConfig) -> Self {
        Self {
            config: HardNatSchedulerConfig {
                nat4_ip_count: config.nat4_ip_count.max(1),
                nat3_addr_count: config.nat3_addr_count.max(1),
                ..config
            },
            phase: HardNatSchedulerPhase::Init,
            cursor: None,
            next_seq: 1,
            last_acked_seq: 0,
        }
    }

    pub fn phase(&self) -> HardNatSchedulerPhase {
        self.phase.clone()
    }

    pub fn start_batch(&mut self, ports: Vec<u32>) -> Result<HardNatControlEnvelope> {
        self.start_batch_inner(1, ports, false)
    }

    pub fn start_next_batch(&mut self, ports: Vec<u32>) -> Result<HardNatControlEnvelope> {
        let next_batch_id = self
            .cursor
            .as_ref()
            .map(|cursor| cursor.batch_id + 1)
            .unwrap_or(1);
        self.start_batch_inner(next_batch_id, ports, true)
    }

    pub fn advance_after_timeout(&mut self) -> Option<HardNatSchedulerAdvance> {
        if self.phase != HardNatSchedulerPhase::ProbingBatch {
            return None;
        }
        let msg = {
            let cursor = self.cursor.as_mut()?;
            if cursor.nat4_ip_index + 1 < self.config.nat4_ip_count as u32 {
                cursor.nat4_ip_index += 1;
                Some(hard_nat_control_envelope::Msg::AdvanceNat4Ip(
                    HardNatAdvanceNat4Ip {
                        batch_id: cursor.batch_id,
                        next_nat4_ip_index: cursor.nat4_ip_index,
                        ..Default::default()
                    },
                ))
            } else if cursor.nat3_addr_index + 1 < self.config.nat3_addr_count as u32 {
                cursor.nat3_addr_index += 1;
                cursor.nat4_ip_index = 0;
                Some(hard_nat_control_envelope::Msg::AdvanceNat3Addr(
                    HardNatAdvanceNat3Addr {
                        batch_id: cursor.batch_id,
                        next_nat3_addr_index: cursor.nat3_addr_index,
                        ..Default::default()
                    },
                ))
            } else {
                None
            }
        };

        if let Some(msg) = msg {
            return Some(HardNatSchedulerAdvance::Send(self.next_control(msg)));
        }

        let cursor = self.cursor.as_ref()?;
        Some(HardNatSchedulerAdvance::NeedNextBatch {
            next_batch_id: cursor.batch_id + 1,
        })
    }

    pub fn lease_keepalive(&mut self) -> HardNatControlEnvelope {
        self.next_control(hard_nat_control_envelope::Msg::LeaseKeepAlive(
            HardNatLeaseKeepAlive {
                lease_timeout_ms: duration_ms_u32(self.config.lease_timeout),
                ..Default::default()
            },
        ))
    }

    pub fn connected(
        &mut self,
        selected_nat3_addr: String,
        selected_nat4_ip: String,
        selected_port: u32,
        restore_ttl: u32,
        selected_socket_id: u64,
        selected_generation: u64,
    ) -> Result<HardNatControlEnvelope> {
        self.phase = HardNatSchedulerPhase::Connected;
        Ok(self.next_control(hard_nat_control_envelope::Msg::Connected(
            HardNatConnected {
                selected_nat3_addr: selected_nat3_addr.into(),
                selected_nat4_ip: selected_nat4_ip.into(),
                selected_port,
                restore_ttl,
                selected_socket_id,
                selected_generation,
                ..Default::default()
            },
        )))
    }

    pub fn abort(&mut self, reason: impl Into<String>) -> Result<HardNatControlEnvelope> {
        self.phase = HardNatSchedulerPhase::Aborted;
        Ok(
            self.next_control(hard_nat_control_envelope::Msg::Abort(HardNatAbort {
                reason: reason.into().into(),
                ..Default::default()
            })),
        )
    }

    pub fn apply_ack(&mut self, env: HardNatControlEnvelope) -> bool {
        if env.session_id != self.config.session_id {
            return false;
        }
        let Some(hard_nat_control_envelope::Msg::Ack(HardNatAck { acked_seq, .. })) = env.msg
        else {
            return false;
        };
        let last_sent_seq = self.next_seq.saturating_sub(1);
        if acked_seq == 0 || acked_seq > last_sent_seq || acked_seq <= self.last_acked_seq {
            return false;
        }
        self.last_acked_seq = acked_seq;
        true
    }

    fn start_batch_inner(
        &mut self,
        batch_id: u64,
        ports: Vec<u32>,
        is_next_batch: bool,
    ) -> Result<HardNatControlEnvelope> {
        let cursor = HardNatBatchCursor {
            batch_id,
            nat3_addr_index: 0,
            nat4_ip_index: 0,
            ports: ports.clone(),
        };
        self.cursor = Some(cursor);
        self.phase = HardNatSchedulerPhase::ProbingBatch;
        let msg = if is_next_batch {
            hard_nat_control_envelope::Msg::NextBatch(HardNatNextBatch {
                next_batch_id: batch_id,
                nat3_addr_index: 0,
                nat4_ip_index: 0,
                ports,
                ..Default::default()
            })
        } else {
            hard_nat_control_envelope::Msg::StartBatch(HardNatStartBatch {
                batch_id,
                nat3_addr_index: 0,
                nat4_ip_index: 0,
                ports,
                ..Default::default()
            })
        };
        Ok(self.next_control(msg))
    }

    fn next_control(&mut self, msg: hard_nat_control_envelope::Msg) -> HardNatControlEnvelope {
        let seq = self.next_seq;
        self.next_seq += 1;
        HardNatControlEnvelope {
            session_id: self.config.session_id,
            seq,
            role_from: hard_nat_role_proto_value(HardNatRole::Nat4),
            msg: Some(msg),
            ..Default::default()
        }
    }

    fn current_cursor(&self) -> Option<HardNatBatchCursor> {
        self.cursor.clone()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HardNatExecutorPhase {
    Idle,
    Leased,
    ExecutingBatch(HardNatBatchCursor),
    WaitingNextCommand(HardNatBatchCursor),
    Connected,
    Expired,
    Aborted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardNatControlApply {
    Applied,
    IgnoredSession,
    IgnoredSeq,
    IgnoredTerminal,
    IgnoredMessage,
}

#[derive(Debug, Clone)]
pub struct HardNatExecutor {
    session_id: u64,
    default_lease_timeout: Duration,
    last_seq: u64,
    lease_deadline: Option<Instant>,
    phase: HardNatExecutorPhase,
}

impl HardNatExecutor {
    pub fn new(session_id: u64, lease_timeout: Duration) -> Self {
        Self {
            session_id,
            default_lease_timeout: lease_timeout,
            last_seq: 0,
            lease_deadline: None,
            phase: HardNatExecutorPhase::Idle,
        }
    }

    pub fn phase(&self) -> HardNatExecutorPhase {
        self.phase.clone()
    }

    pub fn lease_deadline(&self) -> Option<Instant> {
        self.lease_deadline
    }

    pub fn mark_waiting_for_next_command(&mut self) {
        if let HardNatExecutorPhase::ExecutingBatch(cursor) = &self.phase {
            self.phase = HardNatExecutorPhase::WaitingNextCommand(cursor.clone());
        }
    }

    pub fn expire_if_needed(&mut self, now: Instant) -> bool {
        if matches!(
            self.phase,
            HardNatExecutorPhase::Connected
                | HardNatExecutorPhase::Expired
                | HardNatExecutorPhase::Aborted
        ) {
            return false;
        }

        let Some(deadline) = self.lease_deadline else {
            return false;
        };
        if now < deadline {
            return false;
        }

        self.phase = HardNatExecutorPhase::Expired;
        self.lease_deadline = None;
        true
    }

    pub fn apply_control(
        &mut self,
        env: HardNatControlEnvelope,
        now: Instant,
    ) -> HardNatControlApply {
        if env.session_id != self.session_id {
            return HardNatControlApply::IgnoredSession;
        }
        if env.seq <= self.last_seq {
            return HardNatControlApply::IgnoredSeq;
        }
        if matches!(
            self.phase,
            HardNatExecutorPhase::Connected
                | HardNatExecutorPhase::Expired
                | HardNatExecutorPhase::Aborted
        ) {
            return HardNatControlApply::IgnoredTerminal;
        }

        let Some(msg) = env.msg else {
            return HardNatControlApply::IgnoredMessage;
        };

        self.last_seq = env.seq;
        match msg {
            hard_nat_control_envelope::Msg::StartBatch(msg) => {
                self.phase = HardNatExecutorPhase::ExecutingBatch(
                    HardNatBatchCursor::from_start_batch(&msg),
                );
                self.refresh_lease_deadline(now, self.default_lease_timeout);
                HardNatControlApply::Applied
            }
            hard_nat_control_envelope::Msg::AdvanceNat4Ip(msg) => {
                let mut cursor = self.current_cursor().unwrap_or(HardNatBatchCursor {
                    batch_id: msg.batch_id,
                    nat3_addr_index: 0,
                    nat4_ip_index: 0,
                    ports: Vec::new(),
                });
                cursor.batch_id = msg.batch_id;
                cursor.nat4_ip_index = msg.next_nat4_ip_index;
                self.phase = HardNatExecutorPhase::ExecutingBatch(cursor);
                self.refresh_lease_deadline(now, self.default_lease_timeout);
                HardNatControlApply::Applied
            }
            hard_nat_control_envelope::Msg::AdvanceNat3Addr(msg) => {
                let mut cursor = self.current_cursor().unwrap_or(HardNatBatchCursor {
                    batch_id: msg.batch_id,
                    nat3_addr_index: 0,
                    nat4_ip_index: 0,
                    ports: Vec::new(),
                });
                cursor.batch_id = msg.batch_id;
                cursor.nat3_addr_index = msg.next_nat3_addr_index;
                cursor.nat4_ip_index = 0;
                self.phase = HardNatExecutorPhase::ExecutingBatch(cursor);
                self.refresh_lease_deadline(now, self.default_lease_timeout);
                HardNatControlApply::Applied
            }
            hard_nat_control_envelope::Msg::NextBatch(msg) => {
                self.phase =
                    HardNatExecutorPhase::ExecutingBatch(HardNatBatchCursor::from_next_batch(&msg));
                self.refresh_lease_deadline(now, self.default_lease_timeout);
                HardNatControlApply::Applied
            }
            hard_nat_control_envelope::Msg::LeaseKeepAlive(msg) => {
                let lease_timeout =
                    duration_from_ms_or_default(msg.lease_timeout_ms, self.default_lease_timeout);
                if self.phase == HardNatExecutorPhase::Idle {
                    self.phase = HardNatExecutorPhase::Leased;
                }
                self.refresh_lease_deadline(now, lease_timeout);
                HardNatControlApply::Applied
            }
            hard_nat_control_envelope::Msg::Connected(_) => {
                self.phase = HardNatExecutorPhase::Connected;
                self.lease_deadline = None;
                HardNatControlApply::Applied
            }
            hard_nat_control_envelope::Msg::Abort(_) => {
                self.phase = HardNatExecutorPhase::Aborted;
                self.lease_deadline = None;
                HardNatControlApply::Applied
            }
            hard_nat_control_envelope::Msg::Ack(_) => HardNatControlApply::IgnoredMessage,
        }
    }

    fn current_cursor(&self) -> Option<HardNatBatchCursor> {
        match &self.phase {
            HardNatExecutorPhase::ExecutingBatch(cursor)
            | HardNatExecutorPhase::WaitingNextCommand(cursor) => Some(cursor.clone()),
            _ => None,
        }
    }

    fn refresh_lease_deadline(&mut self, now: Instant, lease_timeout: Duration) {
        self.lease_deadline = Some(now + lease_timeout);
    }
}

fn duration_ms_u32(duration: Duration) -> u32 {
    duration.as_millis().min(u32::MAX as u128) as u32
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Nat3ControlledStateSnapshot {
    session_id: u64,
    phase: HardNatExecutorPhase,
    has_connected: bool,
    connected_count: usize,
}

fn nat3_controlled_state_snapshot(
    executor: &HardNatExecutor,
    shared: &Shared,
) -> Nat3ControlledStateSnapshot {
    Nat3ControlledStateSnapshot {
        session_id: executor.session_id,
        phase: executor.phase(),
        has_connected: shared.has_connected(),
        connected_count: shared.connected_count(),
    }
}

fn duration_from_ms_or_default(value: u32, default: Duration) -> Duration {
    if value == 0 {
        default
    } else {
        Duration::from_millis(value as u64)
    }
}

fn hard_nat_role_proto_value(role: HardNatRole) -> u32 {
    match role {
        HardNatRole::Nat3 => 1,
        HardNatRole::Nat4 => 2,
    }
}

fn hard_nat_executor_state_value(phase: HardNatExecutorPhase) -> u32 {
    match phase {
        HardNatExecutorPhase::Idle => 0,
        HardNatExecutorPhase::Leased => 1,
        HardNatExecutorPhase::ExecutingBatch(_) | HardNatExecutorPhase::WaitingNextCommand(_) => 2,
        HardNatExecutorPhase::Connected => 3,
        HardNatExecutorPhase::Expired => 4,
        HardNatExecutorPhase::Aborted => 5,
    }
}

fn build_hard_nat_ack(
    session_id: u64,
    seq: u64,
    acked_seq: u64,
    state: u32,
) -> HardNatControlEnvelope {
    HardNatControlEnvelope {
        session_id,
        seq,
        role_from: hard_nat_role_proto_value(HardNatRole::Nat3),
        msg: Some(hard_nat_control_envelope::Msg::Ack(HardNatAck {
            acked_seq,
            state,
            ..Default::default()
        })),
        ..Default::default()
    }
}

fn hard_nat_control_debug_label(env: &HardNatControlEnvelope) -> String {
    let msg = match env.msg.as_ref() {
        Some(hard_nat_control_envelope::Msg::StartBatch(msg)) => {
            format!(
                "StartBatch(batch_id={}, ports={})",
                msg.batch_id,
                msg.ports.len()
            )
        }
        Some(hard_nat_control_envelope::Msg::NextBatch(msg)) => {
            format!(
                "NextBatch(next_batch_id={}, nat3_addr_index={}, nat4_ip_index={}, ports={})",
                msg.next_batch_id,
                msg.nat3_addr_index,
                msg.nat4_ip_index,
                msg.ports.len()
            )
        }
        Some(hard_nat_control_envelope::Msg::LeaseKeepAlive(msg)) => {
            format!("LeaseKeepAlive(timeout_ms={})", msg.lease_timeout_ms)
        }
        Some(hard_nat_control_envelope::Msg::AdvanceNat4Ip(msg)) => {
            format!(
                "AdvanceNat4Ip(batch_id={}, next_index={})",
                msg.batch_id, msg.next_nat4_ip_index
            )
        }
        Some(hard_nat_control_envelope::Msg::AdvanceNat3Addr(msg)) => {
            format!(
                "AdvanceNat3Addr(batch_id={}, next_index={})",
                msg.batch_id, msg.next_nat3_addr_index
            )
        }
        Some(hard_nat_control_envelope::Msg::Connected(msg)) => format!(
            "Connected(nat3_addr={}, nat4_ip={}, port={}, restore_ttl={})",
            msg.selected_nat3_addr, msg.selected_nat4_ip, msg.selected_port, msg.restore_ttl
        ),
        Some(hard_nat_control_envelope::Msg::Abort(msg)) => {
            format!("Abort(reason={})", msg.reason)
        }
        Some(hard_nat_control_envelope::Msg::Ack(msg)) => {
            format!("Ack(acked_seq={}, state={})", msg.acked_seq, msg.state)
        }
        None => "None".to_string(),
    };
    format!(
        "session_id={}, seq={}, role_from={}, msg={}",
        env.session_id, env.seq, env.role_from, msg
    )
}

fn parse_hard_nat_connected_target(msg: &HardNatConnected) -> Option<SocketAddr> {
    let ip = msg.selected_nat4_ip.parse::<IpAddr>().ok()?;
    let port = u16::try_from(msg.selected_port).ok()?;
    Some(SocketAddr::new(ip, port))
}

fn parse_hard_nat_connected_nat3_addr(msg: &HardNatConnected) -> Option<SocketAddr> {
    msg.selected_nat3_addr.parse::<SocketAddr>().ok()
}

fn parse_hard_nat_connected_candidate(msg: &HardNatConnected) -> (u64, u64) {
    (msg.selected_socket_id, msg.selected_generation)
}

fn hard_nat_connected_nat3_addr_matches(
    msg: &HardNatConnected,
    local_addr: SocketAddr,
    session: &HardNatSessionParams,
) -> bool {
    let Ok(selected_nat3_addr) = msg.selected_nat3_addr.parse::<SocketAddr>() else {
        return false;
    };
    if selected_nat3_addr == local_addr {
        return true;
    }
    session
        .nat3_public_addrs
        .iter()
        .any(|addr| addr == &msg.selected_nat3_addr.to_string())
}

fn resolve_nat3_controlled_public_addr(
    session: &HardNatSessionParams,
    cursor: Option<&HardNatBatchCursor>,
) -> Result<Option<SocketAddr>> {
    if session.nat3_public_addrs.is_empty() {
        return Ok(None);
    }

    let nat3_addr_index = cursor.map(|cursor| cursor.nat3_addr_index).unwrap_or(0);
    let value = session
        .nat3_public_addrs
        .get(nat3_addr_index as usize)
        .with_context(|| {
            format!("hard-nat nat3 public addr index out of range [{nat3_addr_index}]")
        })?;
    let addr = value
        .parse()
        .with_context(|| format!("parse hard-nat nat3 public addr failed [{value}]"))?;
    Ok(Some(addr))
}

async fn recv_hard_nat_control(
    control_rx: &mut broadcast::Receiver<HardNatControlEnvelope>,
) -> Result<HardNatControlEnvelope> {
    loop {
        match control_rx.recv().await {
            Ok(env) => {
                debug!(
                    "hard_nat: received control {}",
                    hard_nat_control_debug_label(&env)
                );
                return Ok(env);
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                warn!("hard-nat control receiver lagged, skipped [{skipped}] messages");
            }
            Err(broadcast::error::RecvError::Closed) => {
                bail!("hard-nat control channel closed");
            }
        }
    }
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

pub fn derive_target_plan_from_ice_with_explicit_nat4_target(
    remote: &IceArgs,
    explicit_nat4_target: Option<SocketAddr>,
) -> HardNatTargetPlan {
    let mut plan = derive_target_plan_from_ice(remote);
    if let Some(target) = explicit_nat4_target {
        plan.nat3_target_ip = Some(target.ip());
        plan.nat4_target = Some(target);
    }
    plan
}

pub fn collect_udp_candidate_ips_from_ice(remote: &IceArgs) -> Vec<String> {
    let mut usable = Vec::new();

    for candidate in &remote.candidates {
        let Ok(candidate) = parse_candidate(candidate) else {
            continue;
        };
        if !candidate.proto().eq_ignore_ascii_case("udp") {
            continue;
        }
        usable.push(candidate);
    }

    usable.sort_by_key(|candidate| candidate_priority_key(candidate.kind(), candidate.addr()));

    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for candidate in usable {
        let ip = candidate.addr().ip().to_string();
        if seen.insert(ip.clone()) {
            out.push(ip);
        }
    }
    out
}

pub fn collect_public_udp_candidate_ips_from_ice(remote: &IceArgs) -> Vec<String> {
    collect_udp_candidate_ips_from_ice(remote)
        .into_iter()
        .filter(|value| value.parse::<IpAddr>().map(is_public_ip).unwrap_or(false))
        .collect()
}

pub fn merge_nat4_candidate_ips_from_sources(
    ice_candidates: &[String],
    sampled_addrs: &[SocketAddr],
) -> Vec<String> {
    let mut sampled_hit_counts = HashMap::<String, usize>::new();
    for addr in sampled_addrs {
        if !is_public_ip(addr.ip()) {
            continue;
        }
        *sampled_hit_counts
            .entry(addr.ip().to_string())
            .or_insert(0_usize) += 1;
    }

    let mut sampled_ranked = sampled_hit_counts.into_iter().collect::<Vec<_>>();
    sampled_ranked.sort_by(|(left_ip, left_hits), (right_ip, right_hits)| {
        right_hits
            .cmp(left_hits)
            .then_with(|| left_ip.cmp(right_ip))
    });

    let mut out = sampled_ranked
        .into_iter()
        .map(|(ip, _)| ip)
        .collect::<Vec<_>>();
    let mut seen = out.iter().cloned().collect::<HashSet<_>>();
    for value in ice_candidates {
        let Ok(ip) = value.parse::<IpAddr>() else {
            continue;
        };
        if !is_public_ip(ip) {
            continue;
        }
        if seen.insert(value.clone()) {
            out.push(value.clone());
        }
    }
    out
}

pub async fn collect_local_nat4_candidate_ips(local_ice: &IceArgs) -> Vec<String> {
    let ice_candidates = collect_public_udp_candidate_ips_from_ice(local_ice);
    let stun_servers = match resolve_nat3_stun_servers(true, &[]) {
        Ok(servers) => servers,
        Err(err) => {
            warn!("resolve nat4 candidate ip stun servers failed, fallback to ICE only: {err:#}");
            return ice_candidates;
        }
    };

    let sampled_addrs = match sample_nat4_candidate_public_addrs(
        HARD_NAT_NAT4_CANDIDATE_SAMPLE_SOCKET_COUNT,
        &stun_servers,
        Duration::from_secs(HARD_NAT_NAT4_CANDIDATE_SAMPLE_TIMEOUT_SECS),
    )
    .await
    {
        Ok(addrs) => addrs,
        Err(err) => {
            warn!("sample nat4 candidate ips failed, fallback to ICE only: {err:#}");
            return ice_candidates;
        }
    };

    let merged = merge_nat4_candidate_ips_from_sources(&ice_candidates, &sampled_addrs);
    debug!(
        "nat4 candidate ip sampling merged: ice_candidates={ice_candidates:?}, sampled_addrs={sampled_addrs:?}, merged={merged:?}"
    );
    merged
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
        IpAddr::V4(v4) => is_public_ipv4(v4),
        IpAddr::V6(v6) => is_public_ipv6(v6),
    }
}

fn is_public_ipv4(v4: std::net::Ipv4Addr) -> bool {
    if v4.is_private()
        || v4.is_loopback()
        || v4.is_link_local()
        || v4.is_broadcast()
        || v4.is_multicast()
        || v4.is_unspecified()
    {
        return false;
    }

    let [a, b, c, _d] = v4.octets();

    if a == 0 {
        return false;
    }
    if a == 100 && (64..=127).contains(&b) {
        return false;
    }
    if a == 192 && b == 0 && (c == 0 || c == 2) {
        return false;
    }
    if a == 198 && (b == 18 || b == 19) {
        return false;
    }
    if a == 198 && b == 51 && c == 100 {
        return false;
    }
    if a == 203 && b == 0 && c == 113 {
        return false;
    }
    if a >= 240 {
        return false;
    }

    true
}

fn is_public_ipv6(v6: std::net::Ipv6Addr) -> bool {
    if v6.is_loopback()
        || v6.is_unspecified()
        || v6.is_multicast()
        || v6.is_unique_local()
        || v6.is_unicast_link_local()
    {
        return false;
    }

    let segments = v6.segments();
    if segments[0] == 0x2001 && segments[1] == 0x0db8 {
        return false;
    }

    true
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
    pub discover_public_addr: bool,
    pub pause_after_discovery: bool,
    pub hold_batch_until_enter: bool,
    pub debug_converge_lease: bool,
    pub stun_servers: Vec<String>,
}

impl Nat3RunConfig {
    pub fn validate(&self) -> Result<()> {
        if self.count == 0 {
            bail!("count must be > 0");
        }
        if self.count > HARD_NAT_MAX_SCAN_COUNT as usize {
            bail!(
                "count too large: {} (max {})",
                self.count,
                HARD_NAT_MAX_SCAN_COUNT
            );
        }
        if self.interval.is_zero() {
            bail!("interval must be > 0");
        }
        if self.interval > Duration::from_millis(HARD_NAT_MAX_INTERVAL_MS as u64) {
            bail!(
                "interval too large: {}ms (max {}ms)",
                self.interval.as_millis(),
                HARD_NAT_MAX_INTERVAL_MS
            );
        }
        if self.batch_interval.is_zero() {
            bail!("batch_interval must be > 0");
        }
        if self.batch_interval > Duration::from_millis(HARD_NAT_MAX_BATCH_INTERVAL_MS as u64) {
            bail!(
                "batch_interval too large: {}ms (max {}ms)",
                self.batch_interval.as_millis(),
                HARD_NAT_MAX_BATCH_INTERVAL_MS
            );
        }
        if self.pause_after_discovery && !self.discover_public_addr {
            bail!("pause_after_discovery requires discover_public_addr");
        }
        if let Some(ttl) = self.ttl {
            if ttl == 0 {
                bail!("ttl must be > 0");
            }
            if ttl > HARD_NAT_MAX_TTL {
                bail!("ttl too large: {} (max {})", ttl, HARD_NAT_MAX_TTL);
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
    pub dump_public_addrs: bool,
    pub debug_keep_recv: bool,
    pub debug_promote_hit_ttl: Option<u32>,
    pub debug_converge_lease: bool,
}

impl Nat4RunConfig {
    pub fn validate(&self) -> Result<()> {
        if self.count == 0 {
            bail!("count must be > 0");
        }
        if self.count > HARD_NAT_MAX_SOCKET_COUNT as usize {
            bail!(
                "count too large: {} (max {})",
                self.count,
                HARD_NAT_MAX_SOCKET_COUNT
            );
        }
        if self.interval.is_zero() {
            bail!("interval must be > 0");
        }
        if self.interval > Duration::from_millis(HARD_NAT_MAX_INTERVAL_MS as u64) {
            bail!(
                "interval too large: {}ms (max {}ms)",
                self.interval.as_millis(),
                HARD_NAT_MAX_INTERVAL_MS
            );
        }
        if let Some(ttl) = self.ttl {
            if ttl == 0 {
                bail!("ttl must be > 0");
            }
            if ttl > HARD_NAT_MAX_TTL {
                bail!("ttl too large: {} (max {})", ttl, HARD_NAT_MAX_TTL);
            }
        }
        if let Some(ttl) = self.debug_promote_hit_ttl {
            if !self.debug_keep_recv {
                bail!("debug_promote_hit_ttl requires debug_keep_recv");
            }
            if ttl == 0 {
                bail!("debug_promote_hit_ttl must be > 0");
            }
            if ttl > HARD_NAT_MAX_TTL {
                bail!(
                    "debug_promote_hit_ttl too large: {} (max {})",
                    ttl,
                    HARD_NAT_MAX_TTL
                );
            }
        }
        if self.debug_converge_lease && !self.debug_keep_recv {
            bail!("debug_converge_lease requires debug_keep_recv");
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

fn resolve_nat3_stun_servers(
    discover_public_addr: bool,
    stun_servers: &[String],
) -> Result<Vec<String>> {
    if !discover_public_addr {
        return Ok(Vec::new());
    }

    let servers = if stun_servers.is_empty() {
        default_ice_servers()
    } else {
        stun_servers.to_vec()
    };

    let mut uniq = HashSet::new();
    let mut normalized = Vec::new();
    for server in servers {
        let server = normalize_nat3_stun_server(&server)?;
        if uniq.insert(server.clone()) {
            normalized.push(server);
        }
    }
    Ok(normalized)
}

fn normalize_nat3_stun_server(server: &str) -> Result<String> {
    if let Some(server) = server.strip_prefix("stun:") {
        return Ok(server.to_string());
    }
    if server.starts_with("turn:") {
        bail!("turn server is not supported for nat3 public address discovery: {server}");
    }
    Ok(server.to_string())
}

async fn discover_nat3_public_addr(
    socket: TokioUdpSocket,
    stun_servers: &[String],
) -> Result<(TokioUdpSocket, BindingOutput)> {
    detect_nat_type3(
        socket,
        stun_servers.iter().map(String::as_str),
        StunConfig::default()
            .with_min_success_response(2)
            .with_transaction_timeout(NAT3_STUN_TRANSACTION_TIMEOUT),
    )
    .await
    .with_context(|| "detect nat3 public address failed")
}

#[cfg(test)]
async fn discover_nat4_public_addr(
    socket: TokioUdpSocket,
    stun_servers: &[String],
) -> Result<(TokioUdpSocket, Option<SocketAddr>)> {
    let (socket, output) = detect_nat_type3_with_recovery(
        socket,
        stun_servers.iter().map(String::as_str),
        StunConfig::default()
            .with_min_success_response(1)
            .with_transaction_timeout(NAT3_STUN_TRANSACTION_TIMEOUT),
    )
    .await;

    let mapped_addr = output
        .with_context(|| "detect nat4 public address failed")
        .ok()
        .and_then(|output| output.mapped_iter().next());
    Ok((socket, mapped_addr))
}

async fn discover_nat4_public_addrs_from_all_servers(
    socket: TokioUdpSocket,
    stun_servers: &[String],
) -> Result<Vec<SocketAddr>> {
    let (_socket, output) = detect_nat_type3_with_recovery(
        socket,
        stun_servers.iter().map(String::as_str),
        StunConfig::default()
            .with_detect_all_server(true)
            .with_min_success_response(1)
            .with_transaction_timeout(NAT3_STUN_TRANSACTION_TIMEOUT),
    )
    .await;

    let mapped_addrs = match output {
        Ok(output) => output.mapped_iter().collect::<Vec<_>>(),
        Err(err) => {
            warn!("discover nat4 candidate socket public addrs failed: {err:#}");
            Vec::new()
        }
    };
    Ok(mapped_addrs)
}

async fn sample_nat4_candidate_public_addrs(
    sample_socket_count: usize,
    stun_servers: &[String],
    total_timeout: Duration,
) -> Result<Vec<SocketAddr>> {
    if sample_socket_count == 0 || stun_servers.is_empty() {
        return Ok(Vec::new());
    }

    let mut pending = FuturesUnordered::new();
    for _ in 0..sample_socket_count {
        let socket = tokio_socket_bind("0.0.0.0:0")
            .await
            .with_context(|| "bind nat4 candidate sample socket failed")?;
        let servers = stun_servers.to_vec();
        pending.push(
            async move { discover_nat4_public_addrs_from_all_servers(socket, &servers).await },
        );
    }

    let deadline = tokio::time::Instant::now() + total_timeout;
    let sleep = tokio::time::sleep_until(deadline);
    tokio::pin!(sleep);

    let mut sampled_addrs = Vec::new();
    while !pending.is_empty() {
        tokio::select! {
            result = pending.next() => {
                if let Some(result) = result {
                    sampled_addrs.extend(result?);
                }
            }
            _ = &mut sleep => {
                warn!(
                    "nat4 candidate ip sampling hit deadline: sockets [{}], timeout [{:?}], collected [{}]",
                    sample_socket_count,
                    total_timeout,
                    sampled_addrs.len()
                );
                break;
            }
        }
    }
    Ok(sampled_addrs)
}

#[cfg(test)]
async fn discover_nat4_public_addrs(
    sockets: Vec<TokioUdpSocket>,
    stun_servers: &[String],
) -> Result<Vec<(TokioUdpSocket, Option<SocketAddr>)>> {
    let mut discovered = Vec::with_capacity(sockets.len());
    for socket in sockets {
        discovered.push(discover_nat4_public_addr(socket, stun_servers).await?);
    }
    Ok(discovered)
}

pub struct PreparedNat3Socket {
    pub socket: UdpSocket,
    pub nat4_target: SocketAddr,
}

pub async fn prepare_nat3_public_target(
    listen: &str,
    stun_servers: &[String],
) -> Result<Option<PreparedNat3Socket>> {
    let stun_servers = resolve_nat3_stun_servers(true, stun_servers)?;
    let socket = tokio_socket_bind(listen)
        .await
        .with_context(|| format!("failed to bind socket addr [{listen}]"))?;
    let local_addr = socket
        .local_addr()
        .with_context(|| "get local address failed")?;
    info!(
        "discover nat3 public address using stun servers {:?} from [{local_addr}]",
        stun_servers
    );
    let (socket, output) = discover_nat3_public_addr(socket, &stun_servers).await?;
    log_nat3_public_addr_discovery(local_addr, &output);

    let mapped = output.mapped_iter().collect::<Vec<_>>();
    let Some(nat4_target) = recommended_nat4_target_for_nat3_discovery(&mapped, output.nat_type())
    else {
        return Ok(None);
    };

    Ok(Some(PreparedNat3Socket {
        socket: socket.into_inner(),
        nat4_target,
    }))
}

fn recommended_nat4_target_for_nat3_discovery(
    mapped: &[SocketAddr],
    nat_type: Option<NatType>,
) -> Option<SocketAddr> {
    match (nat_type, mapped) {
        (Some(NatType::Cone), [mapped]) => Some(*mapped),
        _ => None,
    }
}

fn log_nat3_public_addr_discovery(local_addr: SocketAddr, output: &BindingOutput) {
    let mapped = output.mapped_iter().collect::<Vec<_>>();
    if mapped.is_empty() {
        return;
    }

    let nat_type = match output.nat_type() {
        Some(NatType::Cone) => "cone",
        Some(NatType::Symmetric) => "symmetric",
        None => "unknown",
    };

    info!("nat3 public address discovery local [{local_addr}] => mapped {mapped:?}, nat_type [{nat_type}]");

    if let Some(target) = recommended_nat4_target_for_nat3_discovery(&mapped, output.nat_type()) {
        info!("recommended peer command: rtun nat4 nat4 -t {}", target);
    } else if output.nat_type().is_none() && mapped.len() == 1 {
        warn!(
            "single mapped address discovered for nat3 local [{local_addr}], but nat type is still unknown; skip recommended nat4 command until at least two STUN responses agree"
        );
    } else {
        warn!(
            "multiple mapped addresses discovered for nat3 local [{local_addr}]: {mapped:?}; nat4 manual test may be unstable"
        );
    }
}

#[cfg(test)]
fn wait_for_enter_prompt<R, W>(reader: &mut R, writer: &mut W, prompt: &str) -> Result<()>
where
    R: std::io::BufRead,
    W: std::io::Write,
{
    write_wait_prompt(writer, prompt)?;
    let mut line = String::new();
    loop {
        line.clear();
        let bytes = reader
            .read_line(&mut line)
            .with_context(|| "read pause confirmation failed")?;
        if bytes == 0 {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        break;
    }
    Ok(())
}

#[cfg(test)]
fn wait_for_enter_after_discovery<R, W>(reader: &mut R, writer: &mut W) -> Result<()>
where
    R: std::io::BufRead,
    W: std::io::Write,
{
    wait_for_enter_prompt(reader, writer, NAT3_PAUSE_AFTER_DISCOVERY_PROMPT)
}

#[cfg(test)]
fn wait_for_enter_before_nat3_batch_reroll<R, W>(reader: &mut R, writer: &mut W) -> Result<()>
where
    R: std::io::BufRead,
    W: std::io::Write,
{
    write_wait_prompt(writer, NAT3_HOLD_BATCH_UNTIL_ENTER_PROMPT)?;
    let mut line = String::new();
    loop {
        line.clear();
        let bytes = reader
            .read_line(&mut line)
            .with_context(|| "read pause confirmation failed")?;
        if bytes == 0 {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        break;
    }
    Ok(())
}

async fn hold_batch_send_loop<SendFn, SendFut, WaitSignalFut>(
    mut send_once: SendFn,
    wait_signal: WaitSignalFut,
    interval: Duration,
) -> Result<()>
where
    SendFn: FnMut() -> SendFut,
    SendFut: Future<Output = Result<()>>,
    WaitSignalFut: Future<Output = Result<()>>,
{
    tokio::pin!(wait_signal);
    loop {
        send_once().await?;
        tokio::select! {
            signal = &mut wait_signal => {
                signal?;
                break;
            }
            _ = tokio::time::sleep(interval) => {}
        }
    }
    Ok(())
}

fn write_wait_prompt<W: std::io::Write>(writer: &mut W, prompt: &str) -> Result<()> {
    writeln!(writer, "{prompt}").with_context(|| "write pause prompt failed")?;
    writer
        .flush()
        .with_context(|| "flush pause prompt failed")?;
    Ok(())
}

async fn write_wait_prompt_async(prompt: &'static str) -> Result<()> {
    tokio::task::spawn_blocking(move || -> Result<()> {
        let stdout = std::io::stdout();
        let mut writer = stdout.lock();
        write_wait_prompt(&mut writer, prompt)
    })
    .await
    .with_context(|| "wait prompt task join failed")??;
    Ok(())
}

struct Nat3RerollEnterListener {
    stop: Arc<AtomicBool>,
    cmd_tx: std::sync::mpsc::Sender<Nat3RerollListenerCommand>,
    rx: mpsc::UnboundedReceiver<u64>,
    join_handle: Option<std::thread::JoinHandle<()>>,
    next_epoch: u64,
}

impl Nat3RerollEnterListener {
    fn spawn() -> Self {
        Self::spawn_with_reader(std::io::stdin())
    }

    fn spawn_with_reader<R>(reader: R) -> Self
    where
        R: std::io::Read + AsRawFd + Send + 'static,
    {
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let (tx, rx) = mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let join_handle = std::thread::spawn(move || {
            nat3_reroll_enter_listener_loop(reader, thread_stop, cmd_rx, tx)
        });
        Self {
            stop,
            cmd_tx,
            rx,
            join_handle: Some(join_handle),
            next_epoch: 0,
        }
    }

    async fn begin_epoch(&mut self) -> Result<(u64, usize)> {
        self.next_epoch += 1;
        let epoch = self.next_epoch;
        let (ready_tx, ready_rx) = oneshot::channel();
        self.cmd_tx
            .send(Nat3RerollListenerCommand::StartEpoch { epoch, ready_tx })
            .map_err(|_| anyhow::anyhow!("nat3 hold-batch enter listener stopped"))?;
        let discarded_enters = ready_rx
            .await
            .with_context(|| "nat3 hold-batch enter listener start-epoch ack dropped")??;
        Ok((epoch, discarded_enters))
    }

    async fn recv_epoch(&mut self, epoch: u64) -> Result<()> {
        wait_for_nat3_reroll_signal(&mut self.rx, epoch).await
    }
}

impl Drop for Nat3RerollEnterListener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join_handle) = self.join_handle.take() {
            if join_handle.join().is_err() {
                warn!("nat3 hold-batch enter listener panicked");
            }
        }
    }
}

enum Nat3RerollListenerCommand {
    StartEpoch {
        epoch: u64,
        ready_tx: oneshot::Sender<Result<usize>>,
    },
}

fn nat3_reroll_enter_listener_loop<R>(
    mut reader: R,
    stop: Arc<AtomicBool>,
    cmd_rx: std::sync::mpsc::Receiver<Nat3RerollListenerCommand>,
    tx: mpsc::UnboundedSender<u64>,
) where
    R: std::io::Read + AsRawFd,
{
    let stdin_fd = reader.as_raw_fd();
    let mut input = [0_u8; 256];
    let mut active_epoch = 0_u64;
    while !stop.load(Ordering::Relaxed) {
        while let Ok(command) = cmd_rx.try_recv() {
            if let Err(err) = handle_nat3_reroll_listener_command(
                &mut reader,
                stdin_fd,
                &mut active_epoch,
                command,
            ) {
                warn!("nat3 hold-batch start epoch failed: {err:#}");
            }
        }

        let mut poll_fds = [PollFd::new(
            stdin_fd,
            PollFlags::POLLIN | PollFlags::POLLERR | PollFlags::POLLHUP,
        )];
        match poll(&mut poll_fds, NAT3_HOLD_BATCH_STDIN_POLL_TIMEOUT_MS) {
            Ok(0) => continue,
            Ok(_) => {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                match reader.read(&mut input) {
                    Ok(0) => std::thread::sleep(NAT3_HOLD_BATCH_STDIN_EOF_RETRY),
                    Ok(n) => {
                        let observed = input[..n].iter().filter(|byte| **byte == b'\n').count();
                        if active_epoch != 0 {
                            for _ in 0..observed {
                                if tx.send(active_epoch).is_err() {
                                    return;
                                }
                            }
                        }
                        if observed > 0 {
                            debug!(
                                "nat3 hold-batch enter observed: epoch [{active_epoch}] chunk_enters [{observed}]"
                            );
                        }
                    }
                    Err(err) => {
                        warn!("nat3 hold-batch read stdin failed: {err:#}");
                        std::thread::sleep(NAT3_HOLD_BATCH_STDIN_ERROR_RETRY);
                    }
                }
            }
            Err(err) => {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                warn!("nat3 hold-batch poll stdin failed: {err:#}");
                std::thread::sleep(NAT3_HOLD_BATCH_STDIN_ERROR_RETRY);
            }
        }
    }
}

fn handle_nat3_reroll_listener_command<R>(
    reader: &mut R,
    stdin_fd: i32,
    active_epoch: &mut u64,
    command: Nat3RerollListenerCommand,
) -> Result<()>
where
    R: std::io::Read,
{
    match command {
        Nat3RerollListenerCommand::StartEpoch { epoch, ready_tx } => {
            let discarded_enters = drain_nat3_reroll_stale_input(reader, stdin_fd)?;
            *active_epoch = epoch;
            let _ = ready_tx.send(Ok(discarded_enters));
            Ok(())
        }
    }
}

fn drain_nat3_reroll_stale_input<R>(reader: &mut R, stdin_fd: i32) -> Result<usize>
where
    R: std::io::Read,
{
    let mut discarded_enters = 0_usize;
    let mut input = [0_u8; 256];
    loop {
        let mut poll_fds = [PollFd::new(
            stdin_fd,
            PollFlags::POLLIN | PollFlags::POLLERR | PollFlags::POLLHUP,
        )];
        match poll(&mut poll_fds, 0) {
            Ok(0) => return Ok(discarded_enters),
            Ok(_) => match reader.read(&mut input) {
                Ok(0) => return Ok(discarded_enters),
                Ok(n) => {
                    discarded_enters += input[..n].iter().filter(|byte| **byte == b'\n').count();
                }
                Err(err) => {
                    return Err(err).with_context(|| {
                        "nat3 hold-batch read stdin while draining stale input failed"
                    });
                }
            },
            Err(err) => {
                return Err(err).with_context(|| {
                    "nat3 hold-batch poll stdin while draining stale input failed"
                });
            }
        }
    }
}

async fn wait_for_nat3_reroll_signal(
    rx: &mut mpsc::UnboundedReceiver<u64>,
    epoch: u64,
) -> Result<()> {
    loop {
        let observed_epoch = rx
            .recv()
            .await
            .with_context(|| "nat3 hold-batch enter listener stopped")?;
        if observed_epoch == epoch {
            return Ok(());
        }
        debug!(
            "nat3 hold-batch ignore stale enter signal: expected_epoch [{epoch}] observed_epoch [{observed_epoch}]"
        );
    }
}

async fn wait_for_nat3_enter_with_listener(prompt: &'static str) -> Result<()> {
    let mut listener = Nat3RerollEnterListener::spawn();
    let (epoch, discarded_enters) = listener.begin_epoch().await?;
    write_wait_prompt_async(prompt).await?;
    debug!(
        "nat3 enter wait start: prompt [{prompt}], epoch [{epoch}], discarded_enters [{discarded_enters}]"
    );
    listener.recv_epoch(epoch).await
}

async fn maybe_pause_after_discovery(pause_after_discovery: bool) -> Result<()> {
    if !pause_after_discovery {
        return Ok(());
    }

    wait_for_nat3_enter_with_listener(NAT3_PAUSE_AFTER_DISCOVERY_PROMPT).await
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
    if half == 0 {
        None
    } else {
        Some(half)
    }
}

pub async fn run_nat3_controlled_once<SendControl, SendControlFut>(
    args: Nat3RunConfig,
    session: HardNatSessionParams,
    control_rx: &mut broadcast::Receiver<HardNatControlEnvelope>,
    send_control: SendControl,
) -> Result<HardNatConnectedSocket>
where
    SendControl: FnMut(HardNatControlEnvelope) -> SendControlFut,
    SendControlFut: Future<Output = Result<()>>,
{
    run_nat3_controlled_once_with_socket(args, session, None, control_rx, send_control).await
}

pub async fn run_nat3_controlled_once_with_prebound_socket<SendControl, SendControlFut>(
    args: Nat3RunConfig,
    session: HardNatSessionParams,
    socket: UdpSocket,
    control_rx: &mut broadcast::Receiver<HardNatControlEnvelope>,
    send_control: SendControl,
) -> Result<HardNatConnectedSocket>
where
    SendControl: FnMut(HardNatControlEnvelope) -> SendControlFut,
    SendControlFut: Future<Output = Result<()>>,
{
    run_nat3_controlled_once_with_socket(args, session, Some(socket), control_rx, send_control)
        .await
}

async fn run_nat3_controlled_once_with_socket<SendControl, SendControlFut>(
    args: Nat3RunConfig,
    session: HardNatSessionParams,
    prebound_socket: Option<UdpSocket>,
    control_rx: &mut broadcast::Receiver<HardNatControlEnvelope>,
    mut send_control: SendControl,
) -> Result<HardNatConnectedSocket>
where
    SendControl: FnMut(HardNatControlEnvelope) -> SendControlFut,
    SendControlFut: Future<Output = Result<()>>,
{
    args.validate()?;
    if args.hold_batch_until_enter {
        bail!("hold_batch_until_enter is only supported by run_nat3");
    }

    let socket = prepare_nat3_socket(&args, prebound_socket).await?;
    if let Some(ttl) = args.ttl {
        socket.set_ttl(ttl).with_context(|| "set ttl failed")?;
        debug!("set ttl [{ttl}]");
    }

    let socket = Arc::new(socket);
    let local_addr = socket
        .local_addr()
        .with_context(|| "get local address failed")?;
    let text = Arc::new(resolve_probe_text(args.content.as_deref()));
    let shared = Arc::new(Shared {
        connecteds: Default::default(),
        connecteds_by_socket_id: Default::default(),
        nat3_candidate_observed_remotes: Default::default(),
        nat3_current_public_addr: Default::default(),
    });
    let send_mode = Nat3BatchSendMode::JsonProbe {
        session_id: session.session_id,
        next_seq: AtomicU64::new(0),
    };

    let recv_task = {
        let socket = socket.clone();
        let shared = shared.clone();
        let opts = RecvLoopOptions {
            role: HardNatRole::Nat3,
            expected_text: text.clone(),
            promote_hit_ttl: None,
            token_handshake_enabled: true,
            json_probe_mode: JsonProbeMode::Nat3HandshakeResponder {
                next_seq: Arc::new(AtomicU64::new(0)),
            },
            echo_content_ack: false,
            debug_converge_lease: args.debug_converge_lease,
            manual_converge_socket: None,
        };
        tokio::spawn(async move {
            let r = recv_loop(socket, &shared, opts).await;
            info!("recv finished [{r:?}]");
        })
    };
    let recv_tasks = RecvTaskGuard::new(vec![recv_task]);

    let mut executor = HardNatExecutor::new(session.session_id, session.lease_timeout());
    let mut ack_seq = 1_u64;
    let mut send_tick = tokio::time::interval(args.interval);
    send_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut status_tick = tokio::time::interval(Duration::from_millis(10).min(args.interval));
    status_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let start_at = Instant::now();
    let mut connected_control_target: Option<SocketAddr> = None;

    loop {
        if executor.phase() == HardNatExecutorPhase::Connected {
            let snapshot = nat3_controlled_state_snapshot(&executor, &shared);
            if let Some(target) = connected_control_target {
                if let Some(conn) =
                    shared.connected_conn_for_remote(HardNatRole::Nat3, target, start_at)
                {
                    warn!(
                        "[relay_hardnat_diag] nat3 controlled returning connected: session_id={}, phase={:?}, has_connected={}, connected_count={}",
                        snapshot.session_id,
                        snapshot.phase,
                        snapshot.has_connected,
                        snapshot.connected_count
                    );
                    recv_tasks.abort_and_wait().await;
                    return Ok(conn);
                }
            } else if shared.has_connected() {
                warn!(
                    "[relay_hardnat_diag] nat3 controlled returning connected: session_id={}, phase={:?}, has_connected={}, connected_count={}",
                    snapshot.session_id,
                    snapshot.phase,
                    snapshot.has_connected,
                    snapshot.connected_count
                );
                let first = shared
                    .first_connected_conn(HardNatRole::Nat3, start_at)
                    .with_context(|| "missing connected target after nat3 controlled connect")?;
                recv_tasks.abort_and_wait().await;
                return Ok(first);
            }
        }

        let lease_deadline = executor.lease_deadline();
        let lease_sleep = async move {
            if let Some(deadline) = lease_deadline {
                tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
            } else {
                futures::future::pending::<()>().await;
            }
        };
        tokio::select! {
            env = recv_hard_nat_control(control_rx) => {
                let env = env?;
                let abort_reason = match env.msg.as_ref() {
                    Some(hard_nat_control_envelope::Msg::Abort(msg)) => Some(msg.reason.to_string()),
                    _ => None,
                };
                let mut connected_target = None;
                if let Some(hard_nat_control_envelope::Msg::Connected(msg)) = env.msg.as_ref() {
                    let nat3_addr_matches =
                        hard_nat_connected_nat3_addr_matches(msg, local_addr, &session);
                    let parsed_nat3_addr = parse_hard_nat_connected_nat3_addr(msg);
                    let parsed_target = parse_hard_nat_connected_target(msg);
                    let connected_candidate = parse_hard_nat_connected_candidate(msg);
                    let has_candidate_observation = parsed_nat3_addr
                        .zip(parsed_target)
                        .map(|(selected_nat3_addr, target)| {
                            shared
                                .nat3_candidate_observation(target)
                                .map(|obs| {
                                    obs.nat3_addr == selected_nat3_addr
                                        && obs.nat4_addr == target
                                        && obs.socket_id == connected_candidate.0
                                        && obs.generation == connected_candidate.1
                                })
                                .unwrap_or(false)
                        })
                        .unwrap_or(false);
                    if nat3_addr_matches && has_candidate_observation {
                        connected_target = parsed_target;
                    } else {
                        warn!(
                            "[relay_hardnat_diag] nat3 controlled ignored invalid Connected: session_id={}, nat3_addr_matches={}, parsed_nat3_addr={:?}, parsed_target={:?}, has_candidate_observation={}, local_addr={}, selected_nat3_addr={}, selected_nat4_ip={}, selected_port={}",
                            session.session_id,
                            nat3_addr_matches,
                            parsed_nat3_addr,
                            parsed_target,
                            has_candidate_observation,
                            local_addr,
                            msg.selected_nat3_addr,
                            msg.selected_nat4_ip,
                            msg.selected_port
                        );
                        continue;
                    }
                }
                let acked_seq = env.seq;
                let control_label = hard_nat_control_debug_label(&env);
                let apply = executor.apply_control(env, Instant::now());
                debug!(
                    "hard_nat nat3 controlled apply={apply:?}, phase={:?}",
                    executor.phase()
                );
                if apply == HardNatControlApply::Applied {
                    let current_nat3_public_addr =
                        resolve_nat3_controlled_public_addr(&session, executor.current_cursor().as_ref())?;
                    shared.set_nat3_current_public_addr(current_nat3_public_addr);
                    let snapshot = nat3_controlled_state_snapshot(&executor, &shared);
                    warn!(
                        "[relay_hardnat_diag] nat3 controlled applied control: session_id={}, acked_seq={}, control={}, phase={:?}, has_connected={}, connected_count={}",
                        snapshot.session_id,
                        acked_seq,
                        control_label,
                        snapshot.phase,
                        snapshot.has_connected,
                        snapshot.connected_count
                    );
                    if let Some(target) = connected_target {
                        connected_control_target = Some(target);
                        let _ = shared.record_connected(target, socket.clone(), None);
                    }
                    let ack = build_hard_nat_ack(
                        session.session_id,
                        ack_seq,
                        acked_seq,
                        hard_nat_executor_state_value(executor.phase()),
                    );
                    ack_seq += 1;
                    debug!(
                        "hard_nat nat3 controlled sending ack {}",
                        hard_nat_control_debug_label(&ack)
                    );
                    send_control(ack).await?;
                    if let Some(reason) = abort_reason {
                        recv_tasks.abort_and_wait().await;
                        bail!("hard-nat nat3 aborted: {reason}");
                    }
                }
            }
            _ = status_tick.tick() => {}
            _ = send_tick.tick(), if matches!(executor.phase(), HardNatExecutorPhase::ExecutingBatch(_) | HardNatExecutorPhase::WaitingNextCommand(_)) => {
                if let Some(cursor) = executor.current_cursor() {
                    let current_nat3_public_addr =
                        resolve_nat3_controlled_public_addr(&session, Some(&cursor))?;
                    shared.set_nat3_current_public_addr(current_nat3_public_addr);
                    let targets = build_nat3_controlled_targets(&session, args.target_ip, &cursor)?;
                    let _ = send_nat3_batch_once(
                        &socket,
                        &text,
                        &targets,
                        &shared,
                        0,
                        false,
                        &send_mode,
                    )
                    .await?;
                }
            }
            _ = lease_sleep => {
                if executor.expire_if_needed(Instant::now()) {
                    recv_tasks.abort_and_wait().await;
                    bail!("hard-nat nat3 lease expired");
                }
            }
        }
    }
}

pub async fn run_nat4_controlled_once<SendControl, SendControlFut>(
    args: Nat4RunConfig,
    session: HardNatSessionParams,
    local_nat4_candidate_ips: Vec<String>,
    start_batch_ports: Vec<u32>,
    selected_nat4_ip: Option<String>,
    mut send_control: SendControl,
) -> Result<HardNatConnectedSocket>
where
    SendControl: FnMut(HardNatControlEnvelope) -> SendControlFut,
    SendControlFut: Future<Output = Result<()>>,
{
    args.validate()?;
    if args.debug_keep_recv {
        bail!("debug_keep_recv is only supported by run_nat4");
    }

    let mut scheduler = HardNatScheduler::new(HardNatSchedulerConfig {
        session_id: session.session_id,
        nat4_ip_count: local_nat4_candidate_ips.len().max(1),
        nat3_addr_count: session.nat3_public_addrs.len().max(1),
        lease_timeout: session.lease_timeout(),
    });
    let batch_count = if session.batch_port_count == 0 {
        start_batch_ports.len().max(1)
    } else {
        session.batch_port_count as usize
    };

    let start_batch = scheduler.start_batch(start_batch_ports)?;
    debug!(
        "hard_nat nat4 controlled sending start {}",
        hard_nat_control_debug_label(&start_batch)
    );
    send_control(start_batch).await?;

    let runtime = match prepare_nat4_probe_runtime(
        &args,
        Nat4ProbeRuntimeOptions::controlled_converge(args.interval),
    )
    .await
    {
        Ok(runtime) => runtime,
        Err(err) => {
            if let Ok(abort) = scheduler.abort(format!("{err:#}")) {
                let _ = send_control(abort).await;
            }
            return Err(err);
        }
    };
    if let Some(coordinator) = runtime.manual_converge.as_ref() {
        loop {
            if coordinator.lock().finish_warming(Instant::now()) {
                debug!("controlled converge enter probing");
                break;
            }
            tokio::time::sleep(Duration::from_millis(20).min(runtime.interval)).await;
        }
    }
    let mut probe_tick = tokio::time::interval(runtime.interval);
    probe_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let ip_try_timeout = session.ip_try_timeout().max(args.interval);
    let mut ip_try_sleep = Box::pin(tokio::time::sleep(ip_try_timeout));
    let mut keepalive_tick = tokio::time::interval(session.keepalive_interval());
    keepalive_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let _ = keepalive_tick.tick().await;

    let mut probe_due = true;
    let result: Result<HardNatConnectedSocket> = async {
        loop {
            if probe_due {
                let current_cursor = scheduler.current_cursor();
                let current_target =
                    resolve_nat4_controlled_target(&session, args.target, current_cursor.as_ref())?;
                if let Some(coordinator) = runtime.manual_converge.as_ref() {
                    let socket_ids = runtime
                        .senders
                        .iter()
                        .map(|sender| sender.socket_id)
                        .collect::<Vec<_>>();
                    let plans = {
                        let mut state = coordinator.lock();
                        plan_nat4_controlled_converge_send_step(
                            &mut state,
                            &socket_ids,
                            Instant::now(),
                        )
                    };
                    for (sender, plan) in runtime.senders.iter().zip(plans.iter()) {
                        debug_assert_eq!(sender.socket_id, plan.socket_id);
                        if !plan.should_send {
                            continue;
                        }
                        if let Some(token) = plan.token {
                            let json_handshake_session_id =
                                coordinator.lock().json_probe_session_id(plan.socket_id);
                            if let Some(session_id) = json_handshake_session_id {
                                sender
                                    .send_json_handshake_req_from_token_to(
                                        current_target,
                                        session_id,
                                        token,
                                    )
                                    .await?;
                            } else {
                                sender.send_token_to(current_target, token).await?;
                            }
                        }
                    }
                } else {
                    for sender in &runtime.senders {
                        if runtime.shared.has_connected() {
                            break;
                        }
                        sender.send_one_to(current_target).await?;
                    }
                }
                probe_due = false;
            }

            let manual_connected_candidate = runtime
                .manual_converge
                .as_ref()
                .and_then(|coordinator| coordinator.lock().connected_socket_candidate());
            if manual_connected_candidate.is_some()
                || (runtime.manual_converge.is_none() && runtime.shared.has_connected())
            {
                break;
            }

            tokio::select! {
                _ = probe_tick.tick() => {
                    probe_due = true;
                }
                _ = keepalive_tick.tick() => {
                    let keepalive = scheduler.lease_keepalive();
                    debug!(
                        "hard_nat nat4 controlled sending keepalive {}",
                        hard_nat_control_debug_label(&keepalive)
                    );
                    send_control(keepalive).await?;
                }
                _ = &mut ip_try_sleep => {
                    match scheduler.advance_after_timeout() {
                        Some(HardNatSchedulerAdvance::Send(env)) => {
                            debug!(
                                "hard_nat nat4 controlled sending advance {}",
                                hard_nat_control_debug_label(&env)
                            );
                            send_control(env).await?;
                            probe_due = true;
                        }
                        Some(HardNatSchedulerAdvance::NeedNextBatch { .. }) => {
                            let next_ports = build_random_port_batch(batch_count);
                            let env = scheduler.start_next_batch(next_ports)?;
                            debug!(
                                "hard_nat nat4 controlled sending next batch {}",
                                hard_nat_control_debug_label(&env)
                            );
                            send_control(env).await?;
                            probe_due = true;
                        }
                        None => {}
                    }
                    ip_try_sleep
                        .as_mut()
                        .reset(tokio::time::Instant::now() + ip_try_timeout);
                }
            }
        }

        let manual_connected_candidate = runtime
            .manual_converge
            .as_ref()
            .and_then(|coordinator| coordinator.lock().connected_socket_candidate());
        let first = if let Some((socket_id, _generation)) = manual_connected_candidate {
            runtime
                .shared
                .connected_conn_for_socket_id(HardNatRole::Nat4, socket_id, runtime.start_at)
                .with_context(|| "missing controlled converge winner after nat4 connect")?
        } else {
            runtime
                .shared
                .first_connected_conn(HardNatRole::Nat4, runtime.start_at)
                .with_context(|| "missing connected target after nat4 controlled connect")?
        };

        let current_cursor = scheduler.current_cursor();
        let selected_nat4_ip = resolve_selected_nat4_ip(
            &local_nat4_candidate_ips,
            selected_nat4_ip.as_deref(),
            current_cursor.as_ref(),
        )?;
        let connected = scheduler.connected(
            first.remote_addr.to_string(),
            selected_nat4_ip,
            first.remote_addr.port() as u32,
            session.connected_ttl.max(HARD_NAT_DEFAULT_CONNECTED_TTL),
            manual_connected_candidate.map(|x| x.0).unwrap_or_default(),
            manual_connected_candidate.map(|x| x.1).unwrap_or_default(),
        )?;
        debug!(
            "hard_nat nat4 controlled sending connected {}",
            hard_nat_control_debug_label(&connected)
        );
        send_control(connected).await?;
        Ok(first)
    }
    .await;

    runtime.recv_tasks.abort_and_wait().await;
    match result {
        Ok(first) => Ok(first),
        Err(err) => {
            if let Ok(abort) = scheduler.abort(format!("{err:#}")) {
                debug!(
                    "hard_nat nat4 controlled sending abort {}",
                    hard_nat_control_debug_label(&abort)
                );
                let _ = send_control(abort).await;
            }
            Err(err)
        }
    }
}

pub async fn run_nat3(args: Nat3RunConfig) -> Result<()> {
    if args.hold_batch_until_enter {
        return run_nat3_hold_batch_until_enter(args).await;
    }

    let interval = args.interval;
    let (conn, recv_tasks, text) = probe_nat3_until_connected(args, None).await?;
    send_conn_loop_keep_recv(conn.socket, conn.remote_addr, &text, interval, recv_tasks).await
}

pub async fn run_nat3_once(args: Nat3RunConfig) -> Result<HardNatConnectedSocket> {
    run_nat3_once_with_socket(args, None).await
}

pub async fn run_nat3_once_with_prebound_socket(
    args: Nat3RunConfig,
    socket: UdpSocket,
) -> Result<HardNatConnectedSocket> {
    run_nat3_once_with_socket(args, Some(socket)).await
}

async fn run_nat3_once_with_socket(
    args: Nat3RunConfig,
    prebound_socket: Option<UdpSocket>,
) -> Result<HardNatConnectedSocket> {
    let (first, recv_tasks, _text) = probe_nat3_until_connected(args, prebound_socket).await?;
    recv_tasks.abort_and_wait().await;
    Ok(first)
}

async fn probe_nat3_until_connected(
    args: Nat3RunConfig,
    prebound_socket: Option<UdpSocket>,
) -> Result<(HardNatConnectedSocket, RecvTaskGuard, String)> {
    args.validate()?;
    if args.hold_batch_until_enter {
        bail!("hold_batch_until_enter is only supported by run_nat3");
    }

    let interval = args.interval;
    let batch_interval = args.batch_interval;
    let ttl = args.ttl;
    let target_ip = args.target_ip;
    let start_at = Instant::now();

    let socket = prepare_nat3_socket(&args, prebound_socket).await?;

    if let Some(ttl) = ttl {
        socket.set_ttl(ttl).with_context(|| "set ttl failed")?;
        info!("set ttl [{ttl}]");
    }

    let socket = Arc::new(socket);
    let local = socket
        .local_addr()
        .with_context(|| "get local address failed")?;

    let text = resolve_probe_text(args.content.as_deref());
    let expected_text = Arc::new(text.clone());
    let send_mode = nat3_default_send_mode(
        allocate_hard_nat_session_id(),
        args.debug_converge_lease,
    );

    let shared = Arc::new(Shared {
        connecteds: Default::default(),
        connecteds_by_socket_id: Default::default(),
        nat3_candidate_observed_remotes: Default::default(),
        nat3_current_public_addr: Default::default(),
    });

    let recv_task = {
        let socket = socket.clone();
        let shared = shared.clone();
        let opts = RecvLoopOptions {
            role: HardNatRole::Nat3,
            expected_text: expected_text.clone(),
            promote_hit_ttl: None,
            token_handshake_enabled: args.debug_converge_lease,
            json_probe_mode: if args.debug_converge_lease {
                JsonProbeMode::Nat3HandshakeResponder {
                    next_seq: Arc::new(AtomicU64::new(0)),
                }
            } else {
                JsonProbeMode::Nat3ProbeOnly
            },
            echo_content_ack: false,
            debug_converge_lease: args.debug_converge_lease,
            manual_converge_socket: None,
        };

        tokio::spawn(async move {
            let r = recv_loop(socket, &shared, opts).await;
            info!("recv finished [{r:?}]");
        })
    };
    let recv_tasks = RecvTaskGuard::new(vec![recv_task]);

    let mut has_recv = false;
    let mut num = 0_usize;
    let max_ports = 50000;
    let mut try_ports = HashSet::with_capacity(max_ports);

    while !has_recv {
        let start_time = Instant::now();
        let targets =
            build_nat3_target_batch(args.count, target_ip, &mut try_ports, max_ports, &mut num);

        while start_time.elapsed() < batch_interval {
            has_recv = send_nat3_batch_once(
                &socket,
                &expected_text,
                &targets,
                &shared,
                num,
                true,
                &send_mode,
            )
            .await?;

            if has_recv {
                break;
            }

            debug!("sent num [{num}]: [{local}] => [{target_ip}]");
            tokio::time::sleep(interval).await;
        }
    }

    let first = shared
        .first_connected_conn(HardNatRole::Nat3, start_at)
        .with_context(|| "missing connected target")?;
    Ok((first, recv_tasks, text))
}

async fn run_nat3_hold_batch_until_enter(args: Nat3RunConfig) -> Result<()> {
    args.validate()?;

    let interval = args.interval;
    let target_ip = args.target_ip;
    let socket = prepare_nat3_socket(&args, None).await?;

    if let Some(ttl) = args.ttl {
        socket.set_ttl(ttl).with_context(|| "set ttl failed")?;
        debug!("set ttl [{ttl}]");
    }

    let socket = Arc::new(socket);
    let text = Arc::new(resolve_probe_text(args.content.as_deref()));
    let shared = Arc::new(Shared {
        connecteds: Default::default(),
        connecteds_by_socket_id: Default::default(),
        nat3_candidate_observed_remotes: Default::default(),
        nat3_current_public_addr: Default::default(),
    });
    let send_mode = nat3_default_send_mode(
        allocate_hard_nat_session_id(),
        args.debug_converge_lease,
    );

    let recv_task = {
        let socket = socket.clone();
        let shared = shared.clone();
        let opts = RecvLoopOptions {
            role: HardNatRole::Nat3,
            expected_text: text.clone(),
            promote_hit_ttl: None,
            token_handshake_enabled: args.debug_converge_lease,
            json_probe_mode: if args.debug_converge_lease {
                JsonProbeMode::Nat3HandshakeResponder {
                    next_seq: Arc::new(AtomicU64::new(0)),
                }
            } else {
                JsonProbeMode::Nat3ProbeOnly
            },
            echo_content_ack: false,
            debug_converge_lease: args.debug_converge_lease,
            manual_converge_socket: None,
        };
        tokio::spawn(async move {
            let r = recv_loop(socket, &shared, opts).await;
            info!("recv finished [{r:?}]");
        })
    };
    let _recv_tasks = RecvTaskGuard::new(vec![recv_task]);

    let mut num = 0_usize;
    let max_ports = 50000;
    let mut try_ports = HashSet::with_capacity(max_ports);
    let mut reroll_listener = Nat3RerollEnterListener::spawn();
    let mut batch_id = 0_u64;

    loop {
        batch_id += 1;
        let targets =
            build_nat3_target_batch(args.count, target_ip, &mut try_ports, max_ports, &mut num);
        let (batch_epoch, discarded_enters) = reroll_listener.begin_epoch().await?;
        write_wait_prompt_async(NAT3_HOLD_BATCH_UNTIL_ENTER_PROMPT).await?;
        debug!(
            "nat3 hold-batch start: batch [{batch_id}], epoch [{batch_epoch}], discarded_enters [{discarded_enters}], targets [{}]",
            targets.len()
        );

        hold_batch_send_loop(
            || async {
                let _ =
                    send_nat3_batch_once(&socket, &text, &targets, &shared, num, false, &send_mode)
                        .await?;
                Ok::<(), anyhow::Error>(())
            },
            reroll_listener.recv_epoch(batch_epoch),
            interval,
        )
        .await?;
        debug!("nat3 hold-batch reroll: batch [{batch_id}]");
    }
}

pub async fn run_nat4(args: Nat4RunConfig) -> Result<()> {
    if args.debug_keep_recv {
        return run_nat4_keep_recv(args).await;
    }

    let interval = args.interval;
    let text = resolve_probe_text(args.content.as_deref());
    let conn = run_nat4_once(args).await?;
    send_conn_loop(conn.socket, conn.remote_addr, &text, interval).await
}

pub async fn run_nat4_once(args: Nat4RunConfig) -> Result<HardNatConnectedSocket> {
    args.validate()?;
    if args.debug_keep_recv {
        bail!("debug_keep_recv is only supported by run_nat4");
    }

    let runtime = prepare_nat4_probe_runtime(&args, Nat4ProbeRuntimeOptions::basic()).await?;
    let target = runtime.target;
    let ttl = runtime.ttl;

    let mut has_recv = false;

    while !has_recv {
        for sender in &runtime.senders {
            if runtime.shared.has_connected() {
                has_recv = true;
                break;
            }

            sender.send_one().await?;
        }

        info!(
            "send target [{}], num [{}], ttl [{:?}]",
            target,
            runtime.senders.len(),
            ttl
        );

        tokio::time::sleep(runtime.interval).await;
    }

    let first = runtime
        .shared
        .first_connected_conn(HardNatRole::Nat4, runtime.start_at)
        .with_context(|| "missing connected target")?;
    runtime.recv_tasks.abort_and_wait().await;
    Ok(first)
}

async fn run_nat4_keep_recv(args: Nat4RunConfig) -> Result<()> {
    args.validate()?;

    let runtime = prepare_nat4_probe_runtime(
        &args,
        if args.debug_converge_lease {
            Nat4ProbeRuntimeOptions::debug_converge(args.interval)
        } else {
            Nat4ProbeRuntimeOptions::basic()
        },
    )
    .await?;
    let target = runtime.target;
    let ttl = runtime.ttl;
    let _recv_tasks = runtime.recv_tasks;
    let senders = runtime.senders;
    let interval = runtime.interval;
    let manual_converge = runtime.manual_converge;

    if let Some(coordinator) = manual_converge.as_ref() {
        loop {
            let entered = coordinator.lock().finish_warming(Instant::now());
            if entered {
                debug!("manual converge enter probing");
                break;
            }
            tokio::time::sleep(Duration::from_millis(20).min(interval)).await;
        }
    }

    loop {
        let plans = if let Some(coordinator) = manual_converge.as_ref() {
            let socket_ids = senders
                .iter()
                .map(|sender| sender.socket_id)
                .collect::<Vec<_>>();
            let mut state = coordinator.lock();
            plan_nat4_manual_converge_send_step(&mut state, &socket_ids, Instant::now())
        } else {
            senders
                .iter()
                .map(|sender| Nat4SendPlan {
                    socket_id: sender.socket_id,
                    should_send: true,
                    token: None,
                })
                .collect::<Vec<_>>()
        };

        let mut sent = 0usize;
        for (sender, plan) in senders.iter().zip(plans.iter()) {
            debug_assert_eq!(sender.socket_id, plan.socket_id);
            if !plan.should_send {
                continue;
            }
            if let Some(token) = plan.token {
                let json_handshake_session_id = manual_converge.as_ref().and_then(|coordinator| {
                    coordinator.lock().json_probe_session_id(plan.socket_id)
                });
                if let Some(session_id) = json_handshake_session_id {
                    sender
                        .send_json_handshake_req_from_token(session_id, token)
                        .await?;
                } else {
                    sender.send_token(token).await?;
                }
            } else {
                sender.send_one().await?;
            }
            sent += 1;
        }

        debug!(
            "nat4 debug keep-recv send target [{}], sent [{}], total [{}], ttl [{:?}]",
            target,
            sent,
            senders.len(),
            ttl
        );
        tokio::time::sleep(interval).await;
    }
}

async fn prepare_nat3_socket(
    args: &Nat3RunConfig,
    prebound_socket: Option<UdpSocket>,
) -> Result<UdpSocket> {
    let socket = if let Some(socket) = prebound_socket {
        socket
    } else if args.discover_public_addr {
        let stun_servers =
            resolve_nat3_stun_servers(args.discover_public_addr, &args.stun_servers)?;
        let socket = tokio_socket_bind(&args.listen)
            .await
            .with_context(|| format!("failed to bind socket addr [{}]", args.listen))?;
        let local_addr = socket
            .local_addr()
            .with_context(|| "get local address failed")?;
        info!(
            "discover nat3 public address using stun servers {:?} from [{local_addr}]",
            stun_servers
        );
        let (socket, output) = discover_nat3_public_addr(socket, &stun_servers).await?;
        log_nat3_public_addr_discovery(local_addr, &output);
        maybe_pause_after_discovery(args.pause_after_discovery).await?;
        socket.into_inner()
    } else {
        UdpSocket::bind(&args.listen)
            .await
            .with_context(|| format!("failed to bind socket addr [{}]", args.listen))?
    };

    Ok(socket)
}

fn build_nat3_target_batch(
    count: usize,
    target_ip: IpAddr,
    try_ports: &mut HashSet<u16>,
    max_ports: usize,
    num: &mut usize,
) -> Vec<SocketAddr> {
    let mut targets = Vec::with_capacity(count);

    for _ in 0..count {
        loop {
            let port = rand::thread_rng().gen_range(1024..=u16::MAX);

            if try_ports.len() >= max_ports {
                try_ports.clear();
            }

            if !try_ports.contains(&port) {
                try_ports.insert(port);
                targets.push(SocketAddr::new(target_ip, port));
                break;
            }
        }
        *num += 1;
    }

    targets
}

fn build_nat3_controlled_targets(
    session: &HardNatSessionParams,
    fallback_target_ip: IpAddr,
    cursor: &HardNatBatchCursor,
) -> Result<Vec<SocketAddr>> {
    let target_ip = resolve_nat4_candidate_ip(session, fallback_target_ip, cursor.nat4_ip_index)?;
    cursor
        .ports
        .iter()
        .map(|port| {
            let port = u16::try_from(*port)
                .with_context(|| format!("hard-nat nat3 batch port out of range [{port}]"))?;
            Ok(SocketAddr::new(target_ip, port))
        })
        .collect()
}

fn resolve_nat4_candidate_ip(
    session: &HardNatSessionParams,
    fallback_target_ip: IpAddr,
    nat4_ip_index: u32,
) -> Result<IpAddr> {
    if session.nat4_candidate_ips.is_empty() {
        return Ok(fallback_target_ip);
    }

    let value = session
        .nat4_candidate_ips
        .get(nat4_ip_index as usize)
        .with_context(|| {
            format!("hard-nat nat4 candidate ip index out of range [{nat4_ip_index}]")
        })?;
    value
        .parse()
        .with_context(|| format!("parse hard-nat nat4 candidate ip failed [{value}]"))
}

fn resolve_nat4_controlled_target(
    session: &HardNatSessionParams,
    fallback_target: SocketAddr,
    cursor: Option<&HardNatBatchCursor>,
) -> Result<SocketAddr> {
    if session.nat3_public_addrs.is_empty() {
        return Ok(fallback_target);
    }

    let nat3_addr_index = cursor.map(|cursor| cursor.nat3_addr_index).unwrap_or(0);
    let value = session
        .nat3_public_addrs
        .get(nat3_addr_index as usize)
        .with_context(|| {
            format!("hard-nat nat3 public addr index out of range [{nat3_addr_index}]")
        })?;
    value
        .parse()
        .with_context(|| format!("parse hard-nat nat3 public addr failed [{value}]"))
}

fn resolve_selected_nat4_ip(
    local_nat4_candidate_ips: &[String],
    fallback_target_ip: Option<&str>,
    cursor: Option<&HardNatBatchCursor>,
) -> Result<String> {
    if local_nat4_candidate_ips.is_empty() {
        return Ok(fallback_target_ip.unwrap_or_default().to_string());
    }

    let nat4_ip_index = cursor.map(|cursor| cursor.nat4_ip_index).unwrap_or(0);
    local_nat4_candidate_ips
        .get(nat4_ip_index as usize)
        .cloned()
        .with_context(|| format!("hard-nat nat4 candidate ip index out of range [{nat4_ip_index}]"))
}

pub fn build_random_port_batch(count: usize) -> Vec<u32> {
    let mut ports = Vec::with_capacity(count);
    let mut seen = HashSet::with_capacity(count);
    while ports.len() < count {
        let port = rand::thread_rng().gen_range(1024..=u16::MAX);
        if seen.insert(port) {
            ports.push(port as u32);
        }
    }
    ports
}

async fn send_nat3_batch_once(
    socket: &Arc<UdpSocket>,
    text: &str,
    targets: &[SocketAddr],
    shared: &Arc<Shared>,
    num: usize,
    stop_on_connected: bool,
    send_mode: &Nat3BatchSendMode,
) -> Result<bool> {
    for target in targets {
        if stop_on_connected && shared.has_connected() {
            return Ok(true);
        }

        let payload = send_mode.next_payload(text)?;
        let sent_bytes = socket
            .send_to(payload.as_bytes(), target)
            .await
            .with_context(|| "send failed")?;
        if sent_bytes == payload.as_bytes().len() {
            debug!("=> [{target}, {sent_bytes}]: [{payload}]");
        } else {
            warn!(
                "No.{num}: sent partial {sent_bytes} < {}",
                payload.as_bytes().len()
            );
        }
    }

    Ok(stop_on_connected && shared.has_connected())
}

struct Nat4ProbeRuntime {
    target: SocketAddr,
    interval: Duration,
    ttl: Option<u32>,
    start_at: Instant,
    shared: Arc<Shared>,
    senders: Vec<UdpSender>,
    recv_tasks: RecvTaskGuard,
    manual_converge: Option<Arc<Mutex<ManualConvergeCoordinator>>>,
}

#[derive(Clone)]
struct Nat4ProbeRuntimeOptions {
    manual_converge_cfg: Option<ManualConvergeConfig>,
    token_handshake_enabled: bool,
    json_probe_mode: Nat4RuntimeJsonProbeMode,
    echo_content_ack: bool,
}

#[derive(Clone, Copy)]
enum Nat4RuntimeJsonProbeMode {
    Disabled,
    ProbeResponder,
    HandshakeResponder,
}

impl Nat4ProbeRuntimeOptions {
    fn basic() -> Self {
        Self {
            manual_converge_cfg: None,
            token_handshake_enabled: false,
            json_probe_mode: Nat4RuntimeJsonProbeMode::Disabled,
            echo_content_ack: false,
        }
    }

    fn debug_converge(interval: Duration) -> Self {
        Self {
            manual_converge_cfg: Some(ManualConvergeConfig::for_debug_lease(interval)),
            token_handshake_enabled: true,
            json_probe_mode: Nat4RuntimeJsonProbeMode::HandshakeResponder,
            echo_content_ack: false,
        }
    }

    fn controlled_converge(interval: Duration) -> Self {
        Self {
            manual_converge_cfg: Some(ManualConvergeConfig::for_controlled(interval)),
            token_handshake_enabled: true,
            json_probe_mode: Nat4RuntimeJsonProbeMode::HandshakeResponder,
            echo_content_ack: false,
        }
    }
}

async fn prepare_nat4_probe_runtime(
    args: &Nat4RunConfig,
    options: Nat4ProbeRuntimeOptions,
) -> Result<Nat4ProbeRuntime> {
    let target = args.target;
    let interval = args.interval;
    let start_at = Instant::now();

    let text = Arc::new(resolve_probe_text(args.content.as_deref()));
    let shared = Arc::new(Shared {
        connecteds: Default::default(),
        connecteds_by_socket_id: Default::default(),
        nat3_candidate_observed_remotes: Default::default(),
        nat3_current_public_addr: Default::default(),
    });
    let manual_converge_cfg = options.manual_converge_cfg.clone();
    let manual_converge = manual_converge_cfg.as_ref().map(|cfg| {
        Arc::new(Mutex::new(ManualConvergeCoordinator::new(
            args.count,
            interval,
            cfg.warm_drain,
        )))
    });

    let ttl = match args.ttl {
        Some(ttl) => Some(ttl),
        None => ping_and_half_hops(target.ip())
            .await
            .with_context(|| "ping_and_get_hops failed")?,
    };

    let debug_promote_hit_ttl = args.debug_promote_hit_ttl;

    let mut sender_parts = Vec::with_capacity(args.count);
    let mut recv_tasks = RecvTaskGuard::with_capacity(args.count);

    if let Some(cfg) = manual_converge_cfg.as_ref() {
        debug!(
            "manual converge warming start: sockets [{}], warm_drain [{:?}]",
            args.count, cfg.warm_drain
        );
    }

    for _ in 0..args.count {
        let listen = "0.0.0.0:0";
        let socket = tokio_socket_bind(listen)
            .await
            .with_context(|| format!("failed to bind socket addr [{}]", listen))?;
        let local = socket.local_addr()?;
        sender_parts.push((socket, local));
    }

    if args.dump_public_addrs {
        let stun_servers = resolve_nat3_stun_servers(true, &[])?;
        let sampled_addrs = sample_nat4_candidate_public_addrs(
            HARD_NAT_NAT4_CANDIDATE_SAMPLE_SOCKET_COUNT,
            &stun_servers,
            Duration::from_secs(HARD_NAT_NAT4_CANDIDATE_SAMPLE_TIMEOUT_SECS),
        )
        .await?;
        let candidate_ips = merge_nat4_candidate_ips_from_sources(&[], &sampled_addrs);
        debug!(
            "nat4 dump-public-addrs dedicated sampler: sample_sockets [{}], stun_servers {:?}, sampled_addrs {:?}, candidate_ips {:?}",
            HARD_NAT_NAT4_CANDIDATE_SAMPLE_SOCKET_COUNT,
            stun_servers,
            sampled_addrs,
            candidate_ips
        );
    }

    let mut senders = Vec::with_capacity(args.count);
    for (socket_id, (socket, local)) in sender_parts.into_iter().enumerate() {
        let socket_id = socket_id as u64;
        let socket = Arc::new(socket.into_inner());
        let recv_task = {
            let socket = socket.clone();
            let shared = shared.clone();
            let opts = RecvLoopOptions {
                role: HardNatRole::Nat4,
                expected_text: text.clone(),
                promote_hit_ttl: debug_promote_hit_ttl,
                token_handshake_enabled: options.token_handshake_enabled,
                json_probe_mode: match options.json_probe_mode {
                    Nat4RuntimeJsonProbeMode::Disabled => JsonProbeMode::Disabled,
                    Nat4RuntimeJsonProbeMode::ProbeResponder => JsonProbeMode::Nat4ProbeResponder {
                        socket_id,
                        next_seq: Arc::new(AtomicU64::new(0)),
                    },
                    Nat4RuntimeJsonProbeMode::HandshakeResponder => {
                        JsonProbeMode::Nat4HandshakeResponder {
                            socket_id,
                            next_seq: Arc::new(AtomicU64::new(0)),
                        }
                    }
                },
                echo_content_ack: options.echo_content_ack,
                debug_converge_lease: args.debug_converge_lease,
                manual_converge_socket: manual_converge.as_ref().map(|coordinator| {
                    ManualConvergeRecvState {
                        socket_id,
                        coordinator: coordinator.clone(),
                    }
                }),
            };
            tokio::spawn(async move {
                let r = recv_loop(socket, &shared, opts).await;
                info!("recv finished [{r:?}]");
            })
        };
        recv_tasks.push(recv_task);

        let sender = UdpSender {
            socket_id,
            socket: socket.clone(),
            payload: if args.debug_converge_lease {
                ProbePayload::Nat4Token {
                    socket_id,
                    generation: 0,
                    next_seq: AtomicU64::new(0),
                }
            } else {
                ProbePayload::Plain(text.clone())
            },
            target,
            local,
        };

        if args.debug_converge_lease {
            sender
                .warm_up(ttl)
                .await
                .with_context(|| "warm_up failed")?;
            if let Some(coordinator) = manual_converge.as_ref() {
                let ready_at = coordinator.lock().mark_warm_done(socket_id, Instant::now());
                debug!("manual converge socket [{socket_id}] warming done [{local}]");
                if let Some(ready_at) = ready_at {
                    debug!("manual converge warm barrier ready at [{ready_at:?}]");
                }
            }
        } else if let Some(ttl) = ttl {
            sender
                .prepare_ttl(ttl)
                .await
                .with_context(|| "prepare_ttl failed")?;
            if let Some(coordinator) = manual_converge.as_ref() {
                let ready_at = coordinator.lock().mark_warm_done(socket_id, Instant::now());
                debug!("manual converge socket [{socket_id}] warm-up done [{local}]");
                if let Some(ready_at) = ready_at {
                    debug!("manual converge warm barrier ready at [{ready_at:?}]");
                }
            }
        }

        senders.push(sender);
    }

    Ok(Nat4ProbeRuntime {
        target,
        interval,
        ttl,
        start_at,
        shared,
        senders,
        recv_tasks,
        manual_converge,
    })
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

#[derive(Clone)]
struct ManualConvergeRecvState {
    socket_id: u64,
    coordinator: Arc<Mutex<ManualConvergeCoordinator>>,
}

#[derive(Clone)]
struct RecvLoopOptions {
    role: HardNatRole,
    expected_text: Arc<String>,
    promote_hit_ttl: Option<u32>,
    token_handshake_enabled: bool,
    json_probe_mode: JsonProbeMode,
    echo_content_ack: bool,
    debug_converge_lease: bool,
    manual_converge_socket: Option<ManualConvergeRecvState>,
}

async fn recv_loop(
    socket: Arc<UdpSocket>,
    shared: &Arc<Shared>,
    opts: RecvLoopOptions,
) -> Result<()> {
    let local = socket
        .local_addr()
        .with_context(|| "get local address failed")?;

    let mut buf = vec![0_u8; 1700];
    let mut ttl_promoted = false;
    loop {
        let (len, from) = socket
            .recv_from(&mut buf)
            .await
            .with_context(|| "recv_from failed")?;
        let packet = &buf[..len];
        let packet_kind = classify_probe_packet(packet, opts.expected_text.as_str());
        let action = decide_recv_probe_action(
            opts.role,
            opts.token_handshake_enabled,
            &opts.json_probe_mode,
            &packet_kind,
        );
        let manual_json_handshake_token = match (&action, opts.manual_converge_socket.as_ref()) {
            (RecvProbeAction::AcceptJsonHandshakeAck(packet), Some(manual)) => {
                let coordinator = manual.coordinator.lock();
                validate_manual_json_handshake_ack_packet(
                    &coordinator,
                    manual.socket_id,
                    packet,
                    local,
                    from,
                )
            }
            _ => None,
        };
        let should_handle_probe = match &action {
            RecvProbeAction::AcceptJsonHandshakeAck(_) => manual_json_handshake_token.is_some(),
            _ => action.is_valid_probe(),
        };
        if opts.debug_converge_lease {
            log_debug_converge_recv(
                opts.role,
                &packet_kind,
                local,
                from,
                len,
                &action,
                packet,
                opts.expected_text.as_str(),
            );
        }
        if should_handle_probe {
            let should_echo_content =
                matches!(action, RecvProbeAction::AcceptContent) && opts.echo_content_ack;
            let json_probe_ack = match &action {
                RecvProbeAction::ReplyJsonProbeAck { session_id } => Some(*session_id),
                _ => None,
            };
            let json_handshake_ack = match &action {
                RecvProbeAction::ReplyJsonHandshakeAck {
                    session_id,
                    ack_of,
                    handshake_stage,
                } => Some((*session_id, *ack_of, *handshake_stage)),
                _ => None,
            };
            if !ttl_promoted {
                if let Some(ttl) = opts.promote_hit_ttl {
                    socket
                        .set_ttl(ttl)
                        .with_context(|| format!("set recv-loop promoted ttl [{ttl}] failed"))?;
                    ttl_promoted = true;
                    debug!(
                        "promoted recv socket ttl [{local}] => [{ttl}] after first valid packet from [{from}]"
                    );
                }
            }
            if let Some(session_id) = json_probe_ack {
                let (socket_id, next_seq) = match &opts.json_probe_mode {
                    JsonProbeMode::Nat4ProbeResponder {
                        socket_id,
                        next_seq,
                    }
                    | JsonProbeMode::Nat4HandshakeResponder {
                        socket_id,
                        next_seq,
                    } => (*socket_id, next_seq),
                    _ => bail!("json probe ack requested without nat4 responder mode"),
                };
                let payload = encode_hard_nat_udp_packet(&build_hard_nat_probe_ack_packet(
                    session_id,
                    socket_id,
                    next_seq.fetch_add(1, Ordering::Relaxed),
                ))?;
                socket
                    .send_to(payload.as_bytes(), from)
                    .await
                    .with_context(|| "send json probe ack failed")?;
                debug!("send json probe ack [{local}] => [{from}], session_id [{session_id}]");
            } else if let Some((session_id, ack_of, handshake_stage)) = json_handshake_ack {
                let nat3_tuple_addr = if opts.role == HardNatRole::Nat3 {
                    shared.nat3_current_public_addr().unwrap_or(local)
                } else {
                    local
                };
                if opts.role == HardNatRole::Nat3
                    && handshake_stage == HardNatHandshakeStage::Candidate
                {
                    shared.record_nat3_candidate_observation(
                        from,
                        Nat3CandidateObservation {
                            nat3_addr: nat3_tuple_addr,
                            nat4_addr: from,
                            socket_id: ack_of.socket_id,
                            generation: ack_of.generation,
                        },
                    );
                }
                let next_seq = match &opts.json_probe_mode {
                    JsonProbeMode::Nat3HandshakeResponder { next_seq } => next_seq,
                    _ => bail!("json handshake ack requested without nat3 handshake mode"),
                };
                let payload = encode_hard_nat_udp_packet(&build_hard_nat_handshake_ack_packet(
                    session_id,
                    next_seq.fetch_add(1, Ordering::Relaxed),
                    handshake_stage,
                    ack_of,
                    nat3_tuple_addr,
                    from,
                ))?;
                socket
                    .send_to(payload.as_bytes(), from)
                    .await
                    .with_context(|| "send json handshake ack failed")?;
                debug!(
                    "send json handshake_ack [{local}] => [{from}], session_id [{session_id}], ack_of [{ack_of:?}]"
                );
            } else if action.should_echo() || should_echo_content {
                socket
                    .send_to(packet, from)
                    .await
                    .with_context(|| "echo probe packet failed")?;
                if matches!(action, RecvProbeAction::EchoNat4Token(_)) {
                    debug!("echo probe token [{local}] => [{from}], bytes [{len}]");
                } else if should_echo_content {
                    debug!("echo probe content [{local}] => [{from}], bytes [{len}]");
                }
            }
            if let Some(manual) = opts.manual_converge_socket.as_ref() {
                let now = Instant::now();
                let mut coordinator = manual.coordinator.lock();
                if let RecvProbeAction::ReplyJsonProbeAck { session_id } = &action {
                    coordinator.record_json_probe_session_id(manual.socket_id, *session_id);
                }
                if let RecvProbeAction::AcceptNat4TokenEcho(token) = &action {
                    let _ = coordinator.record_validation_echo(manual.socket_id, *token, now);
                }
                if let Some(token) = manual_json_handshake_token {
                    let _ = coordinator.record_validation_echo(manual.socket_id, token, now);
                }
                let _ = coordinator.record_probe_hit(manual.socket_id, now);
            }
            let should_record_connected = match &action {
                RecvProbeAction::AcceptJsonProbeAck(_) => {
                    !opts.debug_converge_lease && !opts.token_handshake_enabled
                }
                RecvProbeAction::ReplyJsonHandshakeAck { .. } => false,
                _ => true,
            };
            let old = if should_record_connected {
                shared.record_connected(
                    from,
                    socket.clone(),
                    opts.manual_converge_socket.as_ref().map(|x| x.socket_id),
                )
            } else {
                None
            };
            if !opts.debug_converge_lease {
                match &action {
                    RecvProbeAction::AcceptContent => {
                        info!(
                            "recv text [{local}] <= [{from}], text [{}]",
                            opts.expected_text.as_str()
                        );
                    }
                    RecvProbeAction::AcceptJsonProbeAck(packet) => {
                        info!(
                            "recv json probe_ack [{local}] <= [{from}], session_id [{}], sender [{:?}]",
                            packet.session_id,
                            packet.sender
                        );
                    }
                    RecvProbeAction::ReplyJsonProbeAck { session_id } => {
                        info!(
                            "recv json probe_req [{local}] <= [{from}], session_id [{session_id}]"
                        );
                    }
                    RecvProbeAction::AcceptJsonHandshakeAck(packet) => {
                        info!(
                            "recv json handshake_ack [{local}] <= [{from}], session_id [{}], sender [{:?}], ack_of [{:?}], stage [{:?}]",
                            packet.session_id,
                            packet.sender,
                            packet.ack_of,
                            packet.handshake_stage,
                        );
                    }
                    RecvProbeAction::ReplyJsonHandshakeAck {
                        session_id,
                        ack_of,
                        handshake_stage,
                    } => {
                        info!(
                            "recv json handshake_req [{local}] <= [{from}], session_id [{session_id}], ack_of [{ack_of:?}], stage [{handshake_stage:?}]"
                        );
                    }
                    RecvProbeAction::AcceptNat4TokenEcho(token)
                    | RecvProbeAction::EchoNat4Token(token) => {
                        debug!("recv probe token [{local}] <= [{from}], token [{token:?}]");
                    }
                    RecvProbeAction::Ignore => {}
                }
            }
            maybe_log_connected_from_target(should_record_connected, old.is_none(), from);
        } else {
            if !opts.debug_converge_lease {
                info!("recv unknown [{local}] <= [{from}], bytes [{len}]");
            }
        }
    }
}

#[derive(Default)]
struct RecvTaskGuard {
    tasks: Vec<JoinHandle<()>>,
}

impl RecvTaskGuard {
    fn new(tasks: Vec<JoinHandle<()>>) -> Self {
        Self { tasks }
    }

    fn with_capacity(capacity: usize) -> Self {
        Self {
            tasks: Vec::with_capacity(capacity),
        }
    }

    fn push(&mut self, task: JoinHandle<()>) {
        self.tasks.push(task);
    }

    async fn abort_and_wait(mut self) {
        for task in self.tasks.drain(..) {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for RecvTaskGuard {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

struct UdpSender {
    socket_id: u64,
    socket: Arc<UdpSocket>,
    target: SocketAddr,
    payload: ProbePayload,
    local: SocketAddr,
}

enum ProbePayload {
    Plain(Arc<String>),
    Nat4Token {
        socket_id: u64,
        generation: u64,
        next_seq: AtomicU64,
    },
}

impl ProbePayload {
    fn next_text(&self) -> String {
        match self {
            Self::Plain(text) => text.to_string(),
            Self::Nat4Token {
                socket_id,
                generation,
                next_seq,
            } => encode_probe_token(ProbeToken {
                role: HardNatRole::Nat4,
                socket_id: *socket_id,
                generation: *generation,
                seq: next_seq.fetch_add(1, Ordering::Relaxed),
            }),
        }
    }
}

impl UdpSender {
    async fn send_one(&self) -> Result<()> {
        let payload = self.payload.next_text();
        self.send_payload_to(self.target, &payload, None).await
    }

    async fn send_one_to(&self, target: SocketAddr) -> Result<()> {
        let payload = self.payload.next_text();
        self.send_payload_to(target, &payload, None).await
    }

    async fn send_token(&self, token: ProbeToken) -> Result<()> {
        let payload = encode_probe_token(token);
        self.send_payload_to(self.target, &payload, Some(token))
            .await?;
        Ok(())
    }

    async fn send_json_handshake_req_from_token(
        &self,
        session_id: u64,
        token: ProbeToken,
    ) -> Result<()> {
        self.send_json_handshake_req_from_token_to(self.target, session_id, token)
            .await
    }

    async fn send_json_handshake_req_from_token_to(
        &self,
        target: SocketAddr,
        session_id: u64,
        token: ProbeToken,
    ) -> Result<()> {
        let payload = encode_hard_nat_udp_packet(&build_hard_nat_handshake_req_packet_from_token(
            session_id, token,
        ))?;
        self.send_payload_to(target, &payload, None).await?;
        debug!(
            "send json handshake_req [{}] => [{}], session_id [{}], token [{token:?}]",
            self.local, target, session_id
        );
        Ok(())
    }

    async fn send_token_to(&self, target: SocketAddr, token: ProbeToken) -> Result<()> {
        let payload = encode_probe_token(token);
        self.send_payload_to(target, &payload, Some(token)).await?;
        Ok(())
    }

    async fn send_payload_to(
        &self,
        target: SocketAddr,
        payload: &str,
        token: Option<ProbeToken>,
    ) -> Result<()> {
        let len = self
            .socket
            .send_to(payload.as_bytes(), target)
            .await
            .with_context(|| "send_to failed")?;
        debug!("sent to [{}] => [{}]: bytes [{len}]", self.local, target);
        if let Some(token) = token {
            log_manual_converge_token_send(self.local, target, token);
        }
        Ok(())
    }

    async fn warm_up(&self, max_ttl: Option<u32>) -> Result<()> {
        if let Some(max_ttl) = max_ttl {
            for ttl in 1..=max_ttl {
                self.socket
                    .set_ttl(ttl)
                    .with_context(|| format!("failed to set_ttl [{ttl}]"))?;
                self.send_one().await?;
            }
            return Ok(());
        }

        self.send_one().await
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Nat3CandidateObservation {
    nat3_addr: SocketAddr,
    nat4_addr: SocketAddr,
    socket_id: u64,
    generation: u64,
}

#[derive(Default)]
struct Shared {
    connecteds: Mutex<HashMap<SocketAddr, Arc<UdpSocket>>>,
    connecteds_by_socket_id: Mutex<HashMap<u64, (SocketAddr, Arc<UdpSocket>)>>,
    nat3_candidate_observed_remotes: Mutex<HashMap<SocketAddr, Nat3CandidateObservation>>,
    nat3_current_public_addr: Mutex<Option<SocketAddr>>,
}

impl Shared {
    fn has_connected(&self) -> bool {
        !self.connecteds.lock().is_empty()
    }

    fn has_nat3_candidate_observation(&self, remote_addr: SocketAddr) -> bool {
        self.nat3_candidate_observed_remotes
            .lock()
            .contains_key(&remote_addr)
    }

    fn nat3_candidate_observation(
        &self,
        remote_addr: SocketAddr,
    ) -> Option<Nat3CandidateObservation> {
        self.nat3_candidate_observed_remotes
            .lock()
            .get(&remote_addr)
            .copied()
    }

    fn record_nat3_candidate_observation(
        &self,
        remote_addr: SocketAddr,
        observation: Nat3CandidateObservation,
    ) -> Option<Nat3CandidateObservation> {
        self.nat3_candidate_observed_remotes
            .lock()
            .insert(remote_addr, observation)
    }

    fn set_nat3_current_public_addr(&self, addr: Option<SocketAddr>) {
        *self.nat3_current_public_addr.lock() = addr;
    }

    fn nat3_current_public_addr(&self) -> Option<SocketAddr> {
        *self.nat3_current_public_addr.lock()
    }

    fn connected_count(&self) -> usize {
        self.connecteds.lock().len()
    }

    fn record_connected(
        &self,
        remote_addr: SocketAddr,
        socket: Arc<UdpSocket>,
        socket_id: Option<u64>,
    ) -> Option<Arc<UdpSocket>> {
        if let Some(socket_id) = socket_id {
            self.connecteds_by_socket_id
                .lock()
                .insert(socket_id, (remote_addr, socket.clone()));
        }
        self.connecteds.lock().insert(remote_addr, socket)
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

    fn connected_conn_for_socket_id(
        &self,
        role: HardNatRole,
        socket_id: u64,
        start_at: Instant,
    ) -> Option<HardNatConnectedSocket> {
        let by_socket = self.connecteds_by_socket_id.lock();
        let (remote_addr, socket) = by_socket.get(&socket_id)?;
        let local_addr = socket.local_addr().ok()?;
        Some(HardNatConnectedSocket {
            role,
            socket: socket.clone(),
            local_addr,
            remote_addr: *remote_addr,
            elapsed: start_at.elapsed(),
        })
    }

    fn connected_conn_for_remote(
        &self,
        role: HardNatRole,
        remote_addr: SocketAddr,
        start_at: Instant,
    ) -> Option<HardNatConnectedSocket> {
        let from_addrs = self.connecteds.lock();
        let socket = from_addrs.get(&remote_addr)?;
        let local_addr = socket.local_addr().ok()?;
        Some(HardNatConnectedSocket {
            role,
            socket: socket.clone(),
            local_addr,
            remote_addr,
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

async fn send_conn_loop_keep_recv(
    socket: Arc<UdpSocket>,
    target: SocketAddr,
    text: &str,
    interval: Duration,
    _recv_tasks: RecvTaskGuard,
) -> Result<()> {
    send_conn_loop(socket, target, text, interval).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stun::{
        async_udp::{tokio_socket_bind, AsyncUdpSocket},
        stun::{decode_message, try_binding_response_bytes},
    };
    use std::collections::VecDeque;
    use std::fs::File;
    use std::io::{self, BufRead, Cursor, Read, Write};
    use std::net::{Ipv4Addr, SocketAddrV4};
    use std::os::fd::FromRawFd;
    use std::sync::{mpsc as std_mpsc, Arc, Mutex};
    use tokio::sync::{broadcast, mpsc};
    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Clone, Default)]
    struct SharedLogBuffer(Arc<Mutex<Vec<u8>>>);

    struct SharedLogWriter(SharedLogBuffer);

    impl<'a> MakeWriter<'a> for SharedLogBuffer {
        type Writer = SharedLogWriter;

        fn make_writer(&'a self) -> Self::Writer {
            SharedLogWriter(self.clone())
        }
    }

    impl io::Write for SharedLogWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                 .0
                .lock()
                .expect("lock shared log buffer")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn capture_logs(f: impl FnOnce()) -> String {
        let logs = SharedLogBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(logs.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        let bytes = logs.0.lock().expect("lock shared log buffer").clone();
        String::from_utf8(bytes).expect("utf8 logs")
    }

    /// 模拟 stdin 在异步场景下先遇到 EOF（`read_line` 返回 0），再提供用户换行。
    struct SequencedBufRead {
        responses: VecDeque<ReaderResponse>,
        buffer: Vec<u8>,
        saw_empty: bool,
    }

    #[derive(Clone, Copy)]
    enum ReaderResponse {
        Empty,
        Data(&'static [u8]),
    }

    impl SequencedBufRead {
        fn new(responses: Vec<ReaderResponse>) -> Self {
            Self {
                responses: VecDeque::from(responses),
                buffer: Vec::new(),
                saw_empty: false,
            }
        }

        fn saw_empty(&self) -> bool {
            self.saw_empty
        }

        fn responses_remaining(&self) -> usize {
            self.responses.len()
        }
    }

    impl Read for SequencedBufRead {
        fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
            let buf = self.fill_buf()?;
            if buf.is_empty() {
                return Ok(0);
            }
            let amt = buf.len().min(dst.len());
            dst[..amt].copy_from_slice(&buf[..amt]);
            self.consume(amt);
            Ok(amt)
        }
    }

    impl BufRead for SequencedBufRead {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            if self.buffer.is_empty() {
                while let Some(response) = self.responses.pop_front() {
                    match response {
                        ReaderResponse::Empty => {
                            self.saw_empty = true;
                            return Ok(&[]);
                        }
                        ReaderResponse::Data(bytes) => {
                            self.buffer.extend_from_slice(bytes);
                            break;
                        }
                    }
                }
            }
            Ok(&self.buffer)
        }

        fn consume(&mut self, amt: usize) {
            self.buffer.drain(..amt);
        }
    }

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
            discover_public_addr: false,
            pause_after_discovery: false,
            hold_batch_until_enter: false,
            debug_converge_lease: false,
            stun_servers: Vec::new(),
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
            dump_public_addrs: false,
            debug_keep_recv: false,
            debug_promote_hit_ttl: None,
            debug_converge_lease: false,
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("interval"));
    }

    #[test]
    fn nat3_validate_rejects_excessive_scan_count() {
        let cfg = Nat3RunConfig {
            content: None,
            target_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            count: (HARD_NAT_MAX_SCAN_COUNT as usize) + 1,
            listen: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)).to_string(),
            ttl: None,
            interval: Duration::from_millis(100),
            batch_interval: Duration::from_millis(1000),
            discover_public_addr: false,
            pause_after_discovery: false,
            hold_batch_until_enter: false,
            debug_converge_lease: false,
            stun_servers: Vec::new(),
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("max"));
    }

    #[test]
    fn pause_after_discovery_requires_discovery() {
        let cfg = Nat3RunConfig {
            content: None,
            target_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            count: 4,
            listen: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)).to_string(),
            ttl: None,
            interval: Duration::from_millis(100),
            batch_interval: Duration::from_millis(1000),
            discover_public_addr: false,
            pause_after_discovery: true,
            hold_batch_until_enter: false,
            debug_converge_lease: false,
            stun_servers: Vec::new(),
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("discover_public_addr"), "{err}");
    }

    #[test]
    fn nat4_validate_rejects_excessive_ttl() {
        let cfg = Nat4RunConfig {
            content: None,
            target: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 12345)),
            count: 4,
            ttl: Some(HARD_NAT_MAX_TTL + 1),
            interval: Duration::from_millis(10),
            dump_public_addrs: false,
            debug_keep_recv: false,
            debug_promote_hit_ttl: None,
            debug_converge_lease: false,
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("ttl"));
    }

    #[test]
    fn nat4_validate_rejects_debug_promote_hit_ttl_without_debug_keep_recv() {
        let cfg = Nat4RunConfig {
            content: None,
            target: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 12345)),
            count: 4,
            ttl: Some(4),
            interval: Duration::from_millis(10),
            dump_public_addrs: false,
            debug_keep_recv: false,
            debug_promote_hit_ttl: Some(64),
            debug_converge_lease: false,
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("debug_keep_recv"), "{err}");
    }

    #[test]
    fn pause_after_discovery_prompt_message() {
        let mut input = Cursor::new(b"\n".to_vec());
        let mut output = Vec::new();
        wait_for_enter_after_discovery(&mut input, &mut output).unwrap();

        let prompt = String::from_utf8(output).expect("utf8 prompt");
        assert_eq!(
            prompt,
            "nat3 discovery finished, press Enter to start probing\n"
        );
    }

    #[test]
    fn pause_after_discovery_requires_enter_even_after_eof() {
        let mut reader =
            SequencedBufRead::new(vec![ReaderResponse::Empty, ReaderResponse::Data(b"\n")]);
        let mut output = Vec::new();
        wait_for_enter_after_discovery(&mut reader, &mut output).unwrap();

        assert!(reader.saw_empty(), "EOF should be observed before newline");
        assert!(
            !output.is_empty(),
            "prompt should still be written when loop retries"
        );
        assert_eq!(
            reader.responses_remaining(),
            0,
            "newline must be consumed before returning"
        );
    }

    #[test]
    fn hold_batch_until_enter_prompt_message() {
        let mut input = Cursor::new(b"\n".to_vec());
        let mut output = Vec::new();
        wait_for_enter_before_nat3_batch_reroll(&mut input, &mut output).unwrap();

        let prompt = String::from_utf8(output).expect("utf8 prompt");
        assert_eq!(
            prompt,
            "nat3 batch probing active, press Enter to reroll target ports\n"
        );
    }

    #[test]
    fn hold_batch_until_enter_requires_enter_even_after_eof() {
        let mut reader =
            SequencedBufRead::new(vec![ReaderResponse::Empty, ReaderResponse::Data(b"\n")]);
        let mut output = Vec::new();
        wait_for_enter_before_nat3_batch_reroll(&mut reader, &mut output).unwrap();

        assert!(reader.saw_empty(), "EOF should be observed before newline");
        assert!(
            !output.is_empty(),
            "prompt should still be written when loop retries"
        );
        assert_eq!(
            reader.responses_remaining(),
            0,
            "newline must be consumed before returning"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hold_batch_loop_repeats_same_batch_until_reroll() {
        let batch_calls = Arc::new(Mutex::new(Vec::new()));
        let (reroll_tx, mut reroll_rx) = mpsc::unbounded_channel();
        let batch_epoch = 1_u64;
        hold_batch_send_loop(
            {
                let calls = batch_calls.clone();
                let reroll_tx = reroll_tx.clone();
                move || {
                    let calls = calls.clone();
                    let reroll_tx = reroll_tx.clone();
                    async move {
                        let mut guard = calls.lock().unwrap();
                        guard.push(1);
                        if guard.len() == 3 {
                            reroll_tx
                                .send(batch_epoch)
                                .expect("send reroll signal after third batch send");
                        }
                        Ok::<(), anyhow::Error>(())
                    }
                }
            },
            wait_for_nat3_reroll_signal(&mut reroll_rx, batch_epoch),
            Duration::from_millis(1),
        )
        .await
        .unwrap();
        let guard = batch_calls.lock().unwrap();
        assert_eq!(guard.len(), 3);
        assert!(guard.iter().all(|&batch_id| batch_id == 1));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hold_batch_loop_ignores_stale_signal_before_batch_start() {
        let batch_calls = Arc::new(Mutex::new(Vec::new()));
        let (reroll_tx, mut reroll_rx) = mpsc::unbounded_channel();
        let stale_epoch = 1_u64;
        let batch_epoch = 2_u64;
        reroll_tx
            .send(stale_epoch)
            .expect("queue stale signal before batch starts");
        hold_batch_send_loop(
            {
                let calls = batch_calls.clone();
                let reroll_tx = reroll_tx.clone();
                move || {
                    let calls = calls.clone();
                    let reroll_tx = reroll_tx.clone();
                    async move {
                        let mut guard = calls.lock().unwrap();
                        guard.push(1);
                        if guard.len() == 3 {
                            reroll_tx
                                .send(batch_epoch)
                                .expect("send fresh signal for current batch");
                        }
                        Ok::<(), anyhow::Error>(())
                    }
                }
            },
            wait_for_nat3_reroll_signal(&mut reroll_rx, batch_epoch),
            Duration::from_millis(1),
        )
        .await
        .unwrap();

        let guard = batch_calls.lock().unwrap();
        assert_eq!(
            guard.len(),
            3,
            "batch should keep sending until a newer signal arrives"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hold_batch_loop_ignores_stale_signal_that_arrives_after_discard() {
        let batch_calls = Arc::new(Mutex::new(Vec::new()));
        let (reroll_tx, mut reroll_rx) = mpsc::unbounded_channel();
        let stale_epoch = 1_u64;
        let batch_epoch = 2_u64;
        reroll_tx
            .send(stale_epoch)
            .expect("queue stale signal after previous batch boundary");
        hold_batch_send_loop(
            {
                let calls = batch_calls.clone();
                let reroll_tx = reroll_tx.clone();
                move || {
                    let calls = calls.clone();
                    let reroll_tx = reroll_tx.clone();
                    async move {
                        let mut guard = calls.lock().unwrap();
                        guard.push(1);
                        if guard.len() == 3 {
                            reroll_tx
                                .send(batch_epoch)
                                .expect("send fresh signal for current batch");
                        }
                        Ok::<(), anyhow::Error>(())
                    }
                }
            },
            wait_for_nat3_reroll_signal(&mut reroll_rx, batch_epoch),
            Duration::from_millis(1),
        )
        .await
        .unwrap();

        let guard = batch_calls.lock().unwrap();
        assert_eq!(
            guard.len(),
            3,
            "stale signal that arrives after discard should not reroll the current batch"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hold_batch_loop_rerolls_when_signal_is_already_pending_for_current_batch() {
        let batch_calls = Arc::new(Mutex::new(Vec::new()));
        let (reroll_tx, mut reroll_rx) = mpsc::unbounded_channel();
        let batch_epoch = 1_u64;
        reroll_tx
            .send(batch_epoch)
            .expect("queue signal that belongs to current batch");
        hold_batch_send_loop(
            {
                let calls = batch_calls.clone();
                move || {
                    let calls = calls.clone();
                    async move {
                        calls.lock().unwrap().push(1);
                        Ok::<(), anyhow::Error>(())
                    }
                }
            },
            wait_for_nat3_reroll_signal(&mut reroll_rx, batch_epoch),
            Duration::from_millis(1),
        )
        .await
        .unwrap();

        let guard = batch_calls.lock().unwrap();
        assert_eq!(
            guard.len(),
            1,
            "signal queued for the current batch should reroll after the first send"
        );
    }

    #[test]
    fn hold_batch_enter_listener_drop_stops_polling_thread() {
        let (read_fd, write_fd) = nix::unistd::pipe().expect("create pipe for listener test");
        let read_file = unsafe { File::from_raw_fd(read_fd) };
        let _write_file = unsafe { File::from_raw_fd(write_fd) };
        let (done_tx, done_rx) = std_mpsc::channel();

        std::thread::spawn(move || {
            let listener = Nat3RerollEnterListener::spawn_with_reader(read_file);
            drop(listener);
            done_tx.send(()).expect("report listener drop completion");
        });

        assert!(
            done_rx.recv_timeout(Duration::from_millis(500)).is_ok(),
            "dropping the listener should not hang waiting for stdin"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hold_batch_enter_listener_begin_epoch_ignores_preexisting_stale_enters() {
        let (read_fd, write_fd) = nix::unistd::pipe().expect("create pipe for listener test");
        let read_file = unsafe { File::from_raw_fd(read_fd) };
        let mut write_file = unsafe { File::from_raw_fd(write_fd) };
        let mut listener = Nat3RerollEnterListener::spawn_with_reader(read_file);

        write_file
            .write_all(b"\n\n")
            .expect("write stale enters before starting batch");

        let (batch_epoch, _discarded_enters) = listener.begin_epoch().await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), listener.recv_epoch(batch_epoch))
                .await
                .is_err(),
            "stale enters buffered before batch start must be discarded"
        );

        write_file
            .write_all(b"\n")
            .expect("write fresh enter for current batch");
        tokio::time::timeout(Duration::from_millis(200), listener.recv_epoch(batch_epoch))
            .await
            .expect("fresh current-batch enter should arrive in time")
            .unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hold_batch_enter_listener_discovery_enter_does_not_reroll_next_epoch() {
        let (read_fd, write_fd) = nix::unistd::pipe().expect("create pipe for listener test");
        let read_file = unsafe { File::from_raw_fd(read_fd) };
        let mut write_file = unsafe { File::from_raw_fd(write_fd) };
        let mut listener = Nat3RerollEnterListener::spawn_with_reader(read_file);

        let (discovery_epoch, _discarded_enters) = listener.begin_epoch().await.unwrap();
        write_file
            .write_all(b"\n")
            .expect("write discovery enter for current epoch");
        tokio::time::timeout(
            Duration::from_millis(200),
            listener.recv_epoch(discovery_epoch),
        )
        .await
        .expect("discovery enter should arrive in time")
        .unwrap();

        let (batch_epoch, discarded_enters) = listener.begin_epoch().await.unwrap();
        assert_eq!(
            discarded_enters, 0,
            "consumed discovery enter should not remain queued for the next epoch"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), listener.recv_epoch(batch_epoch))
                .await
                .is_err(),
            "discovery enter must not automatically reroll the next epoch"
        );

        write_file
            .write_all(b"\n")
            .expect("write fresh enter for batch epoch");
        tokio::time::timeout(Duration::from_millis(200), listener.recv_epoch(batch_epoch))
            .await
            .expect("fresh batch enter should arrive in time")
            .unwrap();
    }

    #[test]
    fn debug_converge_logs_unknown_preview() {
        let logs = capture_logs(|| {
            log_debug_converge_recv(
                HardNatRole::Nat3,
                &ProbePacketKind::Unknown,
                "127.0.0.1:12345".parse().unwrap(),
                "203.0.113.1:54321".parse().unwrap(),
                4,
                &RecvProbeAction::Ignore,
                b"\x01\x02\x03\x04",
                "nat hello",
            );
        });
        assert!(logs.contains("manual converge recv unknown"));
        assert!(logs.contains("preview [01 02 03 04]"));
        assert!(logs.contains("classification [Unknown]"));
        assert!(logs.contains("decision [Ignore]"));
    }

    #[test]
    fn debug_converge_logs_nat4_token() {
        let token = ProbeToken {
            role: HardNatRole::Nat4,
            socket_id: 7,
            generation: 1,
            seq: 2,
        };
        let logs = capture_logs(|| {
            log_debug_converge_recv(
                HardNatRole::Nat3,
                &ProbePacketKind::Token(token),
                "127.0.0.1:12345".parse().unwrap(),
                "203.0.113.1:54321".parse().unwrap(),
                17,
                &RecvProbeAction::AcceptNat4TokenEcho(token),
                b"token bytes",
                "nat hello",
            );
        });
        assert!(logs.contains("manual converge recv token"));
        assert!(logs.contains("token [ProbeToken"));
        assert!(logs.contains("classification [Token]"));
        assert!(logs.contains("decision [AcceptNat4TokenEcho"));
    }

    #[test]
    fn manual_converge_send_token_logs_metadata() {
        let local: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let target: SocketAddr = "203.0.113.2:2".parse().unwrap();
        let token = ProbeToken {
            role: HardNatRole::Nat4,
            socket_id: 5,
            generation: 3,
            seq: 9,
        };
        let logs = capture_logs(|| log_manual_converge_token_send(local, target, token));
        assert!(logs.contains("manual converge send token"));
        assert!(logs.contains("local [127.0.0.1:1]"));
        assert!(logs.contains("target [203.0.113.2:2]"));
        assert!(logs.contains("socket [5]"));
        assert!(logs.contains("generation [3]"));
        assert!(logs.contains("seq [9]"));
    }

    #[test]
    fn manual_converge_validation_failure_logs_context() {
        let mut state =
            ManualConvergeCoordinator::new(1, Duration::from_millis(1), Duration::from_millis(0));
        let owner = 0;
        let generation = 17;
        let now = Instant::now();
        state.lease_owner = Some(owner);
        state.current_generation = generation;
        let mut socket = Nat4ProbeSocketState::new(owner);
        socket.phase = Nat4SocketPhase::LeaseOwnerValidating { generation };
        socket.validation_echo_count = 1;
        socket.last_validation_sent_seq = Some(42);
        socket.last_validation_matched_seq = Some(40);
        state.sockets.insert(owner, socket);

        let logs = capture_logs(|| state.release_failed_lease(owner, generation, now));
        assert!(logs.contains("manual converge validation failed"));
        assert!(logs.contains("generation [17]"));
        assert!(logs.contains("validation_window"));
        assert!(logs.contains("cooldown"));
        assert!(logs.contains("echo [1/"));
        assert!(logs.contains("last_sent [Some(42)]"));
        assert!(logs.contains("last_matched [Some(40)]"));
    }

    #[test]
    fn probe_token_roundtrip() {
        let s = encode_probe_token(ProbeToken {
            role: HardNatRole::Nat4,
            socket_id: 0x1234_abcd,
            generation: 0x55,
            seq: 0x77,
        });
        let token = decode_probe_token(&s).unwrap();
        assert_eq!(
            token,
            ProbeToken {
                role: HardNatRole::Nat4,
                socket_id: 0x1234_abcd,
                generation: 0x55,
                seq: 0x77,
            }
        );
    }

    #[test]
    fn probe_token_rejects_invalid_prefix() {
        assert!(decode_probe_token("nat4:1234").is_none());
        assert!(decode_probe_token("hn0 nat4 1 2 3").is_none());
    }

    #[test]
    fn hard_nat_udp_json_roundtrip_for_all_packet_types() {
        let probe_req = HardNatUdpPacket {
            v: HARD_NAT_UDP_JSON_VERSION.to_string(),
            packet_type: HardNatUdpPacketType::ProbeReq,
            session_id: 101,
            sender: HardNatUdpSender {
                role: HardNatRole::Nat3,
                socket_id: 0,
                generation: 0,
                seq: 11,
            },
            handshake_stage: None,
            ack_of: None,
            tuple: None,
        };
        let probe_ack = HardNatUdpPacket {
            v: HARD_NAT_UDP_JSON_VERSION.to_string(),
            packet_type: HardNatUdpPacketType::ProbeAck,
            session_id: 101,
            sender: HardNatUdpSender {
                role: HardNatRole::Nat4,
                socket_id: 7,
                generation: 0,
                seq: 12,
            },
            handshake_stage: None,
            ack_of: None,
            tuple: None,
        };
        let handshake_req = HardNatUdpPacket {
            v: HARD_NAT_UDP_JSON_VERSION.to_string(),
            packet_type: HardNatUdpPacketType::HandshakeReq,
            session_id: 101,
            sender: HardNatUdpSender {
                role: HardNatRole::Nat4,
                socket_id: 7,
                generation: 2,
                seq: 13,
            },
            handshake_stage: Some(HardNatHandshakeStage::Candidate),
            ack_of: None,
            tuple: None,
        };
        let handshake_ack = HardNatUdpPacket {
            v: HARD_NAT_UDP_JSON_VERSION.to_string(),
            packet_type: HardNatUdpPacketType::HandshakeAck,
            session_id: 101,
            sender: HardNatUdpSender {
                role: HardNatRole::Nat3,
                socket_id: 0,
                generation: 0,
                seq: 14,
            },
            handshake_stage: Some(HardNatHandshakeStage::Candidate),
            ack_of: Some(HardNatUdpSender {
                role: HardNatRole::Nat4,
                socket_id: 7,
                generation: 2,
                seq: 13,
            }),
            tuple: Some(HardNatTuple {
                nat3_addr: "1.2.3.4:50000".parse().unwrap(),
                nat4_addr: "8.8.8.8:40000".parse().unwrap(),
            }),
        };

        for packet in [probe_req, probe_ack, handshake_req, handshake_ack] {
            let encoded = encode_hard_nat_udp_packet(&packet).unwrap();
            let decoded = decode_hard_nat_udp_packet(&encoded).unwrap();
            assert_eq!(decoded, packet);
        }
    }

    #[test]
    fn hard_nat_udp_json_decode_rejects_invalid_json_or_missing_required_fields() {
        assert!(decode_hard_nat_udp_packet("{not-json").is_err());
        assert!(decode_hard_nat_udp_packet(r#"{"v":"hn1"}"#).is_err());
        assert!(decode_hard_nat_udp_packet(
            r#"{
                "v":"hn1",
                "packet_type":"handshake_req",
                "session_id":9,
                "sender":{"role":"nat4","socket_id":1,"generation":1,"seq":1}
            }"#
        )
        .is_err());
    }

    #[test]
    fn hard_nat_udp_json_classification_reports_packet_type() {
        let packets = [
            (
                HardNatUdpPacketType::ProbeReq,
                HardNatUdpPacket {
                    v: HARD_NAT_UDP_JSON_VERSION.to_string(),
                    packet_type: HardNatUdpPacketType::ProbeReq,
                    session_id: 1,
                    sender: HardNatUdpSender {
                        role: HardNatRole::Nat3,
                        socket_id: 0,
                        generation: 0,
                        seq: 1,
                    },
                    handshake_stage: None,
                    ack_of: None,
                    tuple: None,
                },
            ),
            (
                HardNatUdpPacketType::ProbeAck,
                HardNatUdpPacket {
                    v: HARD_NAT_UDP_JSON_VERSION.to_string(),
                    packet_type: HardNatUdpPacketType::ProbeAck,
                    session_id: 1,
                    sender: HardNatUdpSender {
                        role: HardNatRole::Nat4,
                        socket_id: 9,
                        generation: 0,
                        seq: 2,
                    },
                    handshake_stage: None,
                    ack_of: None,
                    tuple: None,
                },
            ),
            (
                HardNatUdpPacketType::HandshakeReq,
                HardNatUdpPacket {
                    v: HARD_NAT_UDP_JSON_VERSION.to_string(),
                    packet_type: HardNatUdpPacketType::HandshakeReq,
                    session_id: 1,
                    sender: HardNatUdpSender {
                        role: HardNatRole::Nat4,
                        socket_id: 9,
                        generation: 3,
                        seq: 3,
                    },
                    handshake_stage: Some(HardNatHandshakeStage::PreCandidate),
                    ack_of: None,
                    tuple: None,
                },
            ),
            (
                HardNatUdpPacketType::HandshakeAck,
                HardNatUdpPacket {
                    v: HARD_NAT_UDP_JSON_VERSION.to_string(),
                    packet_type: HardNatUdpPacketType::HandshakeAck,
                    session_id: 1,
                    sender: HardNatUdpSender {
                        role: HardNatRole::Nat3,
                        socket_id: 0,
                        generation: 0,
                        seq: 4,
                    },
                    handshake_stage: Some(HardNatHandshakeStage::Candidate),
                    ack_of: Some(HardNatUdpSender {
                        role: HardNatRole::Nat4,
                        socket_id: 9,
                        generation: 3,
                        seq: 3,
                    }),
                    tuple: Some(HardNatTuple {
                        nat3_addr: "10.0.0.1:1111".parse().unwrap(),
                        nat4_addr: "203.0.113.7:2222".parse().unwrap(),
                    }),
                },
            ),
        ];

        for (packet_type, packet) in packets {
            let encoded = encode_hard_nat_udp_packet(&packet).unwrap();
            assert_eq!(
                classify_probe_packet(encoded.as_bytes(), DEFAULT_PROBE_TEXT),
                ProbePacketKind::Json(packet.clone())
            );
            assert_eq!(packet.packet_type, packet_type);
        }
    }

    #[test]
    fn hard_nat_udp_json_handshake_ack_requires_ack_of_and_tuple() {
        let missing_ack_of = r#"{
            "v":"hn1",
            "packet_type":"handshake_ack",
            "session_id":7,
            "sender":{"role":"nat3","socket_id":0,"generation":0,"seq":2},
            "handshake_stage":"candidate",
            "tuple":{"nat3_addr":"1.1.1.1:1000","nat4_addr":"2.2.2.2:2000"}
        }"#;
        assert!(decode_hard_nat_udp_packet(missing_ack_of).is_err());

        let missing_tuple = r#"{
            "v":"hn1",
            "packet_type":"handshake_ack",
            "session_id":7,
            "sender":{"role":"nat3","socket_id":0,"generation":0,"seq":2},
            "handshake_stage":"candidate",
            "ack_of":{"role":"nat4","socket_id":8,"generation":1,"seq":9}
        }"#;
        assert!(decode_hard_nat_udp_packet(missing_tuple).is_err());
    }

    #[test]
    fn hard_nat_udp_json_rejects_invalid_tuple_socket_addr() {
        let invalid_tuple = r#"{
            "v":"hn1",
            "packet_type":"handshake_ack",
            "session_id":7,
            "sender":{"role":"nat3","socket_id":0,"generation":0,"seq":2},
            "handshake_stage":"candidate",
            "ack_of":{"role":"nat4","socket_id":8,"generation":1,"seq":9},
            "tuple":{"nat3_addr":"not-a-socket-addr","nat4_addr":"2.2.2.2:2000"}
        }"#;
        assert!(decode_hard_nat_udp_packet(invalid_tuple).is_err());
    }

    #[test]
    fn nat4_manual_json_probe_req_triggers_probe_ack_action() {
        let packet = HardNatUdpPacket {
            v: HARD_NAT_UDP_JSON_VERSION.to_string(),
            packet_type: HardNatUdpPacketType::ProbeReq,
            session_id: 42,
            sender: HardNatUdpSender {
                role: HardNatRole::Nat3,
                socket_id: 0,
                generation: 0,
                seq: 7,
            },
            handshake_stage: None,
            ack_of: None,
            tuple: None,
        };
        let encoded = encode_hard_nat_udp_packet(&packet).unwrap();
        let kind = classify_probe_packet(encoded.as_bytes(), DEFAULT_PROBE_TEXT);
        assert_eq!(
            decide_recv_probe_action(
                HardNatRole::Nat4,
                false,
                &JsonProbeMode::Nat4ProbeResponder {
                    socket_id: 7,
                    next_seq: Arc::new(AtomicU64::new(0)),
                },
                &kind,
            ),
            RecvProbeAction::ReplyJsonProbeAck { session_id: 42 }
        );
    }

    #[test]
    fn build_hard_nat_handshake_req_packet_uses_token_stage_and_sender() {
        let pre_candidate = build_hard_nat_handshake_req_packet_from_token(
            77,
            ProbeToken {
                role: HardNatRole::Nat4,
                socket_id: 5,
                generation: 0,
                seq: 9,
            },
        );
        assert_eq!(
            pre_candidate.packet_type,
            HardNatUdpPacketType::HandshakeReq
        );
        assert_eq!(pre_candidate.session_id, 77);
        assert_eq!(
            pre_candidate.sender,
            HardNatUdpSender {
                role: HardNatRole::Nat4,
                socket_id: 5,
                generation: 0,
                seq: 9,
            }
        );
        assert_eq!(
            pre_candidate.handshake_stage,
            Some(HardNatHandshakeStage::PreCandidate)
        );
        assert!(pre_candidate.ack_of.is_none());
        assert!(pre_candidate.tuple.is_none());

        let candidate = build_hard_nat_handshake_req_packet_from_token(
            77,
            ProbeToken {
                role: HardNatRole::Nat4,
                socket_id: 5,
                generation: 3,
                seq: 10,
            },
        );
        assert_eq!(
            candidate.handshake_stage,
            Some(HardNatHandshakeStage::Candidate)
        );
    }

    #[test]
    fn nat4_json_handshake_ack_is_accepted() {
        let packet = HardNatUdpPacket {
            v: HARD_NAT_UDP_JSON_VERSION.to_string(),
            packet_type: HardNatUdpPacketType::HandshakeAck,
            session_id: 88,
            sender: HardNatUdpSender {
                role: HardNatRole::Nat3,
                socket_id: 0,
                generation: 0,
                seq: 4,
            },
            handshake_stage: Some(HardNatHandshakeStage::Candidate),
            ack_of: Some(HardNatUdpSender {
                role: HardNatRole::Nat4,
                socket_id: 11,
                generation: 2,
                seq: 7,
            }),
            tuple: Some(HardNatTuple {
                nat3_addr: "127.0.0.1:50000".parse().unwrap(),
                nat4_addr: "127.0.0.1:50001".parse().unwrap(),
            }),
        };
        let encoded = encode_hard_nat_udp_packet(&packet).unwrap();
        let kind = classify_probe_packet(encoded.as_bytes(), DEFAULT_PROBE_TEXT);
        assert_eq!(
            decide_recv_probe_action(
                HardNatRole::Nat4,
                true,
                &JsonProbeMode::Nat4HandshakeResponder {
                    socket_id: 11,
                    next_seq: Arc::new(AtomicU64::new(0)),
                },
                &kind
            ),
            RecvProbeAction::AcceptJsonHandshakeAck(packet)
        );
    }

    #[test]
    fn nat3_probe_only_json_mode_ignores_handshake_req() {
        let packet = HardNatUdpPacket {
            v: HARD_NAT_UDP_JSON_VERSION.to_string(),
            packet_type: HardNatUdpPacketType::HandshakeReq,
            session_id: 88,
            sender: HardNatUdpSender {
                role: HardNatRole::Nat4,
                socket_id: 11,
                generation: 0,
                seq: 7,
            },
            handshake_stage: Some(HardNatHandshakeStage::PreCandidate),
            ack_of: None,
            tuple: None,
        };
        let encoded = encode_hard_nat_udp_packet(&packet).unwrap();
        let kind = classify_probe_packet(encoded.as_bytes(), DEFAULT_PROBE_TEXT);
        assert_eq!(
            decide_recv_probe_action(
                HardNatRole::Nat3,
                true,
                &JsonProbeMode::Nat3ProbeOnly,
                &kind
            ),
            RecvProbeAction::Ignore
        );
    }

    #[test]
    fn nat4_probe_only_json_mode_ignores_handshake_ack() {
        let packet = HardNatUdpPacket {
            v: HARD_NAT_UDP_JSON_VERSION.to_string(),
            packet_type: HardNatUdpPacketType::HandshakeAck,
            session_id: 88,
            sender: HardNatUdpSender {
                role: HardNatRole::Nat3,
                socket_id: 0,
                generation: 0,
                seq: 4,
            },
            handshake_stage: Some(HardNatHandshakeStage::Candidate),
            ack_of: Some(HardNatUdpSender {
                role: HardNatRole::Nat4,
                socket_id: 11,
                generation: 2,
                seq: 7,
            }),
            tuple: Some(HardNatTuple {
                nat3_addr: "127.0.0.1:50000".parse().unwrap(),
                nat4_addr: "127.0.0.1:50001".parse().unwrap(),
            }),
        };
        let encoded = encode_hard_nat_udp_packet(&packet).unwrap();
        let kind = classify_probe_packet(encoded.as_bytes(), DEFAULT_PROBE_TEXT);
        assert_eq!(
            decide_recv_probe_action(
                HardNatRole::Nat4,
                true,
                &JsonProbeMode::Nat4ProbeResponder {
                    socket_id: 11,
                    next_seq: Arc::new(AtomicU64::new(0)),
                },
                &kind
            ),
            RecvProbeAction::Ignore
        );
    }

    #[test]
    fn manual_json_handshake_ack_validation_rejects_wrong_session_stage_or_tuple() {
        let mut state =
            ManualConvergeCoordinator::new(2, Duration::from_millis(1), Duration::from_millis(0));
        let now = Instant::now();
        assert_eq!(state.mark_warm_done(1, now), None);
        assert_eq!(state.mark_warm_done(2, now), Some(now));
        assert!(state.finish_warming(now));
        state.record_json_probe_session_id(1, 88);

        let local: SocketAddr = "127.0.0.1:50001".parse().unwrap();
        let from: SocketAddr = "127.0.0.1:50000".parse().unwrap();
        let good = HardNatUdpPacket {
            v: HARD_NAT_UDP_JSON_VERSION.to_string(),
            packet_type: HardNatUdpPacketType::HandshakeAck,
            session_id: 88,
            sender: HardNatUdpSender {
                role: HardNatRole::Nat3,
                socket_id: 0,
                generation: 0,
                seq: 4,
            },
            handshake_stage: Some(HardNatHandshakeStage::Candidate),
            ack_of: Some(HardNatUdpSender {
                role: HardNatRole::Nat4,
                socket_id: 1,
                generation: 3,
                seq: 7,
            }),
            tuple: Some(HardNatTuple {
                nat3_addr: from,
                nat4_addr: local,
            }),
        };
        assert!(validate_manual_json_handshake_ack_packet(&state, 1, &good, local, from).is_some());

        let wrong_session = HardNatUdpPacket {
            session_id: 89,
            ..good.clone()
        };
        assert!(
            validate_manual_json_handshake_ack_packet(&state, 1, &wrong_session, local, from)
                .is_none()
        );

        let wrong_stage = HardNatUdpPacket {
            handshake_stage: Some(HardNatHandshakeStage::PreCandidate),
            ..good.clone()
        };
        assert!(
            validate_manual_json_handshake_ack_packet(&state, 1, &wrong_stage, local, from)
                .is_none()
        );

        let wrong_tuple = HardNatUdpPacket {
            tuple: Some(HardNatTuple {
                nat3_addr: from,
                nat4_addr: "127.0.0.1:59999".parse().unwrap(),
            }),
            ..good
        };
        assert!(
            validate_manual_json_handshake_ack_packet(&state, 1, &wrong_tuple, local, from)
                .is_none()
        );
    }

    #[test]
    fn connected_log_is_suppressed_when_probe_is_not_recorded() {
        let from: SocketAddr = "127.0.0.1:34567".parse().unwrap();
        let logs = capture_logs(|| maybe_log_connected_from_target(false, true, from));
        assert!(!logs.contains("connected from target"));

        let logs = capture_logs(|| maybe_log_connected_from_target(true, true, from));
        assert!(logs.contains("connected from target"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn send_nat3_batch_once_json_probe_req_payload() -> Result<()> {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let receiver = UdpSocket::bind("127.0.0.1:0").await?;
        let shared = Arc::new(Shared {
            connecteds: Default::default(),
            connecteds_by_socket_id: Default::default(),
            nat3_candidate_observed_remotes: Default::default(),
            nat3_current_public_addr: Default::default(),
        });
        let send_mode = Nat3BatchSendMode::JsonProbe {
            session_id: 55,
            next_seq: AtomicU64::new(0),
        };

        let target = receiver.local_addr()?;
        let has_connected = send_nat3_batch_once(
            &socket,
            DEFAULT_PROBE_TEXT,
            &[target],
            &shared,
            1,
            true,
            &send_mode,
        )
        .await?;
        assert!(!has_connected);

        let mut buf = [0_u8; 1024];
        let (len, _) =
            tokio::time::timeout(Duration::from_millis(300), receiver.recv_from(&mut buf))
                .await
                .with_context(|| "timed out waiting for nat3 json probe_req")??;
        let packet = decode_hard_nat_udp_packet(std::str::from_utf8(&buf[..len])?)?;
        assert_eq!(packet.packet_type, HardNatUdpPacketType::ProbeReq);
        assert_eq!(packet.session_id, 55);
        assert_eq!(packet.sender.role, HardNatRole::Nat3);
        assert_eq!(packet.sender.socket_id, 0);
        assert_eq!(packet.sender.generation, 0);
        assert_eq!(packet.sender.seq, 0);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn recv_loop_nat4_json_probe_req_replies_probe_ack() -> Result<()> {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let local_addr = socket.local_addr()?;
        let peer = UdpSocket::bind("127.0.0.1:0").await?;
        let shared = Arc::new(Shared {
            connecteds: Default::default(),
            connecteds_by_socket_id: Default::default(),
            nat3_candidate_observed_remotes: Default::default(),
            nat3_current_public_addr: Default::default(),
        });

        let recv_task = {
            let socket = socket.clone();
            let shared = shared.clone();
            let opts = RecvLoopOptions {
                role: HardNatRole::Nat4,
                expected_text: Arc::new(DEFAULT_PROBE_TEXT.to_string()),
                promote_hit_ttl: None,
                token_handshake_enabled: false,
                json_probe_mode: JsonProbeMode::Nat4ProbeResponder {
                    socket_id: 9,
                    next_seq: Arc::new(AtomicU64::new(0)),
                },
                echo_content_ack: false,
                debug_converge_lease: false,
                manual_converge_socket: None,
            };
            tokio::spawn(async move {
                let _ = recv_loop(socket, &shared, opts).await;
            })
        };

        let probe_req = HardNatUdpPacket {
            v: HARD_NAT_UDP_JSON_VERSION.to_string(),
            packet_type: HardNatUdpPacketType::ProbeReq,
            session_id: 88,
            sender: HardNatUdpSender {
                role: HardNatRole::Nat3,
                socket_id: 0,
                generation: 0,
                seq: 3,
            },
            handshake_stage: None,
            ack_of: None,
            tuple: None,
        };
        let encoded = encode_hard_nat_udp_packet(&probe_req)?;
        peer.send_to(encoded.as_bytes(), local_addr).await?;

        let mut buf = [0_u8; 1024];
        let (len, from) =
            tokio::time::timeout(Duration::from_millis(300), peer.recv_from(&mut buf))
                .await
                .with_context(|| "timed out waiting for nat4 probe_ack")??;
        assert_eq!(from, local_addr);
        let ack = decode_hard_nat_udp_packet(std::str::from_utf8(&buf[..len])?)?;
        assert_eq!(ack.packet_type, HardNatUdpPacketType::ProbeAck);
        assert_eq!(ack.session_id, 88);
        assert_eq!(ack.sender.role, HardNatRole::Nat4);
        assert_eq!(ack.sender.socket_id, 9);
        assert_eq!(ack.sender.generation, 0);
        assert_eq!(ack.sender.seq, 0);

        recv_task.abort();
        let _ = recv_task.await;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn recv_loop_nat3_json_probe_ack_does_not_mark_connected() -> Result<()> {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let local_addr = socket.local_addr()?;
        let peer = UdpSocket::bind("127.0.0.1:0").await?;
        let shared = Arc::new(Shared {
            connecteds: Default::default(),
            connecteds_by_socket_id: Default::default(),
            nat3_candidate_observed_remotes: Default::default(),
            nat3_current_public_addr: Default::default(),
        });

        let recv_task = {
            let socket = socket.clone();
            let shared = shared.clone();
            let opts = RecvLoopOptions {
                role: HardNatRole::Nat3,
                expected_text: Arc::new(DEFAULT_PROBE_TEXT.to_string()),
                promote_hit_ttl: None,
                token_handshake_enabled: true,
                json_probe_mode: JsonProbeMode::Nat3HandshakeResponder {
                    next_seq: Arc::new(AtomicU64::new(0)),
                },
                echo_content_ack: false,
                debug_converge_lease: true,
                manual_converge_socket: None,
            };
            tokio::spawn(async move {
                let _ = recv_loop(socket, &shared, opts).await;
            })
        };

        let probe_ack = HardNatUdpPacket {
            v: HARD_NAT_UDP_JSON_VERSION.to_string(),
            packet_type: HardNatUdpPacketType::ProbeAck,
            session_id: 66,
            sender: HardNatUdpSender {
                role: HardNatRole::Nat4,
                socket_id: 9,
                generation: 0,
                seq: 1,
            },
            handshake_stage: None,
            ack_of: None,
            tuple: None,
        };
        let encoded = encode_hard_nat_udp_packet(&probe_ack)?;
        peer.send_to(encoded.as_bytes(), local_addr).await?;

        let mut buf = [0_u8; 1024];
        assert!(
            tokio::time::timeout(Duration::from_millis(100), peer.recv_from(&mut buf))
                .await
                .is_err()
        );

        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!shared.has_connected());

        recv_task.abort();
        let _ = recv_task.await;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn recv_loop_nat3_json_handshake_req_replies_handshake_ack_without_marking_connected(
    ) -> Result<()> {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let local_addr = socket.local_addr()?;
        let peer = UdpSocket::bind("127.0.0.1:0").await?;
        let peer_addr = peer.local_addr()?;
        let shared = Arc::new(Shared {
            connecteds: Default::default(),
            connecteds_by_socket_id: Default::default(),
            nat3_candidate_observed_remotes: Default::default(),
            nat3_current_public_addr: Default::default(),
        });

        let recv_task = {
            let socket = socket.clone();
            let shared = shared.clone();
            let opts = RecvLoopOptions {
                role: HardNatRole::Nat3,
                expected_text: Arc::new(DEFAULT_PROBE_TEXT.to_string()),
                promote_hit_ttl: None,
                token_handshake_enabled: true,
                json_probe_mode: JsonProbeMode::Nat3HandshakeResponder {
                    next_seq: Arc::new(AtomicU64::new(0)),
                },
                echo_content_ack: false,
                debug_converge_lease: true,
                manual_converge_socket: None,
            };
            tokio::spawn(async move {
                let _ = recv_loop(socket, &shared, opts).await;
            })
        };

        let handshake_req = HardNatUdpPacket {
            v: HARD_NAT_UDP_JSON_VERSION.to_string(),
            packet_type: HardNatUdpPacketType::HandshakeReq,
            session_id: 99,
            sender: HardNatUdpSender {
                role: HardNatRole::Nat4,
                socket_id: 11,
                generation: 2,
                seq: 7,
            },
            handshake_stage: Some(HardNatHandshakeStage::Candidate),
            ack_of: None,
            tuple: None,
        };
        let encoded = encode_hard_nat_udp_packet(&handshake_req)?;
        peer.send_to(encoded.as_bytes(), local_addr).await?;

        let mut buf = [0_u8; 1024];
        let (len, from) =
            tokio::time::timeout(Duration::from_millis(300), peer.recv_from(&mut buf))
                .await
                .with_context(|| "timed out waiting for nat3 handshake_ack")??;
        assert_eq!(from, local_addr);
        let ack = decode_hard_nat_udp_packet(std::str::from_utf8(&buf[..len])?)?;
        assert_eq!(ack.packet_type, HardNatUdpPacketType::HandshakeAck);
        assert_eq!(ack.session_id, 99);
        assert_eq!(ack.sender.role, HardNatRole::Nat3);
        assert_eq!(ack.sender.socket_id, 0);
        assert_eq!(ack.sender.generation, 0);
        assert_eq!(ack.sender.seq, 0);
        assert_eq!(ack.handshake_stage, Some(HardNatHandshakeStage::Candidate));
        assert_eq!(ack.ack_of, Some(handshake_req.sender));
        assert_eq!(
            ack.tuple,
            Some(HardNatTuple {
                nat3_addr: local_addr,
                nat4_addr: peer_addr,
            })
        );
        assert!(!shared.has_connected());

        recv_task.abort();
        let _ = recv_task.await;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn recv_loop_nat3_manual_non_debug_does_not_reply_json_handshake_req() -> Result<()> {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let local_addr = socket.local_addr()?;
        let peer = UdpSocket::bind("127.0.0.1:0").await?;
        let shared = Arc::new(Shared {
            connecteds: Default::default(),
            connecteds_by_socket_id: Default::default(),
            nat3_candidate_observed_remotes: Default::default(),
            nat3_current_public_addr: Default::default(),
        });

        let recv_task = {
            let socket = socket.clone();
            let shared = shared.clone();
            let opts = RecvLoopOptions {
                role: HardNatRole::Nat3,
                expected_text: Arc::new(DEFAULT_PROBE_TEXT.to_string()),
                promote_hit_ttl: None,
                token_handshake_enabled: false,
                json_probe_mode: JsonProbeMode::Nat3ProbeOnly,
                echo_content_ack: false,
                debug_converge_lease: false,
                manual_converge_socket: None,
            };
            tokio::spawn(async move {
                let _ = recv_loop(socket, &shared, opts).await;
            })
        };

        let handshake_req = HardNatUdpPacket {
            v: HARD_NAT_UDP_JSON_VERSION.to_string(),
            packet_type: HardNatUdpPacketType::HandshakeReq,
            session_id: 199,
            sender: HardNatUdpSender {
                role: HardNatRole::Nat4,
                socket_id: 12,
                generation: 1,
                seq: 8,
            },
            handshake_stage: Some(HardNatHandshakeStage::Candidate),
            ack_of: None,
            tuple: None,
        };
        let encoded = encode_hard_nat_udp_packet(&handshake_req)?;
        peer.send_to(encoded.as_bytes(), local_addr).await?;

        let mut buf = [0_u8; 512];
        assert!(
            tokio::time::timeout(Duration::from_millis(120), peer.recv_from(&mut buf))
                .await
                .is_err()
        );
        assert!(!shared.has_connected());

        recv_task.abort();
        let _ = recv_task.await;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn recv_loop_nat4_json_handshake_ack_ignores_wrong_session_stage_or_tuple_without_marking_connected_after_first_valid_ack() -> Result<()>
    {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let local_addr = socket.local_addr()?;
        let peer = UdpSocket::bind("127.0.0.1:0").await?;
        let peer_addr = peer.local_addr()?;
        let shared = Arc::new(Shared {
            connecteds: Default::default(),
            connecteds_by_socket_id: Default::default(),
            nat3_candidate_observed_remotes: Default::default(),
            nat3_current_public_addr: Default::default(),
        });

        let mut state =
            ManualConvergeCoordinator::new(1, Duration::from_millis(20), Duration::ZERO);
        state.phase = ManualConvergePhase::Probing;
        state.current_generation = 1;
        state.lease_owner = Some(7);
        let validating_started_at = Instant::now();
        let socket_state = state.ensure_socket(7);
        socket_state.phase = Nat4SocketPhase::LeaseOwnerValidating { generation: 1 };
        socket_state.validating_started_at = Some(validating_started_at);
        socket_state.last_validation_sent_seq = Some(9);
        state.record_json_probe_session_id(7, 88);
        let coordinator = Arc::new(parking_lot::Mutex::new(state));

        let recv_task = {
            let socket = socket.clone();
            let shared = shared.clone();
            let coordinator = coordinator.clone();
            let opts = RecvLoopOptions {
                role: HardNatRole::Nat4,
                expected_text: Arc::new(DEFAULT_PROBE_TEXT.to_string()),
                promote_hit_ttl: None,
                token_handshake_enabled: true,
                json_probe_mode: JsonProbeMode::Nat4HandshakeResponder {
                    socket_id: 7,
                    next_seq: Arc::new(AtomicU64::new(0)),
                },
                echo_content_ack: false,
                debug_converge_lease: true,
                manual_converge_socket: Some(ManualConvergeRecvState {
                    socket_id: 7,
                    coordinator,
                }),
            };
            tokio::spawn(async move {
                let _ = recv_loop(socket, &shared, opts).await;
            })
        };

        let send_ack = |session_id: u64, stage: HardNatHandshakeStage, nat4_addr: SocketAddr| {
            HardNatUdpPacket {
                v: HARD_NAT_UDP_JSON_VERSION.to_string(),
                packet_type: HardNatUdpPacketType::HandshakeAck,
                session_id,
                sender: HardNatUdpSender {
                    role: HardNatRole::Nat3,
                    socket_id: 0,
                    generation: 0,
                    seq: 1,
                },
                handshake_stage: Some(stage),
                ack_of: Some(HardNatUdpSender {
                    role: HardNatRole::Nat4,
                    socket_id: 7,
                    generation: 1,
                    seq: 6,
                }),
                tuple: Some(HardNatTuple {
                    nat3_addr: peer_addr,
                    nat4_addr,
                }),
            }
        };

        for packet in [
            send_ack(89, HardNatHandshakeStage::Candidate, local_addr),
            send_ack(88, HardNatHandshakeStage::PreCandidate, local_addr),
            send_ack(
                88,
                HardNatHandshakeStage::Candidate,
                "127.0.0.1:9".parse().unwrap(),
            ),
            send_ack(88, HardNatHandshakeStage::Candidate, local_addr),
        ] {
            let payload = encode_hard_nat_udp_packet(&packet)?;
            peer.send_to(payload.as_bytes(), local_addr).await?;
        }

        tokio::time::sleep(Duration::from_millis(80)).await;
        let guard = coordinator.lock();
        let socket = guard.sockets.get(&7).expect("missing socket");
        assert_eq!(socket.validation_echo_count, 1);
        assert_eq!(socket.last_validation_matched_seq, Some(6));
        assert_eq!(
            socket.phase,
            Nat4SocketPhase::LeaseOwnerValidating { generation: 1 }
        );
        assert_eq!(guard.connected_socket_id(), None);

        recv_task.abort();
        let _ = recv_task.await;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn recv_loop_nat4_json_handshake_ack_without_manual_state_does_not_mark_connected()
    -> Result<()> {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let local_addr = socket.local_addr()?;
        let peer = UdpSocket::bind("127.0.0.1:0").await?;
        let peer_addr = peer.local_addr()?;
        let shared = Arc::new(Shared {
            connecteds: Default::default(),
            connecteds_by_socket_id: Default::default(),
            nat3_candidate_observed_remotes: Default::default(),
            nat3_current_public_addr: Default::default(),
        });

        let recv_task = {
            let socket = socket.clone();
            let shared = shared.clone();
            let opts = RecvLoopOptions {
                role: HardNatRole::Nat4,
                expected_text: Arc::new(DEFAULT_PROBE_TEXT.to_string()),
                promote_hit_ttl: None,
                token_handshake_enabled: true,
                json_probe_mode: JsonProbeMode::Nat4HandshakeResponder {
                    socket_id: 7,
                    next_seq: Arc::new(AtomicU64::new(0)),
                },
                echo_content_ack: false,
                debug_converge_lease: true,
                manual_converge_socket: None,
            };
            tokio::spawn(async move {
                let _ = recv_loop(socket, &shared, opts).await;
            })
        };

        let handshake_ack = HardNatUdpPacket {
            v: HARD_NAT_UDP_JSON_VERSION.to_string(),
            packet_type: HardNatUdpPacketType::HandshakeAck,
            session_id: 88,
            sender: HardNatUdpSender {
                role: HardNatRole::Nat3,
                socket_id: 0,
                generation: 0,
                seq: 1,
            },
            handshake_stage: Some(HardNatHandshakeStage::Candidate),
            ack_of: Some(HardNatUdpSender {
                role: HardNatRole::Nat4,
                socket_id: 7,
                generation: 1,
                seq: 6,
            }),
            tuple: Some(HardNatTuple {
                nat3_addr: peer_addr,
                nat4_addr: local_addr,
            }),
        };
        let payload = encode_hard_nat_udp_packet(&handshake_ack)?;
        peer.send_to(payload.as_bytes(), local_addr).await?;

        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(!shared.has_connected());

        recv_task.abort();
        let _ = recv_task.await;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn recv_loop_nat4_json_handshake_ack_marks_connected_after_second_valid_ack()
    -> Result<()> {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let local_addr = socket.local_addr()?;
        let peer = UdpSocket::bind("127.0.0.1:0").await?;
        let peer_addr = peer.local_addr()?;
        let shared = Arc::new(Shared {
            connecteds: Default::default(),
            connecteds_by_socket_id: Default::default(),
            nat3_candidate_observed_remotes: Default::default(),
            nat3_current_public_addr: Default::default(),
        });

        let mut state =
            ManualConvergeCoordinator::new(1, Duration::from_millis(20), Duration::ZERO);
        state.phase = ManualConvergePhase::Probing;
        state.current_generation = 1;
        state.lease_owner = Some(7);
        let validating_started_at = Instant::now();
        let socket_state = state.ensure_socket(7);
        socket_state.phase = Nat4SocketPhase::LeaseOwnerValidating { generation: 1 };
        socket_state.validating_started_at = Some(validating_started_at);
        socket_state.last_validation_sent_seq = Some(9);
        state.record_json_probe_session_id(7, 88);
        let coordinator = Arc::new(parking_lot::Mutex::new(state));

        let recv_task = {
            let socket = socket.clone();
            let shared = shared.clone();
            let coordinator = coordinator.clone();
            let opts = RecvLoopOptions {
                role: HardNatRole::Nat4,
                expected_text: Arc::new(DEFAULT_PROBE_TEXT.to_string()),
                promote_hit_ttl: None,
                token_handshake_enabled: true,
                json_probe_mode: JsonProbeMode::Nat4HandshakeResponder {
                    socket_id: 7,
                    next_seq: Arc::new(AtomicU64::new(0)),
                },
                echo_content_ack: false,
                debug_converge_lease: true,
                manual_converge_socket: Some(ManualConvergeRecvState {
                    socket_id: 7,
                    coordinator,
                }),
            };
            tokio::spawn(async move {
                let _ = recv_loop(socket, &shared, opts).await;
            })
        };

        let send_ack = |seq: u64| HardNatUdpPacket {
            v: HARD_NAT_UDP_JSON_VERSION.to_string(),
            packet_type: HardNatUdpPacketType::HandshakeAck,
            session_id: 88,
            sender: HardNatUdpSender {
                role: HardNatRole::Nat3,
                socket_id: 0,
                generation: 0,
                seq,
            },
            handshake_stage: Some(HardNatHandshakeStage::Candidate),
            ack_of: Some(HardNatUdpSender {
                role: HardNatRole::Nat4,
                socket_id: 7,
                generation: 1,
                seq,
            }),
            tuple: Some(HardNatTuple {
                nat3_addr: peer_addr,
                nat4_addr: local_addr,
            }),
        };

        for packet in [send_ack(6), send_ack(7)] {
            let payload = encode_hard_nat_udp_packet(&packet)?;
            peer.send_to(payload.as_bytes(), local_addr).await?;
        }

        tokio::time::sleep(Duration::from_millis(80)).await;
        let guard = coordinator.lock();
        let socket = guard.sockets.get(&7).expect("missing socket");
        assert_eq!(socket.validation_echo_count, 2);
        assert_eq!(socket.last_validation_matched_seq, Some(7));
        assert_eq!(socket.phase, Nat4SocketPhase::Connected { generation: 1 });
        assert_eq!(guard.connected_socket_id(), Some(7));

        recv_task.abort();
        let _ = recv_task.await;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn nat4_manual_json_handshake_req_roundtrip_reaches_connected_on_second_ack()
    -> Result<()> {
        let nat4_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let nat4_local_addr = nat4_socket.local_addr()?;
        let nat3_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let nat3_local_addr = nat3_socket.local_addr()?;

        let nat4_shared = Arc::new(Shared {
            connecteds: Default::default(),
            connecteds_by_socket_id: Default::default(),
            nat3_candidate_observed_remotes: Default::default(),
            nat3_current_public_addr: Default::default(),
        });
        let nat3_shared = Arc::new(Shared {
            connecteds: Default::default(),
            connecteds_by_socket_id: Default::default(),
            nat3_candidate_observed_remotes: Default::default(),
            nat3_current_public_addr: Default::default(),
        });

        let mut state =
            ManualConvergeCoordinator::new(1, Duration::from_millis(20), Duration::ZERO);
        state.phase = ManualConvergePhase::Probing;
        state.current_generation = 1;
        state.lease_owner = Some(7);
        let socket_state = state.ensure_socket(7);
        socket_state.phase = Nat4SocketPhase::LeaseOwnerValidating { generation: 1 };
        socket_state.validating_started_at = Some(Instant::now());
        state.record_json_probe_session_id(7, 188);
        let coordinator = Arc::new(parking_lot::Mutex::new(state));

        let nat3_recv_task = {
            let socket = nat3_socket.clone();
            let shared = nat3_shared.clone();
            let opts = RecvLoopOptions {
                role: HardNatRole::Nat3,
                expected_text: Arc::new(DEFAULT_PROBE_TEXT.to_string()),
                promote_hit_ttl: None,
                token_handshake_enabled: true,
                json_probe_mode: JsonProbeMode::Nat3HandshakeResponder {
                    next_seq: Arc::new(AtomicU64::new(0)),
                },
                echo_content_ack: false,
                debug_converge_lease: true,
                manual_converge_socket: None,
            };
            tokio::spawn(async move {
                let _ = recv_loop(socket, &shared, opts).await;
            })
        };

        let nat4_recv_task = {
            let socket = nat4_socket.clone();
            let shared = nat4_shared.clone();
            let coordinator = coordinator.clone();
            let opts = RecvLoopOptions {
                role: HardNatRole::Nat4,
                expected_text: Arc::new(DEFAULT_PROBE_TEXT.to_string()),
                promote_hit_ttl: None,
                token_handshake_enabled: true,
                json_probe_mode: JsonProbeMode::Nat4HandshakeResponder {
                    socket_id: 7,
                    next_seq: Arc::new(AtomicU64::new(0)),
                },
                echo_content_ack: false,
                debug_converge_lease: true,
                manual_converge_socket: Some(ManualConvergeRecvState {
                    socket_id: 7,
                    coordinator,
                }),
            };
            tokio::spawn(async move {
                let _ = recv_loop(socket, &shared, opts).await;
            })
        };

        let sender = UdpSender {
            socket_id: 7,
            socket: nat4_socket.clone(),
            target: nat3_local_addr,
            payload: ProbePayload::Plain(Arc::new(DEFAULT_PROBE_TEXT.to_string())),
            local: nat4_local_addr,
        };

        let first_token = coordinator
            .lock()
            .next_send_token(7)
            .expect("first validation token");
        sender
            .send_json_handshake_req_from_token(188, first_token)
            .await?;

        tokio::time::timeout(Duration::from_millis(300), async {
            loop {
                let guard = coordinator.lock();
                let socket = guard.sockets.get(&7).expect("missing socket");
                if socket.validation_echo_count == 1 {
                    assert_eq!(
                        socket.phase,
                        Nat4SocketPhase::LeaseOwnerValidating { generation: 1 }
                    );
                    assert_eq!(guard.connected_socket_id(), None);
                    break;
                }
                drop(guard);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .with_context(|| "timed out waiting for first nat4 json handshake ack")?;

        let second_token = coordinator
            .lock()
            .next_send_token(7)
            .expect("second validation token");
        sender
            .send_json_handshake_req_from_token(188, second_token)
            .await?;

        tokio::time::timeout(Duration::from_millis(300), async {
            loop {
                let guard = coordinator.lock();
                let socket = guard.sockets.get(&7).expect("missing socket");
                if socket.validation_echo_count == 2 {
                    assert_eq!(
                        socket.phase,
                        Nat4SocketPhase::Connected { generation: 1 }
                    );
                    assert_eq!(guard.connected_socket_id(), Some(7));
                    break;
                }
                drop(guard);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .with_context(|| "timed out waiting for second nat4 json handshake ack")?;

        nat4_recv_task.abort();
        let _ = nat4_recv_task.await;
        nat3_recv_task.abort();
        let _ = nat3_recv_task.await;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn nat4_manual_json_probe_and_handshake_roundtrip_reaches_connected_after_n1_and_n2()
    -> Result<()> {
        let nat4_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let nat4_local_addr = nat4_socket.local_addr()?;
        let nat3_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let nat3_local_addr = nat3_socket.local_addr()?;

        let nat4_shared = Arc::new(Shared {
            connecteds: Default::default(),
            connecteds_by_socket_id: Default::default(),
            nat3_candidate_observed_remotes: Default::default(),
            nat3_current_public_addr: Default::default(),
        });
        let nat3_shared = Arc::new(Shared {
            connecteds: Default::default(),
            connecteds_by_socket_id: Default::default(),
            nat3_candidate_observed_remotes: Default::default(),
            nat3_current_public_addr: Default::default(),
        });

        let mut state =
            ManualConvergeCoordinator::new(1, Duration::from_millis(20), Duration::ZERO);
        state.phase = ManualConvergePhase::Probing;
        state.ensure_socket(7).phase = Nat4SocketPhase::Probing;
        let coordinator = Arc::new(parking_lot::Mutex::new(state));

        let nat3_recv_task = {
            let socket = nat3_socket.clone();
            let shared = nat3_shared.clone();
            let opts = RecvLoopOptions {
                role: HardNatRole::Nat3,
                expected_text: Arc::new(DEFAULT_PROBE_TEXT.to_string()),
                promote_hit_ttl: None,
                token_handshake_enabled: true,
                json_probe_mode: JsonProbeMode::Nat3HandshakeResponder {
                    next_seq: Arc::new(AtomicU64::new(0)),
                },
                echo_content_ack: false,
                debug_converge_lease: true,
                manual_converge_socket: None,
            };
            tokio::spawn(async move {
                let _ = recv_loop(socket, &shared, opts).await;
            })
        };

        let nat4_recv_task = {
            let socket = nat4_socket.clone();
            let shared = nat4_shared.clone();
            let coordinator = coordinator.clone();
            let opts = RecvLoopOptions {
                role: HardNatRole::Nat4,
                expected_text: Arc::new(DEFAULT_PROBE_TEXT.to_string()),
                promote_hit_ttl: None,
                token_handshake_enabled: true,
                json_probe_mode: JsonProbeMode::Nat4HandshakeResponder {
                    socket_id: 7,
                    next_seq: Arc::new(AtomicU64::new(0)),
                },
                echo_content_ack: false,
                debug_converge_lease: true,
                manual_converge_socket: Some(ManualConvergeRecvState {
                    socket_id: 7,
                    coordinator,
                }),
            };
            tokio::spawn(async move {
                let _ = recv_loop(socket, &shared, opts).await;
            })
        };

        let send_mode = Nat3BatchSendMode::JsonProbe {
            session_id: 288,
            next_seq: AtomicU64::new(0),
        };
        send_nat3_batch_once(
            &nat3_socket,
            DEFAULT_PROBE_TEXT,
            &[nat4_local_addr],
            &nat3_shared,
            1,
            false,
            &send_mode,
        )
        .await?;
        send_nat3_batch_once(
            &nat3_socket,
            DEFAULT_PROBE_TEXT,
            &[nat4_local_addr],
            &nat3_shared,
            2,
            false,
            &send_mode,
        )
        .await?;

        tokio::time::timeout(Duration::from_millis(300), async {
            loop {
                let guard = coordinator.lock();
                if guard.socket_phase(7) == Some(Nat4SocketPhase::Candidate) {
                    assert_eq!(guard.socket_probe_hit_count(7), Some(2));
                    assert_eq!(guard.json_probe_session_id(7), Some(288));
                    assert_eq!(guard.connected_socket_id(), None);
                    break;
                }
                drop(guard);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .with_context(|| "timed out waiting for nat4 candidate after N1 probe hits")?;

        let sender = UdpSender {
            socket_id: 7,
            socket: nat4_socket.clone(),
            target: nat3_local_addr,
            payload: ProbePayload::Plain(Arc::new(DEFAULT_PROBE_TEXT.to_string())),
            local: nat4_local_addr,
        };

        let quiet_started_at = Instant::now();
        {
            let mut guard = coordinator.lock();
            let plans = plan_nat4_manual_converge_send_step(&mut guard, &[7], quiet_started_at);
            assert_eq!(guard.lease_owner(), Some(7));
            assert_eq!(plans[0].token, None);
            assert_eq!(
                guard.socket_phase(7),
                Some(Nat4SocketPhase::PendingQuiet { generation: 1 })
            );
        }
        let first_token = {
            let mut guard = coordinator.lock();
            let plans = plan_nat4_manual_converge_send_step(
                &mut guard,
                &[7],
                quiet_started_at + Duration::from_millis(20),
            );
            assert_eq!(
                guard.socket_phase(7),
                Some(Nat4SocketPhase::LeaseOwnerValidating { generation: 1 })
            );
            plans[0].token.expect("first validation token after N1")
        };
        sender
            .send_json_handshake_req_from_token(288, first_token)
            .await?;

        tokio::time::timeout(Duration::from_millis(300), async {
            loop {
                let guard = coordinator.lock();
                let socket = guard.sockets.get(&7).expect("missing socket");
                if socket.validation_echo_count == 1 {
                    assert_eq!(
                        socket.phase,
                        Nat4SocketPhase::LeaseOwnerValidating { generation: 1 }
                    );
                    assert_eq!(guard.connected_socket_id(), None);
                    break;
                }
                drop(guard);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .with_context(|| "timed out waiting for first nat4 json handshake ack after N1")?;

        let second_token = coordinator
            .lock()
            .next_send_token(7)
            .expect("second validation token after first N2 hit");
        sender
            .send_json_handshake_req_from_token(288, second_token)
            .await?;

        tokio::time::timeout(Duration::from_millis(300), async {
            loop {
                let guard = coordinator.lock();
                let socket = guard.sockets.get(&7).expect("missing socket");
                if socket.validation_echo_count == 2 {
                    assert_eq!(
                        socket.phase,
                        Nat4SocketPhase::Connected { generation: 1 }
                    );
                    assert_eq!(guard.connected_socket_id(), Some(7));
                    break;
                }
                drop(guard);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .with_context(|| "timed out waiting for second nat4 json handshake ack after N2")?;

        nat4_recv_task.abort();
        let _ = nat4_recv_task.await;
        nat3_recv_task.abort();
        let _ = nat3_recv_task.await;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn nat4_controlled_converge_characterization_replies_json_probe_ack_then_probe_token()
    -> Result<()> {
        let peer = UdpSocket::bind("127.0.0.1:0").await?;
        let peer_addr = peer.local_addr()?;
        let sink = UdpSocket::bind("127.0.0.1:0").await?;
        let sink_addr = sink.local_addr()?;
        let sink_task = tokio::spawn(async move {
            let mut buf = [0_u8; 512];
            loop {
                let _ = sink.recv_from(&mut buf).await?;
            }
            #[allow(unreachable_code)]
            Ok::<_, anyhow::Error>(())
        });

        let args = Nat4RunConfig {
            content: None,
            target: sink_addr,
            count: 1,
            ttl: Some(4),
            interval: Duration::from_millis(20),
            dump_public_addrs: false,
            debug_keep_recv: false,
            debug_promote_hit_ttl: None,
            debug_converge_lease: false,
        };
        let runtime =
            prepare_nat4_probe_runtime(&args, Nat4ProbeRuntimeOptions::controlled_converge(args.interval))
                .await?;
        let sender = runtime
            .senders
            .first()
            .with_context(|| "missing controlled runtime sender")?;
        let nat4_local_addr = sender.local;
        let nat4_reachable_addr =
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), nat4_local_addr.port());
        let socket_id = sender.socket_id;

        let coordinator = runtime
            .manual_converge
            .as_ref()
            .with_context(|| "controlled runtime missing manual converge coordinator")?;
        assert!(
            coordinator
                .lock()
                .finish_warming(Instant::now() + Duration::from_millis(30)),
            "controlled converge should enter probing after warm drain"
        );

        let probe_req = HardNatUdpPacket {
            v: HARD_NAT_UDP_JSON_VERSION.to_string(),
            packet_type: HardNatUdpPacketType::ProbeReq,
            session_id: 288,
            sender: HardNatUdpSender {
                role: HardNatRole::Nat3,
                socket_id: 0,
                generation: 0,
                seq: 3,
            },
            handshake_stage: None,
            ack_of: None,
            tuple: None,
        };
        let probe_req_payload = encode_hard_nat_udp_packet(&probe_req)?;
        peer.send_to(probe_req_payload.as_bytes(), nat4_reachable_addr)
            .await?;
        let mut buf = [0_u8; 1024];
        let (ack_len, ack_from) = tokio::time::timeout(Duration::from_millis(300), async {
            loop {
                let (len, from) = peer.recv_from(&mut buf).await?;
                if from == nat4_reachable_addr {
                    break Ok::<_, anyhow::Error>((len, from));
                }
            }
        })
        .await
        .with_context(|| "timed out waiting for nat4 controlled json probe_ack")??;
        assert_eq!(ack_from, nat4_reachable_addr);
        let ack_packet = decode_hard_nat_udp_packet(std::str::from_utf8(&buf[..ack_len])?)?;
        assert_eq!(ack_packet.packet_type, HardNatUdpPacketType::ProbeAck);
        assert_eq!(ack_packet.session_id, 288);
        assert_eq!(ack_packet.sender.role, HardNatRole::Nat4);
        assert_eq!(ack_packet.sender.socket_id, socket_id);
        assert_eq!(ack_packet.sender.generation, 0);

        tokio::time::timeout(Duration::from_millis(300), async {
            loop {
                if coordinator.lock().socket_probe_hit_count(socket_id) == Some(1) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .with_context(|| "timed out waiting for controlled probe hit to be recorded")?;

        let token = {
            let mut guard = coordinator.lock();
            let plans = plan_nat4_controlled_converge_send_step(
                &mut guard,
                &[socket_id],
                Instant::now() + Duration::from_millis(50),
            );
            plans
                .first()
                .and_then(|plan| plan.token)
                .with_context(|| "missing controlled probe token after probe hit")?
        };

        sender.send_token_to(peer_addr, token).await?;
        let (token_len, token_from) =
            tokio::time::timeout(Duration::from_millis(300), peer.recv_from(&mut buf))
                .await
                .with_context(|| "timed out waiting for controlled probe token send")??;
        assert_eq!(token_from, nat4_reachable_addr);
        let token_text = std::str::from_utf8(&buf[..token_len])?;
        assert_eq!(decode_probe_token(token_text), Some(token));
        assert!(
            decode_hard_nat_udp_packet(token_text).is_err(),
            "controlled path unexpectedly sent json handshake_req"
        );

        runtime.recv_tasks.abort_and_wait().await;
        sink_task.abort();
        let _ = sink_task.await;
        Ok(())
    }

    #[test]
    fn recv_loop_does_not_log_connected_when_probe_is_not_recorded() {
        let logs = capture_logs(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build tokio runtime");
            runtime.block_on(async {
                let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind nat3"));
                let local_addr = socket.local_addr().expect("nat3 local addr");
                let peer = UdpSocket::bind("127.0.0.1:0").await.expect("bind peer");
                let shared = Arc::new(Shared {
                    connecteds: Default::default(),
                    connecteds_by_socket_id: Default::default(),
                    nat3_candidate_observed_remotes: Default::default(),
                    nat3_current_public_addr: Default::default(),
                });

                let recv_task = {
                    let socket = socket.clone();
                    let shared = shared.clone();
                    let opts = RecvLoopOptions {
                        role: HardNatRole::Nat3,
                        expected_text: Arc::new(DEFAULT_PROBE_TEXT.to_string()),
                        promote_hit_ttl: None,
                        token_handshake_enabled: true,
                        json_probe_mode: JsonProbeMode::Nat3HandshakeResponder {
                            next_seq: Arc::new(AtomicU64::new(0)),
                        },
                        echo_content_ack: false,
                        debug_converge_lease: true,
                        manual_converge_socket: None,
                    };
                    tokio::spawn(async move {
                        let _ = recv_loop(socket, &shared, opts).await;
                    })
                };

                let probe_ack = HardNatUdpPacket {
                    v: HARD_NAT_UDP_JSON_VERSION.to_string(),
                    packet_type: HardNatUdpPacketType::ProbeAck,
                    session_id: 301,
                    sender: HardNatUdpSender {
                        role: HardNatRole::Nat4,
                        socket_id: 9,
                        generation: 0,
                        seq: 1,
                    },
                    handshake_stage: None,
                    ack_of: None,
                    tuple: None,
                };
                let payload = encode_hard_nat_udp_packet(&probe_ack).expect("encode probe_ack");
                peer.send_to(payload.as_bytes(), local_addr)
                    .await
                    .expect("send probe_ack");
                tokio::time::sleep(Duration::from_millis(60)).await;

                recv_task.abort();
                let _ = recv_task.await;
            });
        });
        assert!(
            !logs.contains("connected from target"),
            "unexpected connected log when packet should not be recorded: {logs}"
        );
    }

    #[test]
    fn warm_barrier_waits_for_all_sockets_before_probing() {
        let mut state = ManualConvergeCoordinator::new(
            2,
            Duration::from_millis(10),
            Duration::from_millis(100),
        );
        let now = Instant::now();

        assert!(state.mark_warm_done(0, now).is_none());
        assert_eq!(state.mark_warm_done(1, now), Some(now));
        assert!(!state.finish_warming(now + Duration::from_millis(99)));
        assert_eq!(state.phase(), ManualConvergePhase::Warming);
        assert!(state.finish_warming(now + Duration::from_millis(100)));
        assert_eq!(state.phase(), ManualConvergePhase::Probing);
    }

    #[test]
    fn warming_ignores_probe_hits_until_probing() {
        let mut state =
            ManualConvergeCoordinator::new(1, Duration::from_millis(10), Duration::from_millis(10));
        let now = Instant::now();

        assert!(!state.record_probe_hit(7, now));
        assert_eq!(state.socket_phase(7), Some(Nat4SocketPhase::Warming));

        assert_eq!(state.mark_warm_done(7, now), Some(now));
        assert!(!state.record_probe_hit(7, now + Duration::from_millis(5)));
        assert!(state.finish_warming(now + Duration::from_millis(10)));
        assert!(state.record_probe_hit(7, now + Duration::from_millis(11)));
        assert_eq!(state.socket_phase(7), Some(Nat4SocketPhase::Probing));
        assert_eq!(state.socket_probe_hit_count(7), Some(1));
    }

    #[test]
    fn probing_promotes_socket_to_candidate_after_n1_hits_within_t1() {
        let mut state =
            ManualConvergeCoordinator::new(1, Duration::from_millis(100), Duration::from_millis(0));
        let now = Instant::now();
        assert_eq!(state.mark_warm_done(3, now), Some(now));
        assert!(state.finish_warming(now));

        assert!(state.record_probe_hit(3, now + Duration::from_millis(10)));
        assert_eq!(state.socket_phase(3), Some(Nat4SocketPhase::Probing));
        assert_eq!(state.socket_probe_hit_count(3), Some(1));

        assert!(state.record_probe_hit(3, now + Duration::from_millis(20)));
        assert_eq!(state.socket_phase(3), Some(Nat4SocketPhase::Candidate));
        assert_eq!(state.socket_probe_hit_count(3), Some(2));
    }

    #[test]
    fn probing_resets_hit_window_after_t1_expires() {
        let mut state =
            ManualConvergeCoordinator::new(1, Duration::from_millis(100), Duration::from_millis(0));
        let now = Instant::now();
        assert_eq!(state.mark_warm_done(3, now), Some(now));
        assert!(state.finish_warming(now));

        assert!(state.record_probe_hit(3, now + Duration::from_millis(10)));
        assert!(state.record_probe_hit(3, now + Duration::from_millis(1600)));
        assert_eq!(state.socket_phase(3), Some(Nat4SocketPhase::Probing));
        assert_eq!(state.socket_probe_hit_count(3), Some(1));

        assert!(state.record_probe_hit(3, now + Duration::from_millis(1700)));
        assert_eq!(state.socket_phase(3), Some(Nat4SocketPhase::Candidate));
        assert_eq!(state.socket_probe_hit_count(3), Some(2));
    }

    #[test]
    fn probing_hit_count_is_tracked_independently_per_socket() {
        let mut state =
            ManualConvergeCoordinator::new(2, Duration::from_millis(100), Duration::from_millis(0));
        let now = Instant::now();
        assert_eq!(state.mark_warm_done(1, now), None);
        assert_eq!(state.mark_warm_done(2, now), Some(now));
        assert!(state.finish_warming(now));

        assert!(state.record_probe_hit(1, now + Duration::from_millis(10)));
        assert!(state.record_probe_hit(2, now + Duration::from_millis(20)));
        assert_eq!(state.socket_phase(1), Some(Nat4SocketPhase::Probing));
        assert_eq!(state.socket_phase(2), Some(Nat4SocketPhase::Probing));

        assert!(state.record_probe_hit(1, now + Duration::from_millis(30)));
        assert_eq!(state.socket_phase(1), Some(Nat4SocketPhase::Candidate));
        assert_eq!(state.socket_phase(2), Some(Nat4SocketPhase::Probing));
        assert_eq!(state.socket_probe_hit_count(1), Some(2));
        assert_eq!(state.socket_probe_hit_count(2), Some(1));
    }

    #[test]
    fn lease_grant_allows_only_one_owner_at_a_time() {
        let mut state =
            ManualConvergeCoordinator::new(2, Duration::from_millis(100), Duration::from_millis(0));
        let now = Instant::now();
        assert_eq!(state.mark_warm_done(1, now), None);
        assert_eq!(state.mark_warm_done(2, now), Some(now));
        assert!(state.finish_warming(now));
        assert!(state.record_probe_hit(1, now + Duration::from_millis(10)));
        assert!(state.record_probe_hit(1, now + Duration::from_millis(20)));
        assert!(state.record_probe_hit(2, now + Duration::from_millis(10)));
        assert!(state.record_probe_hit(2, now + Duration::from_millis(20)));

        let generation = state.try_acquire_lease(1, now + Duration::from_millis(30));
        assert_eq!(generation, Some(1));
        assert_eq!(state.lease_owner(), Some(1));
        assert_eq!(
            state.socket_phase(1),
            Some(Nat4SocketPhase::PendingQuiet { generation: 1 })
        );
        assert_eq!(
            state.socket_phase(2),
            Some(Nat4SocketPhase::Silent { generation: 1 })
        );
        assert_eq!(
            state.try_acquire_lease(2, now + Duration::from_millis(31)),
            None
        );
        assert_eq!(state.lease_owner(), Some(1));
    }

    #[test]
    fn quiet_barrier_ignores_old_generation_silent_ready() {
        let mut state =
            ManualConvergeCoordinator::new(2, Duration::from_millis(100), Duration::from_millis(0));
        let now = Instant::now();
        assert_eq!(state.mark_warm_done(1, now), None);
        assert_eq!(state.mark_warm_done(2, now), Some(now));
        assert!(state.finish_warming(now));
        assert!(state.record_probe_hit(1, now + Duration::from_millis(10)));
        assert!(state.record_probe_hit(1, now + Duration::from_millis(20)));

        let generation = state
            .try_acquire_lease(1, now + Duration::from_millis(30))
            .expect("lease granted");
        assert_eq!(generation, 1);
        assert!(!state.mark_silent_ready(2, generation + 1, now + Duration::from_millis(31)));
        assert_eq!(
            state.socket_phase(1),
            Some(Nat4SocketPhase::PendingQuiet { generation: 1 })
        );
        assert!(state.mark_silent_ready(2, generation, now + Duration::from_millis(32)));
    }

    #[test]
    fn quiet_barrier_waits_for_ready_or_timeout_before_validating() {
        let mut state =
            ManualConvergeCoordinator::new(2, Duration::from_millis(100), Duration::from_millis(0));
        let now = Instant::now();
        assert_eq!(state.mark_warm_done(1, now), None);
        assert_eq!(state.mark_warm_done(2, now), Some(now));
        assert!(state.finish_warming(now));
        assert!(state.record_probe_hit(1, now + Duration::from_millis(10)));
        assert!(state.record_probe_hit(1, now + Duration::from_millis(20)));

        let generation = state
            .try_acquire_lease(1, now + Duration::from_millis(30))
            .expect("lease granted");
        assert_eq!(
            state.advance_pending_quiet(now + Duration::from_millis(31)),
            None
        );
        assert!(state.mark_silent_ready(2, generation, now + Duration::from_millis(32)));
        assert_eq!(
            state.advance_pending_quiet(now + Duration::from_millis(40)),
            None
        );
        assert_eq!(
            state.advance_pending_quiet(now + Duration::from_millis(140)),
            Some(generation)
        );
        assert_eq!(
            state.socket_phase(1),
            Some(Nat4SocketPhase::LeaseOwnerValidating { generation })
        );
    }

    #[test]
    fn manual_converge_send_step_keeps_probing_sockets_sending_before_candidate() {
        let mut state =
            ManualConvergeCoordinator::new(2, Duration::from_millis(1), Duration::from_millis(0));
        let now = Instant::now();
        assert_eq!(state.mark_warm_done(1, now), None);
        assert_eq!(state.mark_warm_done(2, now), Some(now));
        assert!(state.finish_warming(now));

        let plans = plan_nat4_manual_converge_send_step(
            &mut state,
            &[1, 2],
            now + Duration::from_millis(1),
        );

        assert_eq!(
            plans,
            vec![
                Nat4SendPlan {
                    socket_id: 1,
                    should_send: true,
                    token: Some(ProbeToken {
                        role: HardNatRole::Nat4,
                        socket_id: 1,
                        generation: 0,
                        seq: 0,
                    }),
                },
                Nat4SendPlan {
                    socket_id: 2,
                    should_send: true,
                    token: Some(ProbeToken {
                        role: HardNatRole::Nat4,
                        socket_id: 2,
                        generation: 0,
                        seq: 0,
                    }),
                },
            ]
        );
        assert_eq!(state.lease_owner(), None);
        assert_eq!(state.socket_phase(1), Some(Nat4SocketPhase::Probing));
        assert_eq!(state.socket_phase(2), Some(Nat4SocketPhase::Probing));
    }

    #[test]
    fn manual_converge_send_step_grants_lease_and_silences_non_owner_sockets() {
        let mut state =
            ManualConvergeCoordinator::new(2, Duration::from_millis(1), Duration::from_millis(0));
        let now = Instant::now();
        assert_eq!(state.mark_warm_done(1, now), None);
        assert_eq!(state.mark_warm_done(2, now), Some(now));
        assert!(state.finish_warming(now));
        assert!(state.record_probe_hit(1, now + Duration::from_millis(1)));
        assert!(state.record_probe_hit(1, now + Duration::from_millis(2)));

        let plans = plan_nat4_manual_converge_send_step(
            &mut state,
            &[1, 2],
            now + Duration::from_millis(3),
        );

        assert_eq!(
            plans,
            vec![
                Nat4SendPlan {
                    socket_id: 1,
                    should_send: false,
                    token: None,
                },
                Nat4SendPlan {
                    socket_id: 2,
                    should_send: false,
                    token: None,
                },
            ]
        );
        assert_eq!(state.lease_owner(), Some(1));
        assert_eq!(
            state.socket_phase(1),
            Some(Nat4SocketPhase::PendingQuiet { generation: 1 })
        );
        assert_eq!(
            state.socket_phase(2),
            Some(Nat4SocketPhase::Silent { generation: 1 })
        );
    }

    #[test]
    fn manual_converge_send_step_only_sends_owner_after_quiet_barrier() {
        let mut state =
            ManualConvergeCoordinator::new(2, Duration::from_millis(1), Duration::from_millis(0));
        let now = Instant::now();
        assert_eq!(state.mark_warm_done(1, now), None);
        assert_eq!(state.mark_warm_done(2, now), Some(now));
        assert!(state.finish_warming(now));
        assert!(state.record_probe_hit(1, now + Duration::from_millis(1)));
        assert!(state.record_probe_hit(1, now + Duration::from_millis(2)));

        let _ = plan_nat4_manual_converge_send_step(
            &mut state,
            &[1, 2],
            now + Duration::from_millis(3),
        );
        let plans = plan_nat4_manual_converge_send_step(
            &mut state,
            &[1, 2],
            now + Duration::from_millis(4),
        );

        assert_eq!(
            plans,
            vec![
                Nat4SendPlan {
                    socket_id: 1,
                    should_send: true,
                    token: Some(ProbeToken {
                        role: HardNatRole::Nat4,
                        socket_id: 1,
                        generation: 1,
                        seq: 0,
                    }),
                },
                Nat4SendPlan {
                    socket_id: 2,
                    should_send: false,
                    token: None,
                },
            ]
        );
        assert_eq!(state.lease_owner(), Some(1));
        assert_eq!(
            state.socket_phase(1),
            Some(Nat4SocketPhase::LeaseOwnerValidating { generation: 1 })
        );
        assert_eq!(
            state.socket_phase(2),
            Some(Nat4SocketPhase::Silent { generation: 1 })
        );
    }

    #[test]
    fn validation_echo_promotes_owner_to_connected_after_n2_matches() {
        let mut state =
            ManualConvergeCoordinator::new(2, Duration::from_millis(1), Duration::from_millis(0));
        let now = Instant::now();
        assert_eq!(state.mark_warm_done(1, now), None);
        assert_eq!(state.mark_warm_done(2, now), Some(now));
        assert!(state.finish_warming(now));
        assert!(state.record_probe_hit(1, now + Duration::from_millis(1)));
        assert!(state.record_probe_hit(1, now + Duration::from_millis(2)));

        let _ = plan_nat4_manual_converge_send_step(
            &mut state,
            &[1, 2],
            now + Duration::from_millis(3),
        );
        let plans = plan_nat4_manual_converge_send_step(
            &mut state,
            &[1, 2],
            now + Duration::from_millis(4),
        );
        let token0 = plans[0].token.expect("first validation token");
        assert!(state.record_validation_echo(1, token0, now + Duration::from_millis(4)));
        assert_eq!(
            state.socket_phase(1),
            Some(Nat4SocketPhase::LeaseOwnerValidating { generation: 1 })
        );

        let plans = plan_nat4_manual_converge_send_step(
            &mut state,
            &[1, 2],
            now + Duration::from_millis(5),
        );
        let token1 = plans[0].token.expect("second validation token");
        assert!(state.record_validation_echo(1, token1, now + Duration::from_millis(5)));
        assert_eq!(
            state.socket_phase(1),
            Some(Nat4SocketPhase::Connected { generation: 1 })
        );
    }

    #[test]
    fn validation_echo_rejects_wrong_generation_or_unseen_seq() {
        let mut state =
            ManualConvergeCoordinator::new(2, Duration::from_millis(1), Duration::from_millis(0));
        let now = Instant::now();
        assert_eq!(state.mark_warm_done(1, now), None);
        assert_eq!(state.mark_warm_done(2, now), Some(now));
        assert!(state.finish_warming(now));
        assert!(state.record_probe_hit(1, now + Duration::from_millis(1)));
        assert!(state.record_probe_hit(1, now + Duration::from_millis(2)));

        let _ = plan_nat4_manual_converge_send_step(
            &mut state,
            &[1, 2],
            now + Duration::from_millis(3),
        );
        let plans = plan_nat4_manual_converge_send_step(
            &mut state,
            &[1, 2],
            now + Duration::from_millis(4),
        );
        let token = plans[0].token.expect("validation token");
        assert!(!state.record_validation_echo(
            1,
            ProbeToken {
                generation: token.generation + 1,
                ..token
            },
            now + Duration::from_millis(4)
        ));
        assert!(!state.record_validation_echo(
            1,
            ProbeToken {
                seq: token.seq + 9,
                ..token
            },
            now + Duration::from_millis(4)
        ));
        assert_eq!(
            state.socket_phase(1),
            Some(Nat4SocketPhase::LeaseOwnerValidating { generation: 1 })
        );
    }

    #[test]
    fn validation_timeout_releases_lease_and_recovers_after_cooldown() {
        let mut state =
            ManualConvergeCoordinator::new(2, Duration::from_millis(1), Duration::from_millis(0));
        let now = Instant::now();
        assert_eq!(state.mark_warm_done(1, now), None);
        assert_eq!(state.mark_warm_done(2, now), Some(now));
        assert!(state.finish_warming(now));
        assert!(state.record_probe_hit(1, now + Duration::from_millis(1)));
        assert!(state.record_probe_hit(1, now + Duration::from_millis(2)));

        let _ = plan_nat4_manual_converge_send_step(
            &mut state,
            &[1, 2],
            now + Duration::from_millis(3),
        );
        let _ = plan_nat4_manual_converge_send_step(
            &mut state,
            &[1, 2],
            now + Duration::from_millis(4),
        );

        let plans = plan_nat4_manual_converge_send_step(
            &mut state,
            &[1, 2],
            now + Duration::from_millis(2_600),
        );
        assert_eq!(state.lease_owner(), None);
        assert_eq!(plans[0].should_send, false);
        assert_eq!(plans[1].should_send, true);
        assert_eq!(state.socket_phase(1), Some(Nat4SocketPhase::Cooldown));
        assert_eq!(state.socket_phase(2), Some(Nat4SocketPhase::Probing));

        let plans = plan_nat4_manual_converge_send_step(
            &mut state,
            &[1, 2],
            now + Duration::from_millis(3_700),
        );
        assert_eq!(plans[0].should_send, true);
        assert_eq!(plans[1].should_send, true);
        assert_eq!(state.socket_phase(1), Some(Nat4SocketPhase::Probing));
        assert_eq!(state.socket_phase(2), Some(Nat4SocketPhase::Probing));
    }

    #[test]
    fn nat3_debug_converge_lease_echoes_nat4_tokens() {
        let token = ProbeToken {
            role: HardNatRole::Nat4,
            socket_id: 0x1a,
            generation: 0x3,
            seq: 0x2f,
        };
        let payload = encode_probe_token(token);

        let kind = classify_probe_packet(payload.as_bytes(), DEFAULT_PROBE_TEXT);
        assert_eq!(
            decide_recv_probe_action(HardNatRole::Nat3, true, &JsonProbeMode::Disabled, &kind),
            RecvProbeAction::EchoNat4Token(token)
        );
    }

    #[test]
    fn nat4_debug_converge_lease_accepts_echoed_nat4_tokens() {
        let token = ProbeToken {
            role: HardNatRole::Nat4,
            socket_id: 0x22,
            generation: 0x8,
            seq: 0x99,
        };
        let payload = encode_probe_token(token);

        assert_eq!(
            decide_recv_probe_action(
                HardNatRole::Nat4,
                true,
                &JsonProbeMode::Disabled,
                &classify_probe_packet(payload.as_bytes(), DEFAULT_PROBE_TEXT),
            ),
            RecvProbeAction::AcceptNat4TokenEcho(token)
        );
    }

    #[test]
    fn debug_converge_lease_still_accepts_plain_probe_text() {
        assert_eq!(
            decide_recv_probe_action(
                HardNatRole::Nat4,
                true,
                &JsonProbeMode::Disabled,
                &classify_probe_packet(DEFAULT_PROBE_TEXT.as_bytes(), DEFAULT_PROBE_TEXT),
            ),
            RecvProbeAction::AcceptContent
        );
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
        assert_eq!(
            plan.nat4_target,
            Some("114.249.237.39:65140".parse().unwrap())
        );
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

    #[test]
    fn derive_target_plan_prefers_explicit_nat4_target_over_ice_candidate() {
        let args = IceArgs {
            ufrag: "u".into(),
            pwd: "p".into(),
            candidates: vec![
                "candidate:1 1 udp 1694498559 198.51.100.10 40001 typ srflx raddr 0.0.0.0 rport 9"
                    .into(),
            ],
        };

        let explicit_target: SocketAddr = "203.0.113.20:54321".parse().unwrap();
        let plan =
            derive_target_plan_from_ice_with_explicit_nat4_target(&args, Some(explicit_target));
        assert_eq!(plan.parsed_candidates, 1);
        assert_eq!(plan.usable_udp_candidates, 1);
        assert_eq!(plan.nat3_target_ip, Some(explicit_target.ip()));
        assert_eq!(plan.nat4_target, Some(explicit_target));
    }

    #[test]
    fn hard_nat_session_params_roundtrip_proto_fields() {
        let params = HardNatSessionParams {
            proto_version: HARD_NAT_PROTO_VERSION,
            session_id: 0x1234_5678_9abc_def0,
            lease_timeout_ms: 15_000,
            keepalive_interval_ms: 3_000,
            batch_port_count: 96,
            ip_try_timeout_ms: 1_200,
            batch_timeout_ms: 8_000,
            connected_ttl: 64,
            nat4_candidate_ips: vec!["198.51.100.10".into(), "2001:db8::10".into()],
            nat3_public_addrs: vec!["203.0.113.10:40000".into(), "[2001:db8::20]:40001".into()],
        };
        let mut proto = P2PHardNatArgs::default();
        params.write_to_proto(&mut proto);

        let got = HardNatSessionParams::from_proto(&proto);
        assert_eq!(got, params);
    }

    fn hard_nat_start_batch_env(
        session_id: u64,
        seq: u64,
        batch_id: u64,
        nat3_addr_index: u32,
        nat4_ip_index: u32,
        ports: &[u32],
    ) -> crate::proto::HardNatControlEnvelope {
        crate::proto::HardNatControlEnvelope {
            session_id,
            seq,
            role_from: HardNatRole::Nat4 as u32,
            msg: Some(crate::proto::hard_nat_control_envelope::Msg::StartBatch(
                crate::proto::HardNatStartBatch {
                    batch_id,
                    nat3_addr_index,
                    nat4_ip_index,
                    ports: ports.to_vec(),
                    ..Default::default()
                },
            )),
            ..Default::default()
        }
    }

    fn hard_nat_keepalive_env(
        session_id: u64,
        seq: u64,
        lease_timeout_ms: u32,
    ) -> crate::proto::HardNatControlEnvelope {
        crate::proto::HardNatControlEnvelope {
            session_id,
            seq,
            role_from: HardNatRole::Nat4 as u32,
            msg: Some(
                crate::proto::hard_nat_control_envelope::Msg::LeaseKeepAlive(
                    crate::proto::HardNatLeaseKeepAlive {
                        lease_timeout_ms,
                        ..Default::default()
                    },
                ),
            ),
            ..Default::default()
        }
    }

    fn hard_nat_connected_env_with_target(
        session_id: u64,
        seq: u64,
        selected_nat3_addr: impl Into<String>,
        selected_nat4_ip: impl Into<String>,
        selected_port: u32,
    ) -> crate::proto::HardNatControlEnvelope {
        hard_nat_connected_env_with_candidate_target(
            session_id,
            seq,
            selected_nat3_addr,
            selected_nat4_ip,
            selected_port,
            0,
            0,
        )
    }

    fn hard_nat_connected_env_with_candidate_target(
        session_id: u64,
        seq: u64,
        selected_nat3_addr: impl Into<String>,
        selected_nat4_ip: impl Into<String>,
        selected_port: u32,
        selected_socket_id: u64,
        selected_generation: u64,
    ) -> crate::proto::HardNatControlEnvelope {
        crate::proto::HardNatControlEnvelope {
            session_id,
            seq,
            role_from: HardNatRole::Nat4 as u32,
            msg: Some(crate::proto::hard_nat_control_envelope::Msg::Connected(
                crate::proto::HardNatConnected {
                    selected_nat3_addr: selected_nat3_addr.into().into(),
                    selected_nat4_ip: selected_nat4_ip.into().into(),
                    selected_port,
                    selected_socket_id,
                    selected_generation,
                    restore_ttl: HARD_NAT_DEFAULT_CONNECTED_TTL,
                    ..Default::default()
                },
            )),
            ..Default::default()
        }
    }

    fn hard_nat_connected_env(session_id: u64, seq: u64) -> crate::proto::HardNatControlEnvelope {
        hard_nat_connected_env_with_target(
            session_id,
            seq,
            "203.0.113.10:40001",
            "198.51.100.20",
            40001,
        )
    }

    fn hard_nat_abort_env(session_id: u64, seq: u64) -> crate::proto::HardNatControlEnvelope {
        crate::proto::HardNatControlEnvelope {
            session_id,
            seq,
            role_from: HardNatRole::Nat4 as u32,
            msg: Some(crate::proto::hard_nat_control_envelope::Msg::Abort(
                crate::proto::HardNatAbort {
                    reason: "timed out".into(),
                    ..Default::default()
                },
            )),
            ..Default::default()
        }
    }

    fn hard_nat_ack_env(
        session_id: u64,
        seq: u64,
        acked_seq: u64,
    ) -> crate::proto::HardNatControlEnvelope {
        crate::proto::HardNatControlEnvelope {
            session_id,
            seq,
            role_from: HardNatRole::Nat3 as u32,
            msg: Some(crate::proto::hard_nat_control_envelope::Msg::Ack(
                crate::proto::HardNatAck {
                    acked_seq,
                    state: 1,
                    ..Default::default()
                },
            )),
            ..Default::default()
        }
    }

    #[test]
    fn hard_nat_scheduler_rotates_nat4_ips_then_nat3_addrs_then_next_batch() {
        let mut scheduler = HardNatScheduler::new(HardNatSchedulerConfig {
            session_id: 7,
            nat4_ip_count: 2,
            nat3_addr_count: 2,
            lease_timeout: Duration::from_secs(12),
        });

        let start = scheduler.start_batch(vec![40001, 40002]).unwrap();
        match start.msg {
            Some(crate::proto::hard_nat_control_envelope::Msg::StartBatch(msg)) => {
                assert_eq!(msg.batch_id, 1);
                assert_eq!(msg.nat3_addr_index, 0);
                assert_eq!(msg.nat4_ip_index, 0);
                assert_eq!(msg.ports, vec![40001, 40002]);
            }
            other => panic!("unexpected start batch msg: {other:?}"),
        }
        assert_eq!(scheduler.phase(), HardNatSchedulerPhase::ProbingBatch);

        let step1 = scheduler.advance_after_timeout().unwrap();
        match step1 {
            HardNatSchedulerAdvance::Send(env) => match env.msg {
                Some(crate::proto::hard_nat_control_envelope::Msg::AdvanceNat4Ip(msg)) => {
                    assert_eq!(msg.batch_id, 1);
                    assert_eq!(msg.next_nat4_ip_index, 1);
                }
                other => panic!("unexpected first advance msg: {other:?}"),
            },
            other => panic!("unexpected first advance step: {other:?}"),
        }

        let step2 = scheduler.advance_after_timeout().unwrap();
        match step2 {
            HardNatSchedulerAdvance::Send(env) => match env.msg {
                Some(crate::proto::hard_nat_control_envelope::Msg::AdvanceNat3Addr(msg)) => {
                    assert_eq!(msg.batch_id, 1);
                    assert_eq!(msg.next_nat3_addr_index, 1);
                }
                other => panic!("unexpected second advance msg: {other:?}"),
            },
            other => panic!("unexpected second advance step: {other:?}"),
        }

        let step3 = scheduler.advance_after_timeout().unwrap();
        match step3 {
            HardNatSchedulerAdvance::Send(env) => match env.msg {
                Some(crate::proto::hard_nat_control_envelope::Msg::AdvanceNat4Ip(msg)) => {
                    assert_eq!(msg.batch_id, 1);
                    assert_eq!(msg.next_nat4_ip_index, 1);
                }
                other => panic!("unexpected third advance msg: {other:?}"),
            },
            other => panic!("unexpected third advance step: {other:?}"),
        }

        assert_eq!(
            scheduler.advance_after_timeout().unwrap(),
            HardNatSchedulerAdvance::NeedNextBatch { next_batch_id: 2 }
        );

        let next = scheduler.start_next_batch(vec![41001, 41002]).unwrap();
        match next.msg {
            Some(crate::proto::hard_nat_control_envelope::Msg::NextBatch(msg)) => {
                assert_eq!(msg.next_batch_id, 2);
                assert_eq!(msg.nat3_addr_index, 0);
                assert_eq!(msg.nat4_ip_index, 0);
                assert_eq!(msg.ports, vec![41001, 41002]);
            }
            other => panic!("unexpected next batch msg: {other:?}"),
        }
    }

    #[test]
    fn hard_nat_executor_refreshes_lease_and_expires_without_keepalive() {
        let mut executor = HardNatExecutor::new(7, Duration::from_secs(5));
        let started_at = Instant::now();

        assert_eq!(
            executor.apply_control(
                hard_nat_start_batch_env(7, 1, 1, 0, 0, &[40001, 40002]),
                started_at,
            ),
            HardNatControlApply::Applied
        );
        assert_eq!(
            executor.phase(),
            HardNatExecutorPhase::ExecutingBatch(HardNatBatchCursor {
                batch_id: 1,
                nat3_addr_index: 0,
                nat4_ip_index: 0,
                ports: vec![40001, 40002],
            })
        );
        assert_eq!(
            executor.lease_deadline(),
            Some(started_at + Duration::from_secs(5))
        );

        executor.mark_waiting_for_next_command();
        assert_eq!(
            executor.phase(),
            HardNatExecutorPhase::WaitingNextCommand(HardNatBatchCursor {
                batch_id: 1,
                nat3_addr_index: 0,
                nat4_ip_index: 0,
                ports: vec![40001, 40002],
            })
        );

        assert_eq!(
            executor.apply_control(
                hard_nat_keepalive_env(7, 2, 7_000),
                started_at + Duration::from_secs(2),
            ),
            HardNatControlApply::Applied
        );
        assert_eq!(
            executor.lease_deadline(),
            Some(started_at + Duration::from_secs(9))
        );
        assert!(!executor.expire_if_needed(started_at + Duration::from_secs(8)));
        assert!(executor.expire_if_needed(started_at + Duration::from_secs(10)));
        assert_eq!(executor.phase(), HardNatExecutorPhase::Expired);
    }

    #[test]
    fn hard_nat_executor_ignores_wrong_session_and_stale_seq_control_packets() {
        let mut executor = HardNatExecutor::new(7, Duration::from_secs(5));
        let now = Instant::now();

        assert_eq!(
            executor.apply_control(hard_nat_start_batch_env(7, 10, 1, 0, 0, &[40001]), now),
            HardNatControlApply::Applied
        );
        assert_eq!(
            executor.apply_control(
                hard_nat_start_batch_env(8, 11, 2, 0, 0, &[41001]),
                now + Duration::from_secs(1),
            ),
            HardNatControlApply::IgnoredSession
        );
        assert_eq!(
            executor.apply_control(
                hard_nat_keepalive_env(7, 10, 9_000),
                now + Duration::from_secs(1),
            ),
            HardNatControlApply::IgnoredSeq
        );
        assert_eq!(
            executor.phase(),
            HardNatExecutorPhase::ExecutingBatch(HardNatBatchCursor {
                batch_id: 1,
                nat3_addr_index: 0,
                nat4_ip_index: 0,
                ports: vec![40001],
            })
        );
    }

    #[test]
    fn hard_nat_scheduler_ack_and_terminal_states_ignore_stale_events() {
        let mut scheduler = HardNatScheduler::new(HardNatSchedulerConfig {
            session_id: 7,
            nat4_ip_count: 2,
            nat3_addr_count: 1,
            lease_timeout: Duration::from_secs(12),
        });

        let start = scheduler.start_batch(vec![40001]).unwrap();
        assert!(scheduler.apply_ack(hard_nat_ack_env(7, 100, start.seq)));
        assert!(!scheduler.apply_ack(hard_nat_ack_env(7, 101, start.seq)));
        assert!(!scheduler.apply_ack(hard_nat_ack_env(8, 102, start.seq)));

        let step = scheduler.advance_after_timeout().unwrap();
        let advance_seq = match step {
            HardNatSchedulerAdvance::Send(ref env) => env.seq,
            other => panic!("unexpected advance step: {other:?}"),
        };
        assert!(scheduler.apply_ack(hard_nat_ack_env(7, 103, advance_seq)));
        assert!(!scheduler.apply_ack(hard_nat_ack_env(7, 104, start.seq)));

        let connected = scheduler.connected(
            "203.0.113.10:40001".into(),
            "198.51.100.20".into(),
            40001,
            HARD_NAT_DEFAULT_CONNECTED_TTL,
            11,
            2,
        );
        assert!(connected.is_ok());
        assert_eq!(scheduler.phase(), HardNatSchedulerPhase::Connected);
        assert_eq!(scheduler.advance_after_timeout(), None);

        let mut executor = HardNatExecutor::new(7, Duration::from_secs(5));
        let now = Instant::now();
        assert_eq!(
            executor.apply_control(hard_nat_start_batch_env(7, 1, 1, 0, 0, &[40001]), now),
            HardNatControlApply::Applied
        );
        assert_eq!(
            executor.apply_control(hard_nat_connected_env(7, 2), now + Duration::from_secs(1)),
            HardNatControlApply::Applied
        );
        assert_eq!(executor.phase(), HardNatExecutorPhase::Connected);
        assert!(!executor.expire_if_needed(now + Duration::from_secs(30)));

        let mut aborted = HardNatExecutor::new(7, Duration::from_secs(5));
        assert_eq!(
            aborted.apply_control(hard_nat_start_batch_env(7, 1, 1, 0, 0, &[40001]), now),
            HardNatControlApply::Applied
        );
        assert_eq!(
            aborted.apply_control(hard_nat_abort_env(7, 2), now + Duration::from_secs(1)),
            HardNatControlApply::Applied
        );
        assert_eq!(aborted.phase(), HardNatExecutorPhase::Aborted);
        assert!(!aborted.expire_if_needed(now + Duration::from_secs(30)));
    }

    #[test]
    fn collect_udp_candidate_ips_from_ice_dedup_and_prefers_public_srflx() {
        let args = IceArgs {
            ufrag: "u".into(),
            pwd: "p".into(),
            candidates: vec![
                "candidate:1 1 udp 2130706175 192.168.1.10 50000 typ host".into(),
                "candidate:2 1 udp 1694498559 8.8.8.8 50001 typ srflx raddr 0.0.0.0 rport 9"
                    .into(),
                "candidate:3 1 udp 1694498558 8.8.8.8 50002 typ srflx raddr 0.0.0.0 rport 9"
                    .into(),
                "candidate:4 1 tcp 1694498557 203.0.113.20 50003 typ host".into(),
                "candidate:5 1 udp 2130706175 8.8.4.4 50004 typ host".into(),
            ],
        };

        let got = collect_udp_candidate_ips_from_ice(&args);
        assert_eq!(got, vec!["8.8.8.8", "8.8.4.4", "192.168.1.10"]);
    }

    #[test]
    fn collect_public_udp_candidate_ips_from_ice_filters_non_public() {
        let args = IceArgs {
            ufrag: "u".into(),
            pwd: "p".into(),
            candidates: vec![
                "candidate:1 1 udp 2130706175 192.168.1.10 50000 typ host".into(),
                "candidate:2 1 udp 1694498559 8.8.8.8 50001 typ srflx raddr 0.0.0.0 rport 9".into(),
                "candidate:3 1 udp 1694498558 1.1.1.1 50002 typ srflx raddr 0.0.0.0 rport 9".into(),
                "candidate:4 1 udp 2130706175 198.18.0.1 50004 typ host".into(),
            ],
        };

        let got = collect_public_udp_candidate_ips_from_ice(&args);
        assert_eq!(got, vec!["8.8.8.8", "1.1.1.1"]);
    }

    #[test]
    fn apply_local_hard_nat_session_inputs_adds_local_candidates_and_nat3_addrs() {
        let mut args = P2PHardNatArgs {
            session_id: 0x99,
            scan_count: 128,
            ..Default::default()
        };
        let local_ice = IceArgs {
            ufrag: "u".into(),
            pwd: "p".into(),
            candidates: vec![
                "candidate:1 1 udp 1694498559 8.8.8.8 40001 typ srflx raddr 0.0.0.0 rport 9".into(),
                "candidate:2 1 udp 2130706175 192.168.1.10 40002 typ host".into(),
            ],
        };
        let nat3_public_addrs = vec!["203.0.113.10:54321".parse().unwrap()];
        let batch_port_count_hint = args.scan_count;

        apply_local_hard_nat_session_inputs(
            &mut args,
            batch_port_count_hint,
            &local_ice,
            &nat3_public_addrs,
        );

        assert_eq!(args.proto_version, HARD_NAT_PROTO_VERSION);
        assert_eq!(args.session_id, 0x99);
        assert_eq!(args.batch_port_count, 128);
        assert_eq!(args.connected_ttl, HARD_NAT_DEFAULT_CONNECTED_TTL);
        assert_eq!(
            args.nat4_candidate_ips
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>(),
            vec!["8.8.8.8"]
        );
        assert_eq!(
            args.nat3_public_addrs
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>(),
            vec!["203.0.113.10:54321"]
        );
    }

    #[test]
    fn merge_nat4_candidate_ips_from_sources_prefers_sample_frequency_and_public_ice_fallbacks() {
        let ice_candidates = vec![
            "8.8.8.8".to_string(),
            "1.1.1.1".to_string(),
            "192.168.1.10".to_string(),
        ];
        let sampled_addrs = vec![
            "9.9.9.9:30001".parse().unwrap(),
            "1.1.1.1:30002".parse().unwrap(),
            "9.9.9.9:30003".parse().unwrap(),
            "4.4.4.4:30004".parse().unwrap(),
            "10.0.0.1:30005".parse().unwrap(),
        ];

        let got = merge_nat4_candidate_ips_from_sources(&ice_candidates, &sampled_addrs);

        assert_eq!(got, vec!["9.9.9.9", "1.1.1.1", "4.4.4.4", "8.8.8.8"]);
    }

    #[test]
    fn apply_local_hard_nat_session_candidates_uses_explicit_nat4_ip_list() {
        let mut args = P2PHardNatArgs {
            session_id: 0x99,
            scan_count: 128,
            ..Default::default()
        };
        let batch_port_count_hint = args.scan_count;
        let local_nat4_candidate_ips = vec!["9.9.9.9".to_string(), "8.8.8.8".to_string()];
        let nat3_public_addrs = vec!["203.0.113.10:54321".parse().unwrap()];

        apply_local_hard_nat_session_candidates(
            &mut args,
            batch_port_count_hint,
            &local_nat4_candidate_ips,
            &nat3_public_addrs,
        );

        assert_eq!(args.proto_version, HARD_NAT_PROTO_VERSION);
        assert_eq!(args.session_id, 0x99);
        assert_eq!(args.batch_port_count, 128);
        assert_eq!(args.connected_ttl, HARD_NAT_DEFAULT_CONNECTED_TTL);
        assert_eq!(
            args.nat4_candidate_ips
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>(),
            vec!["9.9.9.9", "8.8.8.8"]
        );
        assert_eq!(
            args.nat3_public_addrs
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>(),
            vec!["203.0.113.10:54321"]
        );
    }

    #[test]
    fn apply_local_hard_nat_session_inputs_generates_unique_session_id_when_missing() {
        let local_ice = IceArgs {
            ufrag: "u".into(),
            pwd: "p".into(),
            candidates: vec![
                "candidate:1 1 udp 1694498559 8.8.8.8 40001 typ srflx raddr 0.0.0.0 rport 9".into(),
            ],
        };
        let nat3_public_addrs = vec!["203.0.113.10:54321".parse().unwrap()];
        let mut first = P2PHardNatArgs::default();
        let mut second = P2PHardNatArgs::default();

        apply_local_hard_nat_session_inputs(&mut first, 64, &local_ice, &nat3_public_addrs);
        apply_local_hard_nat_session_inputs(&mut second, 64, &local_ice, &nat3_public_addrs);

        assert_ne!(first.session_id, 0);
        assert_ne!(second.session_id, 0);
        assert_ne!(first.session_id, second.session_id);
    }

    #[test]
    fn resolve_nat3_stun_servers_uses_defaults_when_discovery_enabled_without_override() {
        let servers = resolve_nat3_stun_servers(true, &[]).unwrap();
        assert!(!servers.is_empty());
        let defaults = crate::ice::ice_peer::default_ice_servers()
            .into_iter()
            .map(|server| server.trim_start_matches("stun:").to_string())
            .collect::<Vec<_>>();
        assert_eq!(servers, defaults);
    }

    #[test]
    fn resolve_nat3_stun_servers_normalizes_stun_prefix_and_raw_host_port() {
        let servers = resolve_nat3_stun_servers(
            true,
            &[
                "stun:stun.miwifi.com:3478".to_string(),
                "1.1.1.1:3478".to_string(),
            ],
        )
        .unwrap();
        assert_eq!(servers, vec!["stun.miwifi.com:3478", "1.1.1.1:3478"]);
    }

    #[test]
    fn resolve_nat3_stun_servers_deduplicates_identical_servers_after_normalization() {
        let servers = resolve_nat3_stun_servers(
            true,
            &[
                "stun:stun.miwifi.com:3478".to_string(),
                "stun.miwifi.com:3478".to_string(),
                "stun:stun.miwifi.com:3478".to_string(),
            ],
        )
        .unwrap();
        assert_eq!(servers, vec!["stun.miwifi.com:3478"]);
    }

    #[test]
    fn resolve_nat3_stun_servers_rejects_turn_scheme() {
        let err = resolve_nat3_stun_servers(true, &["turn:1.1.1.1:3478".to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("turn"), "{err}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn discover_nat3_public_addr_reuses_socket_and_returns_mapping() -> Result<()> {
        let stun_socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
        let stun_addr = stun_socket.local_addr()?;
        let stun_task = tokio::spawn(async move {
            let mut buf = [0_u8; 2048];
            loop {
                let (len, from) = match stun_socket.recv_from(&mut buf).await {
                    Ok(v) => v,
                    Err(_) => return,
                };
                let req = match decode_message(&buf[..len]) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if let Some(rsp) = try_binding_response_bytes(&req, &from) {
                    let _ = stun_socket.send_to(&rsp, from).await;
                }
            }
        });

        let socket = tokio_socket_bind("127.0.0.1:0").await?;
        let local_addr = socket.local_addr()?;
        let (socket, output) = discover_nat3_public_addr(socket, &[stun_addr.to_string()]).await?;
        let mapped = output.mapped_iter().collect::<Vec<_>>();

        assert_eq!(socket.local_addr()?, local_addr);
        assert_eq!(mapped, vec![local_addr]);

        stun_task.abort();
        let _ = stun_task.await;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn discover_nat3_public_addr_returns_after_bounded_wait_when_other_server_is_silent(
    ) -> Result<()> {
        let stun_socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
        let stun_addr = stun_socket.local_addr()?;
        let stun_task = tokio::spawn(async move {
            let mut buf = [0_u8; 2048];
            loop {
                let (len, from) = match stun_socket.recv_from(&mut buf).await {
                    Ok(v) => v,
                    Err(_) => return,
                };
                let req = match decode_message(&buf[..len]) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if let Some(rsp) = try_binding_response_bytes(&req, &from) {
                    let _ = stun_socket.send_to(&rsp, from).await;
                }
            }
        });

        let silent_socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
        let silent_addr = silent_socket.local_addr()?;

        let socket = tokio_socket_bind("127.0.0.1:0").await?;
        let (_, output) = tokio::time::timeout(
            Duration::from_millis(1500),
            discover_nat3_public_addr(socket, &[stun_addr.to_string(), silent_addr.to_string()]),
        )
        .await
        .with_context(|| "discover_nat3_public_addr timed out waiting for silent stun target")??;

        assert_eq!(output.mapped_iter().count(), 1);

        stun_task.abort();
        let _ = stun_task.await;
        drop(silent_socket);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn discover_nat3_public_addr_waits_briefly_for_second_success_to_confirm_cone_nat(
    ) -> Result<()> {
        let stun_socket1 = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
        let stun_addr1 = stun_socket1.local_addr()?;
        let stun_task1 = tokio::spawn(async move {
            let mut buf = [0_u8; 2048];
            loop {
                let (len, from) = match stun_socket1.recv_from(&mut buf).await {
                    Ok(v) => v,
                    Err(_) => return,
                };
                let req = match decode_message(&buf[..len]) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if let Some(rsp) = try_binding_response_bytes(&req, &from) {
                    let _ = stun_socket1.send_to(&rsp, from).await;
                }
            }
        });

        let stun_socket2 = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
        let stun_addr2 = stun_socket2.local_addr()?;
        let stun_task2 = tokio::spawn(async move {
            let mut buf = [0_u8; 2048];
            loop {
                let (len, from) = match stun_socket2.recv_from(&mut buf).await {
                    Ok(v) => v,
                    Err(_) => return,
                };
                let req = match decode_message(&buf[..len]) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if let Some(rsp) = try_binding_response_bytes(&req, &from) {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    let _ = stun_socket2.send_to(&rsp, from).await;
                }
            }
        });

        let socket = tokio_socket_bind("127.0.0.1:0").await?;
        let local_addr = socket.local_addr()?;
        let (_, output) = tokio::time::timeout(
            Duration::from_secs(1),
            discover_nat3_public_addr(socket, &[stun_addr1.to_string(), stun_addr2.to_string()]),
        )
        .await
        .with_context(|| "discover_nat3_public_addr timed out before second stun response")??;
        let mapped = output.mapped_iter().collect::<Vec<_>>();

        assert_eq!(output.nat_type(), Some(NatType::Cone), "{output:?}");
        assert_eq!(
            recommended_nat4_target_for_nat3_discovery(&mapped, output.nat_type()),
            Some(local_addr)
        );

        stun_task1.abort();
        let _ = stun_task1.await;
        stun_task2.abort();
        let _ = stun_task2.await;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn discover_nat4_public_addrs_reuses_each_socket_and_returns_mapped_ports() -> Result<()>
    {
        let stun_socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
        let stun_addr = stun_socket.local_addr()?;
        let stun_task = tokio::spawn(async move {
            let mut buf = [0_u8; 2048];
            loop {
                let (len, from) = match stun_socket.recv_from(&mut buf).await {
                    Ok(v) => v,
                    Err(_) => return,
                };
                let req = match decode_message(&buf[..len]) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if let Some(rsp) = try_binding_response_bytes(&req, &from) {
                    let _ = stun_socket.send_to(&rsp, from).await;
                }
            }
        });

        let socket1 = tokio_socket_bind("127.0.0.1:0").await?;
        let local1 = socket1.local_addr()?;
        let socket2 = tokio_socket_bind("127.0.0.1:0").await?;
        let local2 = socket2.local_addr()?;

        let discovered =
            discover_nat4_public_addrs(vec![socket1, socket2], &[stun_addr.to_string()]).await?;

        let mut mapped = discovered
            .into_iter()
            .map(|(socket, mapped_addr)| Ok((socket.local_addr()?, mapped_addr)))
            .collect::<Result<Vec<_>>>()?;
        mapped.sort_by_key(|(local_addr, _)| *local_addr);

        let mut expected = vec![(local1, Some(local1)), (local2, Some(local2))];
        expected.sort_by_key(|(local_addr, _)| *local_addr);

        assert_eq!(mapped, expected);

        stun_task.abort();
        let _ = stun_task.await;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn discover_nat4_public_addrs_keeps_sockets_when_a_stun_probe_times_out() -> Result<()> {
        let stun_socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
        let stun_addr = stun_socket.local_addr()?;

        let socket1 = tokio_socket_bind("127.0.0.1:0").await?;
        let local1 = socket1.local_addr()?;
        let socket2 = tokio_socket_bind("127.0.0.1:0").await?;
        let local2 = socket2.local_addr()?;

        let stun_task = tokio::spawn(async move {
            let mut buf = [0_u8; 2048];
            loop {
                let (len, from) = match stun_socket.recv_from(&mut buf).await {
                    Ok(v) => v,
                    Err(_) => return,
                };
                let req = match decode_message(&buf[..len]) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if from != local2 {
                    continue;
                }
                if let Some(rsp) = try_binding_response_bytes(&req, &from) {
                    let _ = stun_socket.send_to(&rsp, from).await;
                }
            }
        });

        let discovered =
            discover_nat4_public_addrs(vec![socket1, socket2], &[stun_addr.to_string()]).await?;

        let mut mapped = discovered
            .into_iter()
            .map(|(socket, mapped_addr)| Ok((socket.local_addr()?, mapped_addr)))
            .collect::<Result<Vec<_>>>()?;
        mapped.sort_by_key(|(local_addr, _)| *local_addr);

        let mut expected = vec![(local1, None), (local2, Some(local2))];
        expected.sort_by_key(|(local_addr, _)| *local_addr);

        assert_eq!(mapped, expected);

        stun_task.abort();
        let _ = stun_task.await;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn log_nat3_public_addr_discovery_skips_recommendation_when_nat_type_unknown(
    ) -> Result<()> {
        let stun_socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
        let stun_addr = stun_socket.local_addr()?;
        let stun_task = tokio::spawn(async move {
            let mut buf = [0_u8; 2048];
            loop {
                let (len, from) = match stun_socket.recv_from(&mut buf).await {
                    Ok(v) => v,
                    Err(_) => return,
                };
                let req = match decode_message(&buf[..len]) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if let Some(rsp) = try_binding_response_bytes(&req, &from) {
                    let _ = stun_socket.send_to(&rsp, from).await;
                }
            }
        });

        let socket = tokio_socket_bind("127.0.0.1:0").await?;
        let local_addr = socket.local_addr()?;
        let (_, output) = discover_nat3_public_addr(socket, &[stun_addr.to_string()]).await?;

        let logs = capture_logs(|| log_nat3_public_addr_discovery(local_addr, &output));
        assert!(
            !logs.contains("recommended peer command"),
            "unexpected recommendation for unknown nat type: {logs}"
        );

        stun_task.abort();
        let _ = stun_task.await;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn first_connected_conn_returns_socket_handle_and_meta() -> Result<()> {
        let shared = Shared {
            connecteds: Default::default(),
            connecteds_by_socket_id: Default::default(),
            nat3_candidate_observed_remotes: Default::default(),
            nat3_current_public_addr: Default::default(),
        };
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let local_addr = socket.local_addr()?;
        let remote_addr: SocketAddr = "127.0.0.1:23456".parse()?;
        shared.record_connected(remote_addr, socket.clone(), None);

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
    async fn connected_conn_for_socket_id_returns_specific_winner_socket() -> Result<()> {
        let shared = Shared {
            connecteds: Default::default(),
            connecteds_by_socket_id: Default::default(),
            nat3_candidate_observed_remotes: Default::default(),
            nat3_current_public_addr: Default::default(),
        };
        let socket1 = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let socket2 = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let local1 = socket1.local_addr()?;
        let local2 = socket2.local_addr()?;
        let remote_addr: SocketAddr = "127.0.0.1:24567".parse()?;

        shared.record_connected(remote_addr, socket1.clone(), Some(11));
        shared.record_connected(remote_addr, socket2.clone(), Some(22));

        let start_at = Instant::now();
        let winner1 = shared
            .connected_conn_for_socket_id(HardNatRole::Nat4, 11, start_at)
            .with_context(|| "missing winner socket 11")?;
        let winner2 = shared
            .connected_conn_for_socket_id(HardNatRole::Nat4, 22, start_at)
            .with_context(|| "missing winner socket 22")?;

        assert_eq!(winner1.remote_addr, remote_addr);
        assert_eq!(winner1.local_addr, local1);
        assert_eq!(winner1.socket.local_addr()?, local1);

        assert_eq!(winner2.remote_addr, remote_addr);
        assert_eq!(winner2.local_addr, local2);
        assert_eq!(winner2.socket.local_addr()?, local2);
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
                dump_public_addrs: false,
                debug_keep_recv: false,
                debug_promote_hit_ttl: None,
                debug_converge_lease: false,
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
        let (len, from) = tokio::time::timeout(
            Duration::from_millis(300),
            conn.socket.recv_from(&mut buf),
        )
        .await
        .with_context(|| {
            "recv after run_nat4_once return timed out (possible probe recv task still reading)"
        })??;
        assert_eq!(from, echo_addr);
        assert_eq!(&buf[..len], payload);

        echo_task.abort();
        let _ = echo_task.await;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn nat4_basic_runtime_ignores_json_probe_req_and_accepts_plain_probe() -> Result<()> {
        let peer = UdpSocket::bind("127.0.0.1:0").await?;
        let sink = UdpSocket::bind("127.0.0.1:0").await?;
        let sink_addr = sink.local_addr()?;
        let sink_task = tokio::spawn(async move {
            let mut buf = [0_u8; 512];
            loop {
                let _ = sink.recv_from(&mut buf).await?;
            }
            #[allow(unreachable_code)]
            Ok::<_, anyhow::Error>(())
        });

        let args = Nat4RunConfig {
            content: None,
            target: sink_addr,
            count: 1,
            ttl: Some(4),
            interval: Duration::from_millis(20),
            dump_public_addrs: false,
            debug_keep_recv: false,
            debug_promote_hit_ttl: None,
            debug_converge_lease: false,
        };
        let runtime = prepare_nat4_probe_runtime(&args, Nat4ProbeRuntimeOptions::basic()).await?;
        let sender = runtime
            .senders
            .first()
            .with_context(|| "missing basic runtime sender")?;
        let nat4_reachable_addr =
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), sender.local.port());

        let probe_req = build_hard_nat_probe_req_packet(288, 3);
        let payload = encode_hard_nat_udp_packet(&probe_req)?;
        peer.send_to(payload.as_bytes(), nat4_reachable_addr).await?;

        let mut buf = [0_u8; 1024];
        let json_ack = tokio::time::timeout(Duration::from_millis(150), async {
            loop {
                let (len, from) = peer.recv_from(&mut buf).await?;
                if from == nat4_reachable_addr {
                    return Ok::<_, anyhow::Error>((len, from));
                }
            }
        })
        .await;
        assert!(
            json_ack.is_err(),
            "basic nat4 runtime should ignore json probe_req and not reply json probe_ack"
        );

        peer.send_to(DEFAULT_PROBE_TEXT.as_bytes(), nat4_reachable_addr)
            .await?;
        tokio::time::timeout(Duration::from_millis(300), async {
            loop {
                if runtime.shared.has_connected() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .with_context(|| "basic nat4 runtime did not accept plain probe in time")?;

        runtime.recv_tasks.abort_and_wait().await;
        sink_task.abort();
        let _ = sink_task.await;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn recv_loop_promotes_socket_ttl_on_first_valid_packet() -> Result<()> {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        socket.set_ttl(1)?;
        let local_addr = socket.local_addr()?;
        let sender = UdpSocket::bind("127.0.0.1:0").await?;
        let shared = Arc::new(Shared {
            connecteds: Default::default(),
            connecteds_by_socket_id: Default::default(),
            nat3_candidate_observed_remotes: Default::default(),
            nat3_current_public_addr: Default::default(),
        });

        let recv_task = {
            let socket = socket.clone();
            let shared = shared.clone();
            let opts = RecvLoopOptions {
                role: HardNatRole::Nat4,
                expected_text: Arc::new(DEFAULT_PROBE_TEXT.to_string()),
                promote_hit_ttl: Some(HARD_NAT_KEEP_RECV_PROMOTED_TTL),
                token_handshake_enabled: false,
                json_probe_mode: JsonProbeMode::Disabled,
                echo_content_ack: false,
                debug_converge_lease: false,
                manual_converge_socket: None,
            };
            tokio::spawn(async move { recv_loop(socket, &shared, opts).await })
        };

        sender
            .send_to(DEFAULT_PROBE_TEXT.as_bytes(), local_addr)
            .await?;

        tokio::time::timeout(Duration::from_millis(300), async {
            loop {
                if shared.has_connected()
                    && socket.ttl().ok() == Some(HARD_NAT_KEEP_RECV_PROMOTED_TTL)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .with_context(|| "recv loop did not promote ttl in time")?;

        recv_task.abort();
        let _ = recv_task.await;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn send_conn_loop_keep_recv_preserves_nat3_token_echo_path() -> Result<()> {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let local_addr = socket.local_addr()?;
        let peer = UdpSocket::bind("127.0.0.1:0").await?;
        let peer_addr = peer.local_addr()?;
        let shared = Arc::new(Shared {
            connecteds: Default::default(),
            connecteds_by_socket_id: Default::default(),
            nat3_candidate_observed_remotes: Default::default(),
            nat3_current_public_addr: Default::default(),
        });

        let recv_task = {
            let socket = socket.clone();
            let shared = shared.clone();
            let opts = RecvLoopOptions {
                role: HardNatRole::Nat3,
                expected_text: Arc::new(DEFAULT_PROBE_TEXT.to_string()),
                promote_hit_ttl: None,
                token_handshake_enabled: true,
                json_probe_mode: JsonProbeMode::Disabled,
                echo_content_ack: false,
                debug_converge_lease: true,
                manual_converge_socket: None,
            };
            tokio::spawn(async move {
                let _ = recv_loop(socket, &shared, opts).await;
            })
        };

        let send_task = {
            let socket = socket.clone();
            tokio::spawn(async move {
                send_conn_loop_keep_recv(
                    socket,
                    peer_addr,
                    DEFAULT_PROBE_TEXT,
                    Duration::from_millis(20),
                    RecvTaskGuard::new(vec![recv_task]),
                )
                .await
            })
        };

        let mut buf = [0_u8; 1024];
        let (len, from) =
            tokio::time::timeout(Duration::from_millis(300), peer.recv_from(&mut buf))
                .await
                .with_context(|| "timed out waiting for initial connected send")??;
        assert_eq!(from, local_addr);
        assert_eq!(&buf[..len], DEFAULT_PROBE_TEXT.as_bytes());

        let token = ProbeToken {
            role: HardNatRole::Nat4,
            socket_id: 0x11,
            generation: 0x22,
            seq: 0x33,
        };
        let payload = encode_probe_token(token);
        peer.send_to(payload.as_bytes(), local_addr).await?;

        let echoed = tokio::time::timeout(Duration::from_millis(300), async {
            loop {
                let (len, from) = peer.recv_from(&mut buf).await?;
                if &buf[..len] == payload.as_bytes() {
                    return Ok::<_, anyhow::Error>((len, from));
                }
            }
        })
        .await
        .with_context(|| "timed out waiting for nat3 token echo during connected send loop")??;

        assert_eq!(echoed.1, local_addr);

        send_task.abort();
        let _ = send_task.await;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn send_conn_loop_keep_recv_preserves_nat3_json_handshake_ack_path() -> Result<()> {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let local_addr = socket.local_addr()?;
        let peer = UdpSocket::bind("127.0.0.1:0").await?;
        let peer_addr = peer.local_addr()?;
        let shared = Arc::new(Shared {
            connecteds: Default::default(),
            connecteds_by_socket_id: Default::default(),
            nat3_candidate_observed_remotes: Default::default(),
            nat3_current_public_addr: Default::default(),
        });

        let recv_task = {
            let socket = socket.clone();
            let shared = shared.clone();
            let opts = RecvLoopOptions {
                role: HardNatRole::Nat3,
                expected_text: Arc::new(DEFAULT_PROBE_TEXT.to_string()),
                promote_hit_ttl: None,
                token_handshake_enabled: true,
                json_probe_mode: JsonProbeMode::Nat3HandshakeResponder {
                    next_seq: Arc::new(AtomicU64::new(0)),
                },
                echo_content_ack: false,
                debug_converge_lease: true,
                manual_converge_socket: None,
            };
            tokio::spawn(async move {
                let _ = recv_loop(socket, &shared, opts).await;
            })
        };

        let send_task = {
            let socket = socket.clone();
            tokio::spawn(async move {
                send_conn_loop_keep_recv(
                    socket,
                    peer_addr,
                    DEFAULT_PROBE_TEXT,
                    Duration::from_millis(20),
                    RecvTaskGuard::new(vec![recv_task]),
                )
                .await
            })
        };

        let mut buf = [0_u8; 1024];
        let (len, from) =
            tokio::time::timeout(Duration::from_millis(300), peer.recv_from(&mut buf))
                .await
                .with_context(|| "timed out waiting for initial connected send")??;
        assert_eq!(from, local_addr);
        assert_eq!(&buf[..len], DEFAULT_PROBE_TEXT.as_bytes());

        let handshake_req = HardNatUdpPacket {
            v: HARD_NAT_UDP_JSON_VERSION.to_string(),
            packet_type: HardNatUdpPacketType::HandshakeReq,
            session_id: 101,
            sender: HardNatUdpSender {
                role: HardNatRole::Nat4,
                socket_id: 0x11,
                generation: 0x22,
                seq: 0x33,
            },
            handshake_stage: Some(HardNatHandshakeStage::Candidate),
            ack_of: None,
            tuple: None,
        };
        let payload = encode_hard_nat_udp_packet(&handshake_req)?;
        peer.send_to(payload.as_bytes(), local_addr).await?;

        let ack = tokio::time::timeout(Duration::from_millis(300), async {
            loop {
                let (len, from) = peer.recv_from(&mut buf).await?;
                let packet = decode_hard_nat_udp_packet(std::str::from_utf8(&buf[..len])?)?;
                if packet.packet_type == HardNatUdpPacketType::HandshakeAck {
                    return Ok::<_, anyhow::Error>((packet, from));
                }
            }
        })
        .await
        .with_context(|| {
            "timed out waiting for nat3 json handshake ack during connected send loop"
        })??;

        assert_eq!(ack.1, local_addr);
        assert_eq!(ack.0.session_id, 101);
        assert_eq!(ack.0.ack_of, Some(handshake_req.sender));
        assert_eq!(
            ack.0.tuple,
            Some(HardNatTuple {
                nat3_addr: local_addr,
                nat4_addr: peer_addr,
            })
        );

        send_task.abort();
        let _ = send_task.await;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn assist_race_returns_first_success_when_both_succeed() {
        let (winner, value) = race_assist(
            Duration::from_millis(0),
            async {
                tokio::time::sleep(Duration::from_millis(10)).await;
                Ok::<_, String>("ice")
            },
            async {
                tokio::time::sleep(Duration::from_millis(30)).await;
                Ok::<_, String>("hardnat")
            },
        )
        .await
        .unwrap();

        assert_eq!(winner, AssistWinner::Ice);
        assert_eq!(value, "ice");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn assist_race_returns_both_failures_when_both_fail() {
        let err = race_assist::<(), _, _, _>(
            Duration::from_millis(0),
            async {
                tokio::time::sleep(Duration::from_millis(5)).await;
                Err::<(), _>("ice failed".to_string())
            },
            async {
                tokio::time::sleep(Duration::from_millis(10)).await;
                Err::<(), _>("hardnat failed".to_string())
            },
        )
        .await
        .unwrap_err();

        assert_eq!(err.ice_error, "ice failed");
        assert_eq!(err.hard_nat_error, "hardnat failed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn assist_race_keeps_waiting_after_ice_timeout_like_failure() {
        let (winner, value) = race_assist(
            Duration::from_millis(0),
            async {
                tokio::time::sleep(Duration::from_millis(5)).await;
                Err::<&'static str, _>("ice-timeout")
            },
            async {
                tokio::time::sleep(Duration::from_millis(15)).await;
                Ok::<_, &'static str>("hardnat-ok")
            },
        )
        .await
        .unwrap();

        assert_eq!(winner, AssistWinner::HardNat);
        assert_eq!(value, "hardnat-ok");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_nat3_controlled_once_uses_start_batch_and_connected_control() -> Result<()> {
        let peer = UdpSocket::bind("127.0.0.1:0").await?;
        let peer_addr = peer.local_addr()?;
        let (control_tx, _) = broadcast::channel(16);
        let mut control_rx = control_tx.subscribe();
        let (ack_tx, mut ack_rx) = mpsc::channel(16);

        let session = HardNatSessionParams {
            proto_version: HARD_NAT_PROTO_VERSION,
            session_id: 88,
            lease_timeout_ms: 3_000,
            keepalive_interval_ms: 300,
            batch_port_count: 1,
            connected_ttl: HARD_NAT_DEFAULT_CONNECTED_TTL,
            ..Default::default()
        };

        let nat3_task = tokio::spawn(async move {
            run_nat3_controlled_once(
                Nat3RunConfig {
                    content: None,
                    target_ip: peer_addr.ip(),
                    count: 1,
                    listen: "127.0.0.1:0".into(),
                    ttl: None,
                    interval: Duration::from_millis(30),
                    batch_interval: Duration::from_millis(200),
                    discover_public_addr: false,
                    pause_after_discovery: false,
                    hold_batch_until_enter: false,
                    debug_converge_lease: false,
                    stun_servers: Vec::new(),
                },
                session,
                &mut control_rx,
                move |env| {
                    let ack_tx = ack_tx.clone();
                    async move {
                        ack_tx
                            .send(env)
                            .await
                            .map_err(|e| anyhow::anyhow!("send hardnat ack failed: {e}"))?;
                        Ok(())
                    }
                },
            )
            .await
        });

        control_tx
            .send(hard_nat_start_batch_env(
                88,
                1,
                1,
                0,
                0,
                &[peer_addr.port() as u32],
            ))
            .unwrap();

        let start_ack = tokio::time::timeout(Duration::from_secs(1), ack_rx.recv())
            .await
            .with_context(|| "timed out waiting for nat3 start-batch ack")?
            .with_context(|| "nat3 start-batch ack channel closed")?;
        match start_ack.msg {
            Some(crate::proto::hard_nat_control_envelope::Msg::Ack(msg)) => {
                assert_eq!(start_ack.session_id, 88);
                assert_eq!(msg.acked_seq, 1);
            }
            other => panic!("unexpected nat3 start-batch ack: {other:?}"),
        }

        let mut buf = [0_u8; 1024];
        let (len, from) = tokio::time::timeout(Duration::from_secs(1), peer.recv_from(&mut buf))
            .await
            .with_context(|| "timed out waiting for nat3 probe packet")??;
        let probe_req = decode_hard_nat_udp_packet(std::str::from_utf8(&buf[..len])?)?;
        assert_eq!(probe_req.packet_type, HardNatUdpPacketType::ProbeReq);
        assert_eq!(probe_req.session_id, 88);
        assert_eq!(probe_req.sender.role, HardNatRole::Nat3);
        let probe_ack = HardNatUdpPacket {
            v: HARD_NAT_UDP_JSON_VERSION.to_string(),
            packet_type: HardNatUdpPacketType::ProbeAck,
            session_id: probe_req.session_id,
            sender: HardNatUdpSender {
                role: HardNatRole::Nat4,
                socket_id: 9,
                generation: 0,
                seq: 1,
            },
            handshake_stage: None,
            ack_of: None,
            tuple: None,
        };
        let payload = encode_hard_nat_udp_packet(&probe_ack)?;
        peer.send_to(payload.as_bytes(), from).await?;
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(
            !nat3_task.is_finished(),
            "nat3 controlled should not return connected after probe_ack before control Connected"
        );

        let handshake_req = HardNatUdpPacket {
            v: HARD_NAT_UDP_JSON_VERSION.to_string(),
            packet_type: HardNatUdpPacketType::HandshakeReq,
            session_id: probe_req.session_id,
            sender: HardNatUdpSender {
                role: HardNatRole::Nat4,
                socket_id: 9,
                generation: 1,
                seq: 7,
            },
            handshake_stage: Some(HardNatHandshakeStage::Candidate),
            ack_of: None,
            tuple: None,
        };
        let payload = encode_hard_nat_udp_packet(&handshake_req)?;
        peer.send_to(payload.as_bytes(), from).await?;
        let handshake_ack = tokio::time::timeout(Duration::from_millis(300), async {
            loop {
                let (len, from2) = peer.recv_from(&mut buf).await?;
                if from2 != from {
                    continue;
                }
                let packet = decode_hard_nat_udp_packet(std::str::from_utf8(&buf[..len])?)?;
                if packet.packet_type == HardNatUdpPacketType::HandshakeAck {
                    break Ok::<_, anyhow::Error>(packet);
                }
            }
        })
        .await
        .with_context(|| "timed out waiting for nat3 handshake_ack before connected control")??;
        assert_eq!(handshake_ack.packet_type, HardNatUdpPacketType::HandshakeAck);
        assert_eq!(
            handshake_ack.handshake_stage,
            Some(HardNatHandshakeStage::Candidate)
        );

        control_tx
            .send(hard_nat_connected_env_with_candidate_target(
                88,
                2,
                from.to_string(),
                peer_addr.ip().to_string(),
                peer_addr.port() as u32,
                handshake_req.sender.socket_id,
                handshake_req.sender.generation,
            ))
            .unwrap();

        let connected_ack = tokio::time::timeout(Duration::from_secs(1), ack_rx.recv())
            .await
            .with_context(|| "timed out waiting for nat3 connected ack")?
            .with_context(|| "nat3 connected ack channel closed")?;
        match connected_ack.msg {
            Some(crate::proto::hard_nat_control_envelope::Msg::Ack(msg)) => {
                assert_eq!(connected_ack.session_id, 88);
                assert_eq!(msg.acked_seq, 2);
            }
            other => panic!("unexpected nat3 connected ack: {other:?}"),
        }

        let conn = tokio::time::timeout(Duration::from_secs(1), nat3_task)
            .await
            .with_context(|| "timed out waiting for controlled nat3 task")???;
        assert_eq!(conn.role, HardNatRole::Nat3);
        assert_eq!(conn.remote_addr, peer_addr);
        Ok(())
    }

    #[test]
    fn nat3_default_send_mode_uses_plain_without_debug_converge_lease() {
        match nat3_default_send_mode(399, false) {
            Nat3BatchSendMode::Plain => {}
            Nat3BatchSendMode::JsonProbe { .. } => {
                panic!("plain nat3 path should keep using Nat3BatchSendMode::Plain")
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_nat3_controlled_uses_json_probe_and_replies_json_handshake_ack() -> Result<()> {
        let peer = UdpSocket::bind("127.0.0.1:0").await?;
        let peer_addr = peer.local_addr()?;
        let (control_tx, _) = broadcast::channel(16);
        let mut control_rx = control_tx.subscribe();
        let (ack_tx, mut ack_rx) = mpsc::channel(16);

        let session = HardNatSessionParams {
            proto_version: HARD_NAT_PROTO_VERSION,
            session_id: 1188,
            lease_timeout_ms: 3_000,
            keepalive_interval_ms: 300,
            batch_port_count: 1,
            connected_ttl: HARD_NAT_DEFAULT_CONNECTED_TTL,
            ..Default::default()
        };

        let nat3_task = tokio::spawn(async move {
            run_nat3_controlled_once(
                Nat3RunConfig {
                    content: None,
                    target_ip: peer_addr.ip(),
                    count: 1,
                    listen: "127.0.0.1:0".into(),
                    ttl: None,
                    interval: Duration::from_millis(30),
                    batch_interval: Duration::from_millis(200),
                    discover_public_addr: false,
                    pause_after_discovery: false,
                    hold_batch_until_enter: false,
                    debug_converge_lease: false,
                    stun_servers: Vec::new(),
                },
                session,
                &mut control_rx,
                move |env| {
                    let ack_tx = ack_tx.clone();
                    async move {
                        ack_tx
                            .send(env)
                            .await
                            .map_err(|e| anyhow::anyhow!("send hardnat ack failed: {e}"))?;
                        Ok(())
                    }
                },
            )
            .await
        });

        control_tx
            .send(hard_nat_start_batch_env(
                1188,
                1,
                1,
                0,
                0,
                &[peer_addr.port() as u32],
            ))
            .unwrap();

        let start_ack = tokio::time::timeout(Duration::from_secs(1), ack_rx.recv())
            .await
            .with_context(|| "timed out waiting for nat3 start-batch ack")?
            .with_context(|| "nat3 start-batch ack channel closed")?;
        match start_ack.msg {
            Some(crate::proto::hard_nat_control_envelope::Msg::Ack(msg)) => {
                assert_eq!(start_ack.session_id, 1188);
                assert_eq!(msg.acked_seq, 1);
            }
            other => panic!("unexpected nat3 start-batch ack: {other:?}"),
        }

        let mut buf = [0_u8; 1024];
        let (len, from) = tokio::time::timeout(Duration::from_secs(1), peer.recv_from(&mut buf))
            .await
            .with_context(|| "timed out waiting for nat3 controlled json probe")??;
        let probe_req = decode_hard_nat_udp_packet(std::str::from_utf8(&buf[..len])?)?;
        assert_eq!(probe_req.packet_type, HardNatUdpPacketType::ProbeReq);
        assert_eq!(probe_req.session_id, 1188);
        assert_eq!(probe_req.sender.role, HardNatRole::Nat3);

        let probe_ack = HardNatUdpPacket {
            v: HARD_NAT_UDP_JSON_VERSION.to_string(),
            packet_type: HardNatUdpPacketType::ProbeAck,
            session_id: probe_req.session_id,
            sender: HardNatUdpSender {
                role: HardNatRole::Nat4,
                socket_id: 9,
                generation: 0,
                seq: 1,
            },
            handshake_stage: None,
            ack_of: None,
            tuple: None,
        };
        let probe_ack_payload = encode_hard_nat_udp_packet(&probe_ack)?;
        peer.send_to(probe_ack_payload.as_bytes(), from).await?;
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(
            !nat3_task.is_finished(),
            "nat3 controlled should not return connected after probe_ack"
        );

        let first_follow_up = tokio::time::timeout(Duration::from_millis(300), async {
            loop {
                let (len, from2) = peer.recv_from(&mut buf).await?;
                if from2 != from {
                    continue;
                }
                let packet = decode_hard_nat_udp_packet(std::str::from_utf8(&buf[..len])?)?;
                if packet.packet_type == HardNatUdpPacketType::ProbeReq {
                    break Ok::<_, anyhow::Error>(packet);
                }
            }
        })
        .await
        .with_context(|| "timed out waiting for follow-up json probe_req after json probe_ack")??;
        assert_eq!(first_follow_up.session_id, probe_req.session_id);
        assert_eq!(first_follow_up.sender.role, HardNatRole::Nat3);

        let handshake_req = HardNatUdpPacket {
            v: HARD_NAT_UDP_JSON_VERSION.to_string(),
            packet_type: HardNatUdpPacketType::HandshakeReq,
            session_id: probe_req.session_id,
            sender: HardNatUdpSender {
                role: HardNatRole::Nat4,
                socket_id: 9,
                generation: 1,
                seq: 7,
            },
            handshake_stage: Some(HardNatHandshakeStage::Candidate),
            ack_of: None,
            tuple: None,
        };
        let payload = encode_hard_nat_udp_packet(&handshake_req)?;
        peer.send_to(payload.as_bytes(), from).await?;

        let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
        let mut saw_handshake_ack = false;
        let mut saw_follow_up_probe_req = false;
        let mut seen_packet_types = Vec::new();
        while tokio::time::Instant::now() < deadline
            && (!saw_handshake_ack || !saw_follow_up_probe_req)
        {
            let recv =
                tokio::time::timeout(Duration::from_millis(20), peer.recv_from(&mut buf)).await;
            let Ok(Ok((len, from2))) = recv else {
                continue;
            };
            if from2 != from {
                continue;
            }
            let packet = decode_hard_nat_udp_packet(std::str::from_utf8(&buf[..len])?)?;
            seen_packet_types.push(packet.packet_type);
            match packet.packet_type {
                HardNatUdpPacketType::HandshakeAck => {
                    saw_handshake_ack = true;
                    assert_eq!(packet.session_id, probe_req.session_id);
                    assert_eq!(packet.handshake_stage, Some(HardNatHandshakeStage::Candidate));
                    assert_eq!(packet.ack_of, Some(handshake_req.sender));
                    assert_eq!(
                        packet.tuple,
                        Some(HardNatTuple {
                            nat3_addr: from,
                            nat4_addr: peer_addr,
                        })
                    );
                }
                HardNatUdpPacketType::ProbeReq => {
                    saw_follow_up_probe_req = true;
                }
                other => bail!(
                    "nat3 controlled should reply handshake_ack then continue probe_req, got {:?}",
                    other
                ),
            }
        }
        let seen_packet_list = seen_packet_types
            .iter()
            .map(|p| format!("{:?}", p))
            .collect::<Vec<_>>()
            .join(", ");
        assert!(
            saw_handshake_ack,
            "timed out waiting for json handshake_ack after json handshake_req; seen packets: {seen_packet_list}"
        );
        assert!(
            saw_follow_up_probe_req,
            "timed out waiting for follow-up json probe_req after json handshake_ack; seen packets: {seen_packet_list}"
        );
        assert!(
            !nat3_task.is_finished(),
            "nat3 controlled should still be probing after json handshake_ack"
        );

        control_tx.send(hard_nat_abort_env(1188, 2)).unwrap();
        let abort_ack = tokio::time::timeout(Duration::from_secs(1), ack_rx.recv())
            .await
            .with_context(|| "timed out waiting for nat3 abort ack")?
            .with_context(|| "nat3 abort ack channel closed")?;
        match abort_ack.msg {
            Some(crate::proto::hard_nat_control_envelope::Msg::Ack(msg)) => {
                assert_eq!(abort_ack.session_id, 1188);
                assert_eq!(msg.acked_seq, 2);
            }
            other => panic!("unexpected nat3 abort ack: {other:?}"),
        }

        let result = tokio::time::timeout(Duration::from_secs(1), nat3_task)
            .await
            .with_context(|| "timed out waiting for controlled nat3 abort")??;
        let err = match result {
            Ok(conn) => bail!(
                "nat3 controlled characterization should return abort error, got remote [{}]",
                conn.remote_addr
            ),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("abort"),
            "unexpected nat3 controlled abort error: {err:#}"
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_nat3_controlled_once_returns_connected_after_candidate_handshake_and_connected_control_without_plain_udp_hit(
    ) -> Result<()> {
        let peer = UdpSocket::bind("127.0.0.1:0").await?;
        let peer_addr = peer.local_addr()?;
        let (control_tx, _) = broadcast::channel(16);
        let mut control_rx = control_tx.subscribe();
        let (ack_tx, mut ack_rx) = mpsc::channel(16);

        let session = HardNatSessionParams {
            proto_version: HARD_NAT_PROTO_VERSION,
            session_id: 1288,
            lease_timeout_ms: 3_000,
            keepalive_interval_ms: 300,
            batch_port_count: 1,
            connected_ttl: HARD_NAT_DEFAULT_CONNECTED_TTL,
            ..Default::default()
        };

        let nat3_task = tokio::spawn(async move {
            run_nat3_controlled_once(
                Nat3RunConfig {
                    content: None,
                    target_ip: peer_addr.ip(),
                    count: 1,
                    listen: "127.0.0.1:0".into(),
                    ttl: None,
                    interval: Duration::from_millis(30),
                    batch_interval: Duration::from_millis(200),
                    discover_public_addr: false,
                    pause_after_discovery: false,
                    hold_batch_until_enter: false,
                    debug_converge_lease: false,
                    stun_servers: Vec::new(),
                },
                session,
                &mut control_rx,
                move |env| {
                    let ack_tx = ack_tx.clone();
                    async move {
                        ack_tx
                            .send(env)
                            .await
                            .map_err(|e| anyhow::anyhow!("send hardnat ack failed: {e}"))?;
                        Ok(())
                    }
                },
            )
            .await
        });

        control_tx
            .send(hard_nat_start_batch_env(
                1288,
                1,
                1,
                0,
                0,
                &[peer_addr.port() as u32],
            ))
            .unwrap();

        let start_ack = tokio::time::timeout(Duration::from_secs(1), ack_rx.recv())
            .await
            .with_context(|| "timed out waiting for nat3 start-batch ack")?
            .with_context(|| "nat3 start-batch ack channel closed")?;
        match start_ack.msg {
            Some(crate::proto::hard_nat_control_envelope::Msg::Ack(msg)) => {
                assert_eq!(start_ack.session_id, 1288);
                assert_eq!(msg.acked_seq, 1);
            }
            other => panic!("unexpected nat3 start-batch ack: {other:?}"),
        }

        let mut buf = [0_u8; 1024];
        let (len, from) = tokio::time::timeout(Duration::from_secs(1), peer.recv_from(&mut buf))
            .await
            .with_context(|| "timed out waiting for nat3 controlled json probe")??;
        let probe_req = decode_hard_nat_udp_packet(std::str::from_utf8(&buf[..len])?)?;
        assert_eq!(probe_req.packet_type, HardNatUdpPacketType::ProbeReq);
        assert_eq!(probe_req.session_id, 1288);

        let handshake_req = HardNatUdpPacket {
            v: HARD_NAT_UDP_JSON_VERSION.to_string(),
            packet_type: HardNatUdpPacketType::HandshakeReq,
            session_id: 1288,
            sender: HardNatUdpSender {
                role: HardNatRole::Nat4,
                socket_id: 19,
                generation: 2,
                seq: 33,
            },
            handshake_stage: Some(HardNatHandshakeStage::Candidate),
            ack_of: None,
            tuple: None,
        };
        let payload = encode_hard_nat_udp_packet(&handshake_req)?;
        peer.send_to(payload.as_bytes(), from).await?;

        let handshake_ack = tokio::time::timeout(Duration::from_millis(300), async {
            loop {
                let (len, from2) = peer.recv_from(&mut buf).await?;
                if from2 != from {
                    continue;
                }
                let packet = decode_hard_nat_udp_packet(std::str::from_utf8(&buf[..len])?)?;
                if packet.packet_type == HardNatUdpPacketType::HandshakeAck {
                    break Ok::<_, anyhow::Error>(packet);
                }
            }
        })
        .await
        .with_context(|| "timed out waiting for nat3 json handshake_ack")??;
        assert_eq!(handshake_ack.session_id, 1288);
        assert_eq!(handshake_ack.ack_of, Some(handshake_req.sender));
        assert_eq!(handshake_ack.handshake_stage, Some(HardNatHandshakeStage::Candidate));
        assert_eq!(
            handshake_ack.tuple,
            Some(HardNatTuple {
                nat3_addr: from,
                nat4_addr: peer_addr,
            })
        );

        control_tx
            .send(hard_nat_connected_env_with_candidate_target(
                1288,
                2,
                from.to_string(),
                peer_addr.ip().to_string(),
                peer_addr.port() as u32,
                handshake_req.sender.socket_id,
                handshake_req.sender.generation,
            ))
            .unwrap();

        let connected_ack = tokio::time::timeout(Duration::from_secs(1), ack_rx.recv())
            .await
            .with_context(|| "timed out waiting for nat3 connected ack")?
            .with_context(|| "nat3 connected ack channel closed")?;
        match connected_ack.msg {
            Some(crate::proto::hard_nat_control_envelope::Msg::Ack(msg)) => {
                assert_eq!(connected_ack.session_id, 1288);
                assert_eq!(msg.acked_seq, 2);
            }
            other => panic!("unexpected nat3 connected ack: {other:?}"),
        }

        let conn = tokio::time::timeout(Duration::from_secs(1), nat3_task)
            .await
            .with_context(|| "timed out waiting for nat3 connected after candidate handshake")???;
        assert_eq!(conn.role, HardNatRole::Nat3);
        assert_eq!(conn.remote_addr, peer_addr);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_nat3_controlled_handshake_ack_uses_current_public_nat3_addr() -> Result<()> {
        let peer = UdpSocket::bind("127.0.0.1:0").await?;
        let peer_addr = peer.local_addr()?;
        let (control_tx, _) = broadcast::channel(16);
        let mut control_rx = control_tx.subscribe();
        let (ack_tx, mut ack_rx) = mpsc::channel(16);
        let current_nat3_addr: SocketAddr = "203.0.113.10:40001".parse()?;

        let session = HardNatSessionParams {
            proto_version: HARD_NAT_PROTO_VERSION,
            session_id: 1289,
            lease_timeout_ms: 3_000,
            keepalive_interval_ms: 300,
            batch_port_count: 1,
            connected_ttl: HARD_NAT_DEFAULT_CONNECTED_TTL,
            nat3_public_addrs: vec![current_nat3_addr.to_string()],
            ..Default::default()
        };

        let nat3_task = tokio::spawn(async move {
            run_nat3_controlled_once(
                Nat3RunConfig {
                    content: None,
                    target_ip: peer_addr.ip(),
                    count: 1,
                    listen: "127.0.0.1:0".into(),
                    ttl: None,
                    interval: Duration::from_millis(30),
                    batch_interval: Duration::from_millis(200),
                    discover_public_addr: false,
                    pause_after_discovery: false,
                    hold_batch_until_enter: false,
                    debug_converge_lease: false,
                    stun_servers: Vec::new(),
                },
                session,
                &mut control_rx,
                move |env| {
                    let ack_tx = ack_tx.clone();
                    async move {
                        ack_tx
                            .send(env)
                            .await
                            .map_err(|e| anyhow::anyhow!("send hardnat ack failed: {e}"))?;
                        Ok(())
                    }
                },
            )
            .await
        });

        control_tx
            .send(hard_nat_start_batch_env(
                1289,
                1,
                1,
                0,
                0,
                &[peer_addr.port() as u32],
            ))
            .unwrap();

        let start_ack = tokio::time::timeout(Duration::from_secs(1), ack_rx.recv())
            .await
            .with_context(|| "timed out waiting for nat3 start-batch ack")?
            .with_context(|| "nat3 start-batch ack channel closed")?;
        match start_ack.msg {
            Some(crate::proto::hard_nat_control_envelope::Msg::Ack(msg)) => {
                assert_eq!(start_ack.session_id, 1289);
                assert_eq!(msg.acked_seq, 1);
            }
            other => panic!("unexpected nat3 start-batch ack: {other:?}"),
        }

        let mut buf = [0_u8; 1024];
        let (len, from) = tokio::time::timeout(Duration::from_secs(1), peer.recv_from(&mut buf))
            .await
            .with_context(|| "timed out waiting for nat3 controlled json probe")??;
        let probe_req = decode_hard_nat_udp_packet(std::str::from_utf8(&buf[..len])?)?;
        assert_eq!(probe_req.packet_type, HardNatUdpPacketType::ProbeReq);
        assert_eq!(probe_req.session_id, 1289);

        let handshake_req = HardNatUdpPacket {
            v: HARD_NAT_UDP_JSON_VERSION.to_string(),
            packet_type: HardNatUdpPacketType::HandshakeReq,
            session_id: 1289,
            sender: HardNatUdpSender {
                role: HardNatRole::Nat4,
                socket_id: 21,
                generation: 2,
                seq: 34,
            },
            handshake_stage: Some(HardNatHandshakeStage::Candidate),
            ack_of: None,
            tuple: None,
        };
        let payload = encode_hard_nat_udp_packet(&handshake_req)?;
        peer.send_to(payload.as_bytes(), from).await?;

        let handshake_ack = tokio::time::timeout(Duration::from_millis(300), async {
            loop {
                let (len, from2) = peer.recv_from(&mut buf).await?;
                if from2 != from {
                    continue;
                }
                let packet = decode_hard_nat_udp_packet(std::str::from_utf8(&buf[..len])?)?;
                if packet.packet_type == HardNatUdpPacketType::HandshakeAck {
                    break Ok::<_, anyhow::Error>(packet);
                }
            }
        })
        .await
        .with_context(|| "timed out waiting for nat3 json handshake_ack")??;
        assert_eq!(handshake_ack.session_id, 1289);
        assert_eq!(handshake_ack.ack_of, Some(handshake_req.sender));
        assert_eq!(handshake_ack.handshake_stage, Some(HardNatHandshakeStage::Candidate));
        assert_eq!(
            handshake_ack.tuple,
            Some(HardNatTuple {
                nat3_addr: current_nat3_addr,
                nat4_addr: peer_addr,
            })
        );

        control_tx.send(hard_nat_abort_env(1289, 2)).unwrap();
        let abort_ack = tokio::time::timeout(Duration::from_secs(1), ack_rx.recv())
            .await
            .with_context(|| "timed out waiting for nat3 abort ack")?
            .with_context(|| "nat3 abort ack channel closed")?;
        match abort_ack.msg {
            Some(crate::proto::hard_nat_control_envelope::Msg::Ack(msg)) => {
                assert_eq!(abort_ack.session_id, 1289);
                assert_eq!(msg.acked_seq, 2);
            }
            other => panic!("unexpected nat3 abort ack: {other:?}"),
        }

        let result = tokio::time::timeout(Duration::from_secs(1), nat3_task)
            .await
            .with_context(|| "timed out waiting for controlled nat3 abort")??;
        let err = match result {
            Ok(conn) => bail!(
                "nat3 controlled helper should abort after handshake_ack tuple test, got remote [{}]",
                conn.remote_addr
            ),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("abort"),
            "unexpected nat3 controlled abort error: {err:#}"
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_nat3_controlled_once_does_not_return_connected_without_udp_hit() -> Result<()> {
        let peer = UdpSocket::bind("127.0.0.1:0").await?;
        let peer_addr = peer.local_addr()?;
        let (control_tx, _) = broadcast::channel(16);
        let mut control_rx = control_tx.subscribe();
        let (ack_tx, mut ack_rx) = mpsc::channel(16);

        let session = HardNatSessionParams {
            proto_version: HARD_NAT_PROTO_VERSION,
            session_id: 1088,
            lease_timeout_ms: 3_000,
            keepalive_interval_ms: 300,
            batch_port_count: 1,
            connected_ttl: HARD_NAT_DEFAULT_CONNECTED_TTL,
            ..Default::default()
        };

        let nat3_task = tokio::spawn(async move {
            run_nat3_controlled_once(
                Nat3RunConfig {
                    content: None,
                    target_ip: peer_addr.ip(),
                    count: 1,
                    listen: "127.0.0.1:0".into(),
                    ttl: None,
                    interval: Duration::from_millis(30),
                    batch_interval: Duration::from_millis(200),
                    discover_public_addr: false,
                    pause_after_discovery: false,
                    hold_batch_until_enter: false,
                    debug_converge_lease: false,
                    stun_servers: Vec::new(),
                },
                session,
                &mut control_rx,
                move |env| {
                    let ack_tx = ack_tx.clone();
                    async move {
                        ack_tx
                            .send(env)
                            .await
                            .map_err(|e| anyhow::anyhow!("send hardnat ack failed: {e}"))?;
                        Ok(())
                    }
                },
            )
            .await
        });

        control_tx
            .send(hard_nat_start_batch_env(
                1088,
                1,
                1,
                0,
                0,
                &[peer_addr.port() as u32],
            ))
            .unwrap();

        let start_ack = tokio::time::timeout(Duration::from_secs(1), ack_rx.recv())
            .await
            .with_context(|| "timed out waiting for nat3 start-batch ack")?
            .with_context(|| "nat3 start-batch ack channel closed")?;
        match start_ack.msg {
            Some(crate::proto::hard_nat_control_envelope::Msg::Ack(msg)) => {
                assert_eq!(start_ack.session_id, 1088);
                assert_eq!(msg.acked_seq, 1);
            }
            other => panic!("unexpected nat3 start-batch ack: {other:?}"),
        }

        let mut buf = [0_u8; 256];
        let (len, from) = tokio::time::timeout(Duration::from_secs(1), peer.recv_from(&mut buf))
            .await
            .with_context(|| "timed out waiting for nat3 probe packet")??;
        let probe_req = decode_hard_nat_udp_packet(std::str::from_utf8(&buf[..len])?)?;
        assert_eq!(probe_req.packet_type, HardNatUdpPacketType::ProbeReq);
        assert_eq!(probe_req.session_id, 1088);
        assert_eq!(probe_req.sender.role, HardNatRole::Nat3);
        control_tx
            .send(hard_nat_connected_env_with_target(
                1088,
                2,
                from.to_string(),
                peer_addr.ip().to_string(),
                peer_addr.port() as u32,
            ))
            .unwrap();

        let connected_ack = tokio::time::timeout(Duration::from_millis(250), ack_rx.recv()).await;
        assert!(
            connected_ack.is_err(),
            "nat3 should ignore illegal connected without candidate handshake and must not ack"
        );

        let follow_up_probe = tokio::time::timeout(Duration::from_millis(300), async {
            loop {
                let (len, from2) = peer.recv_from(&mut buf).await?;
                if from2 != from {
                    continue;
                }
                let packet = decode_hard_nat_udp_packet(std::str::from_utf8(&buf[..len])?)?;
                if packet.packet_type == HardNatUdpPacketType::ProbeReq {
                    break Ok::<_, anyhow::Error>(packet);
                }
            }
        })
        .await
        .with_context(|| "timed out waiting for follow-up probe_req after illegal Connected")??;
        assert_eq!(follow_up_probe.packet_type, HardNatUdpPacketType::ProbeReq);

        control_tx.send(hard_nat_abort_env(1088, 3)).unwrap();
        let abort_ack = tokio::time::timeout(Duration::from_secs(1), ack_rx.recv())
            .await
            .with_context(|| "timed out waiting for nat3 abort ack")?
            .with_context(|| "nat3 abort ack channel closed")?;
        match abort_ack.msg {
            Some(crate::proto::hard_nat_control_envelope::Msg::Ack(msg)) => {
                assert_eq!(abort_ack.session_id, 1088);
                assert_eq!(msg.acked_seq, 3);
            }
            other => panic!("unexpected nat3 abort ack: {other:?}"),
        }

        let result = tokio::time::timeout(Duration::from_secs(1), nat3_task)
            .await
            .with_context(|| "timed out waiting for controlled nat3 abort")??;
        let err = match result {
            Ok(conn) => bail!(
                "nat3 controlled helper should not return connected without candidate handshake, got remote [{}]",
                conn.remote_addr
            ),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("abort"),
            "unexpected nat3 controlled abort error: {err:#}"
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_nat3_controlled_ignores_connected_control_after_plain_udp_hit_without_candidate_handshake(
    ) -> Result<()> {
        let peer = UdpSocket::bind("127.0.0.1:0").await?;
        let peer_addr = peer.local_addr()?;
        let (control_tx, _) = broadcast::channel(16);
        let mut control_rx = control_tx.subscribe();
        let (ack_tx, mut ack_rx) = mpsc::channel(16);

        let session = HardNatSessionParams {
            proto_version: HARD_NAT_PROTO_VERSION,
            session_id: 2088,
            lease_timeout_ms: 3_000,
            keepalive_interval_ms: 300,
            batch_port_count: 1,
            connected_ttl: HARD_NAT_DEFAULT_CONNECTED_TTL,
            ..Default::default()
        };

        let nat3_task = tokio::spawn(async move {
            run_nat3_controlled_once(
                Nat3RunConfig {
                    content: None,
                    target_ip: peer_addr.ip(),
                    count: 1,
                    listen: "127.0.0.1:0".into(),
                    ttl: None,
                    interval: Duration::from_millis(30),
                    batch_interval: Duration::from_millis(200),
                    discover_public_addr: false,
                    pause_after_discovery: false,
                    hold_batch_until_enter: false,
                    debug_converge_lease: false,
                    stun_servers: Vec::new(),
                },
                session,
                &mut control_rx,
                move |env| {
                    let ack_tx = ack_tx.clone();
                    async move {
                        ack_tx
                            .send(env)
                            .await
                            .map_err(|e| anyhow::anyhow!("send hardnat ack failed: {e}"))?;
                        Ok(())
                    }
                },
            )
            .await
        });

        control_tx
            .send(hard_nat_start_batch_env(
                2088,
                1,
                1,
                0,
                0,
                &[peer_addr.port() as u32],
            ))
            .unwrap();

        let start_ack = tokio::time::timeout(Duration::from_secs(1), ack_rx.recv())
            .await
            .with_context(|| "timed out waiting for nat3 start-batch ack")?
            .with_context(|| "nat3 start-batch ack channel closed")?;
        match start_ack.msg {
            Some(crate::proto::hard_nat_control_envelope::Msg::Ack(msg)) => {
                assert_eq!(start_ack.session_id, 2088);
                assert_eq!(msg.acked_seq, 1);
            }
            other => panic!("unexpected nat3 start-batch ack: {other:?}"),
        }

        let mut buf = [0_u8; 1024];
        let (len, from) = tokio::time::timeout(Duration::from_secs(1), peer.recv_from(&mut buf))
            .await
            .with_context(|| "timed out waiting for nat3 probe packet")??;
        let probe_req = decode_hard_nat_udp_packet(std::str::from_utf8(&buf[..len])?)?;
        assert_eq!(probe_req.packet_type, HardNatUdpPacketType::ProbeReq);
        assert_eq!(probe_req.session_id, 2088);

        peer.send_to(DEFAULT_PROBE_TEXT.as_bytes(), from).await?;
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(
            !nat3_task.is_finished(),
            "nat3 controlled should not return connected after plain udp hit before candidate handshake"
        );

        control_tx
            .send(hard_nat_connected_env_with_target(
                2088,
                2,
                from.to_string(),
                peer_addr.ip().to_string(),
                peer_addr.port() as u32,
            ))
            .unwrap();

        let connected_ack = tokio::time::timeout(Duration::from_millis(250), ack_rx.recv()).await;
        assert!(
            connected_ack.is_err(),
            "nat3 should ignore Connected after plain udp hit without candidate handshake and must not ack"
        );

        let follow_up_probe = tokio::time::timeout(Duration::from_millis(300), async {
            loop {
                let (len, from2) = peer.recv_from(&mut buf).await?;
                if from2 != from {
                    continue;
                }
                let packet = decode_hard_nat_udp_packet(std::str::from_utf8(&buf[..len])?)?;
                if packet.packet_type == HardNatUdpPacketType::ProbeReq {
                    break Ok::<_, anyhow::Error>(packet);
                }
            }
        })
        .await
        .with_context(|| "timed out waiting for follow-up probe_req after plain hit + invalid Connected")??;
        assert_eq!(follow_up_probe.packet_type, HardNatUdpPacketType::ProbeReq);

        control_tx.send(hard_nat_abort_env(2088, 3)).unwrap();
        let abort_ack = tokio::time::timeout(Duration::from_secs(1), ack_rx.recv())
            .await
            .with_context(|| "timed out waiting for nat3 abort ack")?
            .with_context(|| "nat3 abort ack channel closed")?;
        match abort_ack.msg {
            Some(crate::proto::hard_nat_control_envelope::Msg::Ack(msg)) => {
                assert_eq!(abort_ack.session_id, 2088);
                assert_eq!(msg.acked_seq, 3);
            }
            other => panic!("unexpected nat3 abort ack: {other:?}"),
        }

        let result = tokio::time::timeout(Duration::from_secs(1), nat3_task)
            .await
            .with_context(|| "timed out waiting for controlled nat3 abort")??;
        let err = match result {
            Ok(conn) => bail!(
                "nat3 controlled should ignore Connected after plain udp hit without candidate handshake, got remote [{}]",
                conn.remote_addr
            ),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("abort"),
            "unexpected nat3 controlled abort error: {err:#}"
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn recv_loop_tracks_nat3_candidate_observation_separately_from_plain_udp_hit(
    ) -> Result<()> {
        let recv_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let recv_addr = recv_socket.local_addr()?;
        let peer = UdpSocket::bind("127.0.0.1:0").await?;
        let peer_addr = peer.local_addr()?;
        let shared = Arc::new(Shared {
            connecteds: Default::default(),
            connecteds_by_socket_id: Default::default(),
            nat3_candidate_observed_remotes: Default::default(),
            nat3_current_public_addr: Default::default(),
        });
        let opts = RecvLoopOptions {
            role: HardNatRole::Nat3,
            expected_text: Arc::new(DEFAULT_PROBE_TEXT.to_string()),
            promote_hit_ttl: None,
            token_handshake_enabled: true,
            json_probe_mode: JsonProbeMode::Nat3HandshakeResponder {
                next_seq: Arc::new(AtomicU64::new(0)),
            },
            echo_content_ack: false,
            debug_converge_lease: false,
            manual_converge_socket: None,
        };
        let recv_task = tokio::spawn({
            let recv_socket = recv_socket.clone();
            let shared = shared.clone();
            async move { recv_loop(recv_socket, &shared, opts).await }
        });

        peer.send_to(DEFAULT_PROBE_TEXT.as_bytes(), recv_addr).await?;
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(
            !shared.has_nat3_candidate_observation(peer_addr),
            "plain udp hit must not be treated as nat3 candidate observation"
        );

        let handshake_req = HardNatUdpPacket {
            v: HARD_NAT_UDP_JSON_VERSION.to_string(),
            packet_type: HardNatUdpPacketType::HandshakeReq,
            session_id: 3088,
            sender: HardNatUdpSender {
                role: HardNatRole::Nat4,
                socket_id: 29,
                generation: 3,
                seq: 44,
            },
            handshake_stage: Some(HardNatHandshakeStage::Candidate),
            ack_of: None,
            tuple: None,
        };
        let payload = encode_hard_nat_udp_packet(&handshake_req)?;
        peer.send_to(payload.as_bytes(), recv_addr).await?;

        let mut buf = [0_u8; 1024];
        let handshake_ack = tokio::time::timeout(Duration::from_millis(300), async {
            loop {
                let (len, from) = peer.recv_from(&mut buf).await?;
                if from != recv_addr {
                    continue;
                }
                let packet = decode_hard_nat_udp_packet(std::str::from_utf8(&buf[..len])?)?;
                if packet.packet_type == HardNatUdpPacketType::HandshakeAck {
                    break Ok::<_, anyhow::Error>(packet);
                }
            }
        })
        .await
        .with_context(|| "timed out waiting for nat3 handshake_ack in recv_loop test")??;
        assert_eq!(handshake_ack.ack_of, Some(handshake_req.sender));
        assert!(
            shared.has_nat3_candidate_observation(peer_addr),
            "candidate handshake must create nat3 candidate observation"
        );

        recv_task.abort();
        let _ = recv_task.await;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_nat3_controlled_ignores_connected_control_with_non_current_nat3_public_addr(
    ) -> Result<()> {
        let peer = UdpSocket::bind("127.0.0.1:0").await?;
        let peer_addr = peer.local_addr()?;
        let (control_tx, _) = broadcast::channel(16);
        let mut control_rx = control_tx.subscribe();
        let (ack_tx, mut ack_rx) = mpsc::channel(16);
        let current_nat3_addr: SocketAddr = "203.0.113.10:40001".parse()?;
        let other_nat3_addr: SocketAddr = "203.0.113.11:40002".parse()?;

        let session = HardNatSessionParams {
            proto_version: HARD_NAT_PROTO_VERSION,
            session_id: 2389,
            lease_timeout_ms: 3_000,
            keepalive_interval_ms: 300,
            batch_port_count: 1,
            connected_ttl: HARD_NAT_DEFAULT_CONNECTED_TTL,
            nat3_public_addrs: vec![
                current_nat3_addr.to_string(),
                other_nat3_addr.to_string(),
            ],
            ..Default::default()
        };

        let nat3_task = tokio::spawn(async move {
            run_nat3_controlled_once(
                Nat3RunConfig {
                    content: None,
                    target_ip: peer_addr.ip(),
                    count: 1,
                    listen: "127.0.0.1:0".into(),
                    ttl: None,
                    interval: Duration::from_millis(30),
                    batch_interval: Duration::from_millis(200),
                    discover_public_addr: false,
                    pause_after_discovery: false,
                    hold_batch_until_enter: false,
                    debug_converge_lease: false,
                    stun_servers: Vec::new(),
                },
                session,
                &mut control_rx,
                move |env| {
                    let ack_tx = ack_tx.clone();
                    async move {
                        ack_tx
                            .send(env)
                            .await
                            .map_err(|e| anyhow::anyhow!("send hardnat ack failed: {e}"))?;
                        Ok(())
                    }
                },
            )
            .await
        });

        control_tx
            .send(hard_nat_start_batch_env(
                2389,
                1,
                1,
                0,
                0,
                &[peer_addr.port() as u32],
            ))
            .unwrap();

        let start_ack = tokio::time::timeout(Duration::from_secs(1), ack_rx.recv())
            .await
            .with_context(|| "timed out waiting for nat3 start-batch ack")?
            .with_context(|| "nat3 start-batch ack channel closed")?;
        match start_ack.msg {
            Some(crate::proto::hard_nat_control_envelope::Msg::Ack(msg)) => {
                assert_eq!(start_ack.session_id, 2389);
                assert_eq!(msg.acked_seq, 1);
            }
            other => panic!("unexpected nat3 start-batch ack: {other:?}"),
        }

        let mut buf = [0_u8; 1024];
        let (len, from) = tokio::time::timeout(Duration::from_secs(1), peer.recv_from(&mut buf))
            .await
            .with_context(|| "timed out waiting for nat3 controlled json probe")??;
        let probe_req = decode_hard_nat_udp_packet(std::str::from_utf8(&buf[..len])?)?;
        assert_eq!(probe_req.packet_type, HardNatUdpPacketType::ProbeReq);

        let handshake_req = HardNatUdpPacket {
            v: HARD_NAT_UDP_JSON_VERSION.to_string(),
            packet_type: HardNatUdpPacketType::HandshakeReq,
            session_id: probe_req.session_id,
            sender: HardNatUdpSender {
                role: HardNatRole::Nat4,
                socket_id: 49,
                generation: 4,
                seq: 36,
            },
            handshake_stage: Some(HardNatHandshakeStage::Candidate),
            ack_of: None,
            tuple: None,
        };
        let payload = encode_hard_nat_udp_packet(&handshake_req)?;
        peer.send_to(payload.as_bytes(), from).await?;

        let _handshake_ack = tokio::time::timeout(Duration::from_millis(300), async {
            loop {
                let (len, from2) = peer.recv_from(&mut buf).await?;
                if from2 != from {
                    continue;
                }
                let packet = decode_hard_nat_udp_packet(std::str::from_utf8(&buf[..len])?)?;
                if packet.packet_type == HardNatUdpPacketType::HandshakeAck {
                    break Ok::<_, anyhow::Error>(packet);
                }
            }
        })
        .await
        .with_context(|| "timed out waiting for nat3 json handshake_ack")??;

        control_tx
            .send(hard_nat_connected_env_with_candidate_target(
                2389,
                2,
                other_nat3_addr.to_string(),
                peer_addr.ip().to_string(),
                peer_addr.port() as u32,
                handshake_req.sender.socket_id,
                handshake_req.sender.generation,
            ))
            .unwrap();

        let connected_ack = tokio::time::timeout(Duration::from_millis(250), ack_rx.recv()).await;
        assert!(
            connected_ack.is_err(),
            "nat3 should ignore Connected that uses a different nat3_public_addr than the observed candidate tuple"
        );

        let follow_up_probe = tokio::time::timeout(Duration::from_millis(300), async {
            loop {
                let (len, from2) = peer.recv_from(&mut buf).await?;
                if from2 != from {
                    continue;
                }
                let packet = decode_hard_nat_udp_packet(std::str::from_utf8(&buf[..len])?)?;
                if packet.packet_type == HardNatUdpPacketType::ProbeReq {
                    break Ok::<_, anyhow::Error>(packet);
                }
            }
        })
        .await
        .with_context(|| "timed out waiting for follow-up probe_req after invalid nat3 addr Connected")??;
        assert_eq!(follow_up_probe.packet_type, HardNatUdpPacketType::ProbeReq);

        control_tx.send(hard_nat_abort_env(2389, 3)).unwrap();
        let abort_ack = tokio::time::timeout(Duration::from_secs(1), ack_rx.recv())
            .await
            .with_context(|| "timed out waiting for nat3 abort ack")?
            .with_context(|| "nat3 abort ack channel closed")?;
        match abort_ack.msg {
            Some(crate::proto::hard_nat_control_envelope::Msg::Ack(msg)) => {
                assert_eq!(abort_ack.session_id, 2389);
                assert_eq!(msg.acked_seq, 3);
            }
            other => panic!("unexpected nat3 abort ack: {other:?}"),
        }

        let result = tokio::time::timeout(Duration::from_secs(1), nat3_task)
            .await
            .with_context(|| "timed out waiting for controlled nat3 abort")??;
        let err = match result {
            Ok(conn) => bail!(
                "nat3 controlled should ignore Connected with non-current nat3_public_addr, got remote [{}]",
                conn.remote_addr
            ),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("abort"),
            "unexpected nat3 controlled abort error: {err:#}"
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_nat3_controlled_ignores_connected_control_with_mismatched_candidate_generation(
    ) -> Result<()> {
        let peer = UdpSocket::bind("127.0.0.1:0").await?;
        let peer_addr = peer.local_addr()?;
        let (control_tx, _) = broadcast::channel(16);
        let mut control_rx = control_tx.subscribe();
        let (ack_tx, mut ack_rx) = mpsc::channel(16);

        let session = HardNatSessionParams {
            proto_version: HARD_NAT_PROTO_VERSION,
            session_id: 2388,
            lease_timeout_ms: 3_000,
            keepalive_interval_ms: 300,
            batch_port_count: 1,
            connected_ttl: HARD_NAT_DEFAULT_CONNECTED_TTL,
            ..Default::default()
        };

        let nat3_task = tokio::spawn(async move {
            run_nat3_controlled_once(
                Nat3RunConfig {
                    content: None,
                    target_ip: peer_addr.ip(),
                    count: 1,
                    listen: "127.0.0.1:0".into(),
                    ttl: None,
                    interval: Duration::from_millis(30),
                    batch_interval: Duration::from_millis(200),
                    discover_public_addr: false,
                    pause_after_discovery: false,
                    hold_batch_until_enter: false,
                    debug_converge_lease: false,
                    stun_servers: Vec::new(),
                },
                session,
                &mut control_rx,
                move |env| {
                    let ack_tx = ack_tx.clone();
                    async move {
                        ack_tx
                            .send(env)
                            .await
                            .map_err(|e| anyhow::anyhow!("send hardnat ack failed: {e}"))?;
                        Ok(())
                    }
                },
            )
            .await
        });

        control_tx
            .send(hard_nat_start_batch_env(
                2388,
                1,
                1,
                0,
                0,
                &[peer_addr.port() as u32],
            ))
            .unwrap();

        let start_ack = tokio::time::timeout(Duration::from_secs(1), ack_rx.recv())
            .await
            .with_context(|| "timed out waiting for nat3 start-batch ack")?
            .with_context(|| "nat3 start-batch ack channel closed")?;
        match start_ack.msg {
            Some(crate::proto::hard_nat_control_envelope::Msg::Ack(msg)) => {
                assert_eq!(start_ack.session_id, 2388);
                assert_eq!(msg.acked_seq, 1);
            }
            other => panic!("unexpected nat3 start-batch ack: {other:?}"),
        }

        let mut buf = [0_u8; 1024];
        let (len, from) = tokio::time::timeout(Duration::from_secs(1), peer.recv_from(&mut buf))
            .await
            .with_context(|| "timed out waiting for nat3 controlled json probe")??;
        let probe_req = decode_hard_nat_udp_packet(std::str::from_utf8(&buf[..len])?)?;
        assert_eq!(probe_req.packet_type, HardNatUdpPacketType::ProbeReq);

        let handshake_req = HardNatUdpPacket {
            v: HARD_NAT_UDP_JSON_VERSION.to_string(),
            packet_type: HardNatUdpPacketType::HandshakeReq,
            session_id: probe_req.session_id,
            sender: HardNatUdpSender {
                role: HardNatRole::Nat4,
                socket_id: 39,
                generation: 4,
                seq: 15,
            },
            handshake_stage: Some(HardNatHandshakeStage::Candidate),
            ack_of: None,
            tuple: None,
        };
        let payload = encode_hard_nat_udp_packet(&handshake_req)?;
        peer.send_to(payload.as_bytes(), from).await?;
        let _handshake_ack = tokio::time::timeout(Duration::from_millis(300), async {
            loop {
                let (len, from2) = peer.recv_from(&mut buf).await?;
                if from2 != from {
                    continue;
                }
                let packet = decode_hard_nat_udp_packet(std::str::from_utf8(&buf[..len])?)?;
                if packet.packet_type == HardNatUdpPacketType::HandshakeAck {
                    break Ok::<_, anyhow::Error>(packet);
                }
            }
        })
        .await
        .with_context(|| "timed out waiting for nat3 handshake_ack before invalid connected")??;

        control_tx
            .send(hard_nat_connected_env_with_candidate_target(
                2388,
                2,
                from.to_string(),
                peer_addr.ip().to_string(),
                peer_addr.port() as u32,
                handshake_req.sender.socket_id,
                handshake_req.sender.generation + 1,
            ))
            .unwrap();

        let invalid_connected_ack =
            tokio::time::timeout(Duration::from_millis(250), ack_rx.recv()).await;
        assert!(
            invalid_connected_ack.is_err(),
            "nat3 should ignore Connected with mismatched candidate generation and must not ack"
        );

        let follow_up_probe = tokio::time::timeout(Duration::from_millis(300), async {
            loop {
                let (len, from2) = peer.recv_from(&mut buf).await?;
                if from2 != from {
                    continue;
                }
                let packet = decode_hard_nat_udp_packet(std::str::from_utf8(&buf[..len])?)?;
                if packet.packet_type == HardNatUdpPacketType::ProbeReq {
                    break Ok::<_, anyhow::Error>(packet);
                }
            }
        })
        .await
        .with_context(|| "timed out waiting for follow-up probe_req after invalid Connected generation")??;
        assert_eq!(follow_up_probe.packet_type, HardNatUdpPacketType::ProbeReq);

        control_tx.send(hard_nat_abort_env(2388, 3)).unwrap();
        let abort_ack = tokio::time::timeout(Duration::from_secs(1), ack_rx.recv())
            .await
            .with_context(|| "timed out waiting for nat3 abort ack")?
            .with_context(|| "nat3 abort ack channel closed")?;
        match abort_ack.msg {
            Some(crate::proto::hard_nat_control_envelope::Msg::Ack(msg)) => {
                assert_eq!(abort_ack.session_id, 2388);
                assert_eq!(msg.acked_seq, 3);
            }
            other => panic!("unexpected nat3 abort ack: {other:?}"),
        }

        let result = tokio::time::timeout(Duration::from_secs(1), nat3_task)
            .await
            .with_context(|| "timed out waiting for controlled nat3 abort")??;
        let err = match result {
            Ok(conn) => bail!(
                "nat3 controlled should ignore Connected with mismatched generation, got remote [{}]",
                conn.remote_addr
            ),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("abort"),
            "unexpected nat3 controlled abort error: {err:#}"
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_nat3_controlled_ignores_connected_control_with_mismatched_target_and_keeps_probing(
    ) -> Result<()> {
        let peer = UdpSocket::bind("127.0.0.1:0").await?;
        let peer_addr = peer.local_addr()?;
        let (control_tx, _) = broadcast::channel(16);
        let mut control_rx = control_tx.subscribe();
        let (ack_tx, mut ack_rx) = mpsc::channel(16);

        let session = HardNatSessionParams {
            proto_version: HARD_NAT_PROTO_VERSION,
            session_id: 1388,
            lease_timeout_ms: 3_000,
            keepalive_interval_ms: 300,
            batch_port_count: 1,
            connected_ttl: HARD_NAT_DEFAULT_CONNECTED_TTL,
            ..Default::default()
        };

        let nat3_task = tokio::spawn(async move {
            run_nat3_controlled_once(
                Nat3RunConfig {
                    content: None,
                    target_ip: peer_addr.ip(),
                    count: 1,
                    listen: "127.0.0.1:0".into(),
                    ttl: None,
                    interval: Duration::from_millis(30),
                    batch_interval: Duration::from_millis(200),
                    discover_public_addr: false,
                    pause_after_discovery: false,
                    hold_batch_until_enter: false,
                    debug_converge_lease: false,
                    stun_servers: Vec::new(),
                },
                session,
                &mut control_rx,
                move |env| {
                    let ack_tx = ack_tx.clone();
                    async move {
                        ack_tx
                            .send(env)
                            .await
                            .map_err(|e| anyhow::anyhow!("send hardnat ack failed: {e}"))?;
                        Ok(())
                    }
                },
            )
            .await
        });

        control_tx
            .send(hard_nat_start_batch_env(
                1388,
                1,
                1,
                0,
                0,
                &[peer_addr.port() as u32],
            ))
            .unwrap();
        let start_ack = tokio::time::timeout(Duration::from_secs(1), ack_rx.recv())
            .await
            .with_context(|| "timed out waiting for nat3 start-batch ack")?
            .with_context(|| "nat3 start-batch ack channel closed")?;
        match start_ack.msg {
            Some(crate::proto::hard_nat_control_envelope::Msg::Ack(msg)) => {
                assert_eq!(start_ack.session_id, 1388);
                assert_eq!(msg.acked_seq, 1);
            }
            other => panic!("unexpected nat3 start-batch ack: {other:?}"),
        }

        let mut buf = [0_u8; 1024];
        let (len, from) = tokio::time::timeout(Duration::from_secs(1), peer.recv_from(&mut buf))
            .await
            .with_context(|| "timed out waiting for nat3 probe packet")??;
        let probe_req = decode_hard_nat_udp_packet(std::str::from_utf8(&buf[..len])?)?;
        assert_eq!(probe_req.packet_type, HardNatUdpPacketType::ProbeReq);

        let handshake_req = HardNatUdpPacket {
            v: HARD_NAT_UDP_JSON_VERSION.to_string(),
            packet_type: HardNatUdpPacketType::HandshakeReq,
            session_id: 1388,
            sender: HardNatUdpSender {
                role: HardNatRole::Nat4,
                socket_id: 27,
                generation: 5,
                seq: 11,
            },
            handshake_stage: Some(HardNatHandshakeStage::Candidate),
            ack_of: None,
            tuple: None,
        };
        let handshake_payload = encode_hard_nat_udp_packet(&handshake_req)?;
        peer.send_to(handshake_payload.as_bytes(), from).await?;
        let handshake_ack = tokio::time::timeout(Duration::from_millis(300), async {
            loop {
                let (len, from2) = peer.recv_from(&mut buf).await?;
                if from2 != from {
                    continue;
                }
                let packet = decode_hard_nat_udp_packet(std::str::from_utf8(&buf[..len])?)?;
                if packet.packet_type == HardNatUdpPacketType::HandshakeAck {
                    break Ok::<_, anyhow::Error>(packet);
                }
            }
        })
        .await
        .with_context(|| "timed out waiting for nat3 handshake_ack")??;
        assert_eq!(handshake_ack.packet_type, HardNatUdpPacketType::HandshakeAck);

        control_tx
            .send(hard_nat_connected_env_with_target(
                1388,
                2,
                from.to_string(),
                "127.0.0.2",
                peer_addr.port() as u32,
            ))
            .unwrap();

        let invalid_connected_ack =
            tokio::time::timeout(Duration::from_millis(250), ack_rx.recv()).await;
        assert!(
            invalid_connected_ack.is_err(),
            "nat3 should ignore mismatched connected target and must not ack"
        );

        let follow_up_probe = tokio::time::timeout(Duration::from_millis(300), async {
            loop {
                let (len, from2) = peer.recv_from(&mut buf).await?;
                if from2 != from {
                    continue;
                }
                let packet = decode_hard_nat_udp_packet(std::str::from_utf8(&buf[..len])?)?;
                if packet.packet_type == HardNatUdpPacketType::ProbeReq {
                    break Ok::<_, anyhow::Error>(packet);
                }
            }
        })
        .await
        .with_context(|| "timed out waiting for follow-up probe_req after mismatched Connected")??;
        assert_eq!(follow_up_probe.packet_type, HardNatUdpPacketType::ProbeReq);

        control_tx
            .send(hard_nat_connected_env_with_candidate_target(
                1388,
                3,
                from.to_string(),
                peer_addr.ip().to_string(),
                peer_addr.port() as u32,
                handshake_req.sender.socket_id,
                handshake_req.sender.generation,
            ))
            .unwrap();
        let valid_connected_ack = tokio::time::timeout(Duration::from_secs(1), ack_rx.recv())
            .await
            .with_context(|| "timed out waiting for nat3 valid connected ack")?
            .with_context(|| "nat3 connected ack channel closed")?;
        match valid_connected_ack.msg {
            Some(crate::proto::hard_nat_control_envelope::Msg::Ack(msg)) => {
                assert_eq!(valid_connected_ack.session_id, 1388);
                assert_eq!(msg.acked_seq, 3);
            }
            other => panic!("unexpected nat3 valid connected ack: {other:?}"),
        }

        let conn = tokio::time::timeout(Duration::from_secs(1), nat3_task)
            .await
            .with_context(|| "timed out waiting for nat3 connected after valid Connected")???;
        assert_eq!(conn.role, HardNatRole::Nat3);
        assert_eq!(conn.remote_addr, peer_addr);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_nat3_controlled_once_returns_after_abort_control() -> Result<()> {
        let peer = UdpSocket::bind("127.0.0.1:0").await?;
        let peer_addr = peer.local_addr()?;
        let (control_tx, _) = broadcast::channel(16);
        let mut control_rx = control_tx.subscribe();
        let (ack_tx, mut ack_rx) = mpsc::channel(16);

        let session = HardNatSessionParams {
            proto_version: HARD_NAT_PROTO_VERSION,
            session_id: 188,
            lease_timeout_ms: 3_000,
            keepalive_interval_ms: 300,
            batch_port_count: 1,
            connected_ttl: HARD_NAT_DEFAULT_CONNECTED_TTL,
            ..Default::default()
        };

        let nat3_task = tokio::spawn(async move {
            run_nat3_controlled_once(
                Nat3RunConfig {
                    content: None,
                    target_ip: peer_addr.ip(),
                    count: 1,
                    listen: "127.0.0.1:0".into(),
                    ttl: None,
                    interval: Duration::from_millis(30),
                    batch_interval: Duration::from_millis(200),
                    discover_public_addr: false,
                    pause_after_discovery: false,
                    hold_batch_until_enter: false,
                    debug_converge_lease: false,
                    stun_servers: Vec::new(),
                },
                session,
                &mut control_rx,
                move |env| {
                    let ack_tx = ack_tx.clone();
                    async move {
                        ack_tx
                            .send(env)
                            .await
                            .map_err(|e| anyhow::anyhow!("send hardnat ack failed: {e}"))?;
                        Ok(())
                    }
                },
            )
            .await
        });

        control_tx
            .send(hard_nat_start_batch_env(
                188,
                1,
                1,
                0,
                0,
                &[peer_addr.port() as u32],
            ))
            .unwrap();
        let start_ack = tokio::time::timeout(Duration::from_secs(1), ack_rx.recv())
            .await
            .with_context(|| "timed out waiting for nat3 start-batch ack")?
            .with_context(|| "nat3 start-batch ack channel closed")?;
        match start_ack.msg {
            Some(crate::proto::hard_nat_control_envelope::Msg::Ack(msg)) => {
                assert_eq!(msg.acked_seq, 1);
            }
            other => panic!("unexpected nat3 start-batch ack: {other:?}"),
        }

        control_tx.send(hard_nat_abort_env(188, 2)).unwrap();
        let abort_ack = tokio::time::timeout(Duration::from_secs(1), ack_rx.recv())
            .await
            .with_context(|| "timed out waiting for nat3 abort ack")?
            .with_context(|| "nat3 abort ack channel closed")?;
        match abort_ack.msg {
            Some(crate::proto::hard_nat_control_envelope::Msg::Ack(msg)) => {
                assert_eq!(msg.acked_seq, 2);
            }
            other => panic!("unexpected nat3 abort ack: {other:?}"),
        }

        let result = tokio::time::timeout(Duration::from_secs(1), nat3_task)
            .await
            .with_context(|| "timed out waiting for controlled nat3 abort")??;
        let err = match result {
            Ok(conn) => bail!(
                "nat3 controlled helper should return error after abort, got remote [{}]",
                conn.remote_addr
            ),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("abort"),
            "unexpected nat3 abort error: {err:#}"
        );
        Ok(())
    }

    #[test]
    fn build_nat3_controlled_targets_uses_cursor_nat4_ip_and_ports() -> Result<()> {
        let session = HardNatSessionParams {
            nat4_candidate_ips: vec!["198.51.100.10".into(), "198.51.100.20".into()],
            ..Default::default()
        };

        let targets = build_nat3_controlled_targets(
            &session,
            "203.0.113.10".parse()?,
            &HardNatBatchCursor {
                batch_id: 1,
                nat3_addr_index: 0,
                nat4_ip_index: 1,
                ports: vec![41001, 41002],
            },
        )?;

        assert_eq!(
            targets,
            vec![
                "198.51.100.20:41001".parse().unwrap(),
                "198.51.100.20:41002".parse().unwrap()
            ]
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn nat3_controlled_state_snapshot_reports_phase_and_connecteds() -> Result<()> {
        let mut executor = HardNatExecutor::new(188, Duration::from_secs(5));
        let now = Instant::now();
        let shared = Shared {
            connecteds: Default::default(),
            connecteds_by_socket_id: Default::default(),
            nat3_candidate_observed_remotes: Default::default(),
            nat3_current_public_addr: Default::default(),
        };

        let initial = nat3_controlled_state_snapshot(&executor, &shared);
        assert_eq!(initial.session_id, 188);
        assert_eq!(initial.phase, HardNatExecutorPhase::Idle);
        assert!(!initial.has_connected);
        assert_eq!(initial.connected_count, 0);

        assert_eq!(
            executor.apply_control(hard_nat_start_batch_env(188, 1, 1, 0, 0, &[40001]), now),
            HardNatControlApply::Applied
        );

        let after_start = nat3_controlled_state_snapshot(&executor, &shared);
        assert_eq!(
            after_start.phase,
            HardNatExecutorPhase::ExecutingBatch(HardNatBatchCursor {
                batch_id: 1,
                nat3_addr_index: 0,
                nat4_ip_index: 0,
                ports: vec![40001],
            })
        );
        assert!(!after_start.has_connected);
        assert_eq!(after_start.connected_count, 0);

        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        shared
            .connecteds
            .lock()
            .insert("127.0.0.1:40001".parse().unwrap(), socket);

        let after_connected = nat3_controlled_state_snapshot(&executor, &shared);
        assert_eq!(after_connected.session_id, 188);
        assert_eq!(
            after_connected.phase,
            HardNatExecutorPhase::ExecutingBatch(HardNatBatchCursor {
                batch_id: 1,
                nat3_addr_index: 0,
                nat4_ip_index: 0,
                ports: vec![40001],
            })
        );
        assert!(after_connected.has_connected);
        assert_eq!(after_connected.connected_count, 1);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_nat4_controlled_once_sends_start_keepalive_and_connected_control() -> Result<()> {
        let peer = UdpSocket::bind("127.0.0.1:0").await?;
        let peer_addr = peer.local_addr()?;
        let peer_events = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let control_events = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let candidate_sender = Arc::new(std::sync::Mutex::new(None::<HardNatUdpSender>));
        let (control_tx, mut control_rx) = mpsc::channel::<HardNatControlEnvelope>(16);
        // Drain control messages while nat4 is running so keepalives do not backpressure the test.
        let control_task = tokio::spawn(async move {
            let mut events = Vec::new();
            while let Some(env) = control_rx.recv().await {
                let connected = matches!(
                    env.msg,
                    Some(crate::proto::hard_nat_control_envelope::Msg::Connected(_))
                );
                events.push(env);
                if connected {
                    break;
                }
            }
            Ok::<_, anyhow::Error>(events)
        });

        let peer_events_task = peer_events.clone();
        let candidate_sender_task = candidate_sender.clone();
        let peer_task = tokio::spawn(async move {
            let peer_events = peer_events_task;
            let candidate_sender = candidate_sender_task;
            let mut buf = [0_u8; 1024];
            let (first_len, from) = peer.recv_from(&mut buf).await?;
            peer_events.lock().expect("lock peer events").push(format!(
                "recv-init:{}",
                if &buf[..first_len] == DEFAULT_PROBE_TEXT.as_bytes() {
                    "plain"
                } else {
                    "other"
                }
            ));
            let first_packet = buf[..first_len].to_vec();
            tokio::time::sleep(Duration::from_millis(120)).await;
            let nat3_addr = peer.local_addr()?;
            // High-level nat4 runtime binds 0.0.0.0:port, so tuple validation expects that local
            // address rather than the peer-visible 127.0.0.1:port source.
            let nat4_tuple_addr =
                SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), from.port());
            peer.send_to(&first_packet, from).await?;
            peer_events
                .lock()
                .expect("lock peer events")
                .push("send-plain-echo".to_string());
            let probe_req = HardNatUdpPacket {
                v: HARD_NAT_UDP_JSON_VERSION.to_string(),
                packet_type: HardNatUdpPacketType::ProbeReq,
                session_id: 99,
                sender: HardNatUdpSender {
                    role: HardNatRole::Nat3,
                    socket_id: 0,
                    generation: 0,
                    seq: 1,
                },
                handshake_stage: None,
                ack_of: None,
                tuple: None,
            };
            let probe_req_payload = encode_hard_nat_udp_packet(&probe_req)?;
            peer.send_to(probe_req_payload.as_bytes(), from).await?;
            peer_events
                .lock()
                .expect("lock peer events")
                .push("send-json-probe-req".to_string());
            let mut saw_probe_ack = false;
            let mut replied_handshake_ack = 0_u32;
            let mut handshake_req_seen = 0;
            while handshake_req_seen < 2 {
                let (len, from2) = peer.recv_from(&mut buf).await?;
                assert_eq!(from2, from);
                if &buf[..len] == DEFAULT_PROBE_TEXT.as_bytes() {
                    peer_events
                        .lock()
                        .expect("lock peer events")
                        .push("recv-plain".to_string());
                    continue;
                }
                let packet = decode_hard_nat_udp_packet(std::str::from_utf8(&buf[..len])?)?;
                match packet.packet_type {
                    HardNatUdpPacketType::ProbeAck => {
                        peer_events
                            .lock()
                            .expect("lock peer events")
                            .push("recv-probe-ack".to_string());
                        saw_probe_ack = true;
                        assert_eq!(packet.session_id, 99);
                        assert_eq!(packet.sender.role, HardNatRole::Nat4);
                    }
                    HardNatUdpPacketType::HandshakeReq => {
                        peer_events.lock().expect("lock peer events").push(format!(
                            "recv-handshake-req:{:?}",
                            packet.handshake_stage
                        ));
                        assert!(
                            saw_probe_ack,
                            "expected probe_ack before handshake_req"
                        );
                        if packet.handshake_stage == Some(HardNatHandshakeStage::Candidate) {
                            *candidate_sender.lock().expect("lock candidate sender") =
                                Some(packet.sender);
                        }
                        handshake_req_seen += 1;
                        let handshake_ack = build_hard_nat_handshake_ack_packet(
                            packet.session_id,
                            replied_handshake_ack as u64,
                            packet
                                .handshake_stage
                                .with_context(|| "handshake_req missing stage")?,
                            packet.sender,
                            nat3_addr,
                            nat4_tuple_addr,
                        );
                        let payload = encode_hard_nat_udp_packet(&handshake_ack)?;
                        peer.send_to(payload.as_bytes(), from).await?;
                        peer_events.lock().expect("lock peer events").push(format!(
                            "send-handshake-ack:{}",
                            replied_handshake_ack
                        ));
                        replied_handshake_ack += 1;
                    }
                    other => bail!("unexpected packet from nat4 controlled: {other:?}"),
                }
            }
            Ok::<_, anyhow::Error>(())
        });

        let session = HardNatSessionParams {
            proto_version: HARD_NAT_PROTO_VERSION,
            session_id: 99,
            lease_timeout_ms: 1_000,
            keepalive_interval_ms: 40,
            batch_port_count: 2,
            connected_ttl: HARD_NAT_DEFAULT_CONNECTED_TTL,
            nat4_candidate_ips: vec!["127.0.0.1".into()],
            ..Default::default()
        };

        let control_events_send = control_events.clone();
        let conn = run_nat4_controlled_once(
            Nat4RunConfig {
                content: None,
                target: peer_addr,
                count: 1,
                ttl: Some(4),
                interval: Duration::from_millis(30),
                dump_public_addrs: false,
                debug_keep_recv: false,
                debug_promote_hit_ttl: None,
                debug_converge_lease: false,
            },
            session,
            vec!["127.0.0.1".into()],
            vec![41001, 41002],
            Some("127.0.0.1".into()),
            move |env| {
                let control_tx = control_tx.clone();
                let control_events = control_events_send.clone();
                async move {
                    control_events
                        .lock()
                        .expect("lock control events")
                        .push(hard_nat_control_debug_label(&env));
                    control_tx
                        .send(env)
                        .await
                        .map_err(|e| anyhow::anyhow!("send hardnat control failed: {e}"))?;
                    Ok(())
                }
            },
        )
        ;

        let conn = match tokio::time::timeout(Duration::from_secs(2), conn).await {
            Ok(result) => result?,
            Err(err) => {
                let peer_trace = peer_events
                    .lock()
                    .expect("lock peer events")
                    .join(", ");
                let control_trace = control_events
                    .lock()
                    .expect("lock control events")
                    .join(", ");
                return Err(err).with_context(|| {
                    format!(
                        "timed out waiting for nat4 controlled connected result; peer_events=[{peer_trace}] control_events=[{control_trace}]"
                    )
                });
            }
        };

        tokio::time::timeout(Duration::from_secs(1), peer_task)
            .await
            .with_context(|| "timed out waiting for nat4 peer task to finish")???;

        let control_events: Vec<HardNatControlEnvelope> =
            tokio::time::timeout(Duration::from_secs(1), control_task)
            .await
            .with_context(|| "timed out waiting for nat4 control collector")???;

        let mut events = control_events.into_iter();
        let first = events
            .next()
            .with_context(|| "missing nat4 start-batch control")?;
        match first.msg {
            Some(crate::proto::hard_nat_control_envelope::Msg::StartBatch(msg)) => {
                assert_eq!(first.session_id, 99);
                assert_eq!(msg.batch_id, 1);
                assert_eq!(msg.ports, vec![41001, 41002]);
            }
            other => panic!("unexpected first nat4 control msg: {other:?}"),
        }

        let mut saw_keepalive = false;
        let mut saw_connected = false;
        let expected_candidate_sender = candidate_sender
            .lock()
            .expect("lock candidate sender")
            .with_context(|| "missing candidate handshake sender in nat4 controlled test")?;
        for env in events {
            match env.msg {
                Some(crate::proto::hard_nat_control_envelope::Msg::LeaseKeepAlive(msg)) => {
                    assert_eq!(env.session_id, 99);
                    assert_eq!(msg.lease_timeout_ms, 1_000);
                    saw_keepalive = true;
                }
                Some(crate::proto::hard_nat_control_envelope::Msg::Connected(msg)) => {
                    assert!(
                        saw_keepalive,
                        "expected at least one keepalive before connected"
                    );
                    assert_eq!(env.session_id, 99);
                    assert_eq!(msg.selected_nat3_addr.to_string(), peer_addr.to_string());
                    assert_eq!(msg.selected_nat4_ip.to_string(), "127.0.0.1");
                    assert_eq!(msg.restore_ttl, HARD_NAT_DEFAULT_CONNECTED_TTL);
                    assert_eq!(msg.selected_socket_id, expected_candidate_sender.socket_id);
                    assert_eq!(msg.selected_generation, expected_candidate_sender.generation);
                    saw_connected = true;
                    break;
                }
                other => panic!("unexpected nat4 follow-up control msg: {other:?}"),
            }
        }
        assert!(saw_connected, "missing nat4 connected control");

        assert_eq!(conn.role, HardNatRole::Nat4);
        assert_eq!(conn.remote_addr, peer_addr);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_nat4_controlled_once_does_not_connect_after_single_udp_hit() -> Result<()> {
        let peer = UdpSocket::bind("127.0.0.1:0").await?;
        let peer_addr = peer.local_addr()?;
        let (control_tx, mut control_rx) = mpsc::channel(32);

        let peer_task = tokio::spawn(async move {
            let mut buf = [0_u8; 256];
            let (len, from) = peer.recv_from(&mut buf).await?;
            peer.send_to(&buf[..len], from).await?;
            Ok::<_, anyhow::Error>(())
        });

        let session = HardNatSessionParams {
            proto_version: HARD_NAT_PROTO_VERSION,
            session_id: 199,
            lease_timeout_ms: 1_000,
            keepalive_interval_ms: 1_000,
            batch_port_count: 1,
            ip_try_timeout_ms: 60,
            connected_ttl: HARD_NAT_DEFAULT_CONNECTED_TTL,
            nat4_candidate_ips: vec!["127.0.0.1".into()],
            nat3_public_addrs: vec![peer_addr.to_string()],
            ..Default::default()
        };

        let nat4_task = tokio::spawn(async move {
            run_nat4_controlled_once(
                Nat4RunConfig {
                    content: None,
                    target: peer_addr,
                    count: 1,
                    ttl: Some(4),
                    interval: Duration::from_millis(20),
                    dump_public_addrs: false,
                    debug_keep_recv: false,
                    debug_promote_hit_ttl: None,
                    debug_converge_lease: false,
                },
                session,
                vec!["127.0.0.1".into()],
                vec![41101],
                Some("127.0.0.1".into()),
                move |env| {
                    let control_tx = control_tx.clone();
                    async move {
                        control_tx
                            .send(env)
                            .await
                            .map_err(|e| anyhow::anyhow!("send hardnat control failed: {e}"))?;
                        Ok(())
                    }
                },
            )
            .await
        });

        let first = tokio::time::timeout(Duration::from_secs(1), control_rx.recv())
            .await
            .with_context(|| "timed out waiting for nat4 start-batch control")?
            .with_context(|| "nat4 control channel closed before start-batch")?;
        match first.msg {
            Some(crate::proto::hard_nat_control_envelope::Msg::StartBatch(msg)) => {
                assert_eq!(first.session_id, 199);
                assert_eq!(msg.batch_id, 1);
                assert_eq!(msg.ports, vec![41101]);
            }
            other => panic!("unexpected first nat4 control msg: {other:?}"),
        }

        peer_task.await??;

        let follow_up = tokio::time::timeout(Duration::from_millis(250), async {
            loop {
                match control_rx.recv().await {
                    Some(env) => match env.msg {
                        Some(crate::proto::hard_nat_control_envelope::Msg::Connected(_)) => {
                            return Some(env);
                        }
                        _ => {}
                    },
                    None => return None,
                }
            }
        })
        .await;

        let nat4_result = if nat4_task.is_finished() {
            Some(
                nat4_task
                    .await
                    .expect("nat4 task join should succeed after early finish"),
            )
        } else {
            nat4_task.abort();
            let _ = nat4_task.await;
            None
        };

        assert!(
            follow_up.is_err(),
            "nat4 controlled should not send connected after a single udp hit: {follow_up:?}"
        );
        assert!(
            nat4_result.is_none(),
            "nat4 controlled should not return connected after a single udp hit"
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_nat4_controlled_once_advances_nat3_addr_before_connected() -> Result<()> {
        let peer1 = UdpSocket::bind("127.0.0.1:0").await?;
        let peer1_addr = peer1.local_addr()?;
        let peer2 = UdpSocket::bind("127.0.0.1:0").await?;
        let peer2_addr = peer2.local_addr()?;
        let (control_tx, mut control_rx) = mpsc::channel(32);
        let (from_tx, from_rx) = tokio::sync::oneshot::channel();
        let peer2_gate = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let peer1_task = tokio::spawn(async move {
            let mut buf = [0_u8; 256];
            let (_, from) = peer1.recv_from(&mut buf).await?;
            let _ = from_tx.send(from);
            Ok::<_, anyhow::Error>(())
        });
        let peer2_task = {
            let peer2_gate = peer2_gate.clone();
            tokio::spawn(async move {
                let mut buf = [0_u8; 256];
                let from = from_rx.await?;
                while !peer2_gate.load(std::sync::atomic::Ordering::Relaxed) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                peer2.send_to(DEFAULT_PROBE_TEXT.as_bytes(), from).await?;
                let mut echoed_tokens = 0_u32;
                loop {
                    let (len, from2) = peer2.recv_from(&mut buf).await?;
                    assert_eq!(from2, from);
                    if &buf[..len] == DEFAULT_PROBE_TEXT.as_bytes() {
                        continue;
                    }
                    peer2.send_to(&buf[..len], from).await?;
                    echoed_tokens += 1;
                    if echoed_tokens >= 3 {
                        return Ok::<_, anyhow::Error>(());
                    }
                }
            })
        };

        let session = HardNatSessionParams {
            proto_version: HARD_NAT_PROTO_VERSION,
            session_id: 299,
            lease_timeout_ms: 1_000,
            keepalive_interval_ms: 500,
            batch_port_count: 1,
            ip_try_timeout_ms: 60,
            connected_ttl: HARD_NAT_DEFAULT_CONNECTED_TTL,
            nat4_candidate_ips: vec!["203.0.113.200".into()],
            nat3_public_addrs: vec![peer1_addr.to_string(), peer2_addr.to_string()],
            ..Default::default()
        };

        let conn = tokio::time::timeout(
            Duration::from_secs(2),
            run_nat4_controlled_once(
                Nat4RunConfig {
                    content: None,
                    target: peer1_addr,
                    count: 1,
                    ttl: Some(4),
                    interval: Duration::from_millis(20),
                    dump_public_addrs: false,
                    debug_keep_recv: false,
                    debug_promote_hit_ttl: None,
                    debug_converge_lease: false,
                },
                session,
                vec!["127.0.0.1".into(), "127.0.0.2".into()],
                vec![42001],
                Some("127.0.0.1".into()),
                move |env| {
                    let control_tx = control_tx.clone();
                    let peer2_gate = peer2_gate.clone();
                    async move {
                        if matches!(
                            env.msg,
                            Some(crate::proto::hard_nat_control_envelope::Msg::AdvanceNat3Addr(_))
                        ) {
                            peer2_gate.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        control_tx
                            .send(env)
                            .await
                            .map_err(|e| anyhow::anyhow!("send hardnat control failed: {e}"))?;
                        Ok(())
                    }
                },
            ),
        )
        .await
        .with_context(|| "timed out waiting for nat4 controlled connect after addr advance")??;

        peer1_task.await??;
        peer2_task.await??;

        let mut saw_start = false;
        let mut saw_advance_nat4_ip = false;
        let mut saw_advance_nat3_addr = false;
        loop {
            let env = tokio::time::timeout(Duration::from_secs(1), control_rx.recv())
                .await
                .with_context(|| "timed out waiting for nat4 control messages")?
                .with_context(|| "nat4 control channel closed unexpectedly")?;
            match env.msg {
                Some(crate::proto::hard_nat_control_envelope::Msg::StartBatch(msg)) => {
                    saw_start = true;
                    assert_eq!(msg.batch_id, 1);
                }
                Some(crate::proto::hard_nat_control_envelope::Msg::AdvanceNat4Ip(msg)) => {
                    saw_advance_nat4_ip = true;
                    assert_eq!(msg.batch_id, 1);
                    assert_eq!(msg.next_nat4_ip_index, 1);
                }
                Some(crate::proto::hard_nat_control_envelope::Msg::AdvanceNat3Addr(msg)) => {
                    saw_advance_nat3_addr = true;
                    assert_eq!(msg.batch_id, 1);
                    assert_eq!(msg.next_nat3_addr_index, 1);
                }
                Some(crate::proto::hard_nat_control_envelope::Msg::Connected(msg)) => {
                    assert!(saw_start, "expected start-batch before connected");
                    assert!(
                        saw_advance_nat4_ip,
                        "expected advance-nat4-ip before connected"
                    );
                    assert!(
                        saw_advance_nat3_addr,
                        "expected advance-nat3-addr before connected"
                    );
                    assert_eq!(msg.selected_nat3_addr.to_string(), peer2_addr.to_string());
                    let selected_nat4_ip = msg.selected_nat4_ip.to_string();
                    assert!(
                        selected_nat4_ip == "127.0.0.1" || selected_nat4_ip == "127.0.0.2",
                        "unexpected selected nat4 ip: {}",
                        selected_nat4_ip
                    );
                    break;
                }
                Some(crate::proto::hard_nat_control_envelope::Msg::LeaseKeepAlive(_)) => {}
                other => panic!("unexpected nat4 control msg during addr advance test: {other:?}"),
            }
        }

        assert_eq!(conn.role, HardNatRole::Nat4);
        assert_eq!(conn.remote_addr, peer2_addr);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_nat4_controlled_once_starts_next_batch_after_timeouts() -> Result<()> {
        let peer = UdpSocket::bind("127.0.0.1:0").await?;
        let peer_addr = peer.local_addr()?;
        let (control_tx, mut control_rx) = mpsc::channel(32);
        let gate = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let peer_task = {
            let gate = gate.clone();
            tokio::spawn(async move {
                let mut buf = [0_u8; 256];
                let (_, from) = peer.recv_from(&mut buf).await?;
                let mut started = false;
                let mut echoed_tokens = 0_u32;
                loop {
                    if !started && gate.load(std::sync::atomic::Ordering::Relaxed) {
                        peer.send_to(DEFAULT_PROBE_TEXT.as_bytes(), from).await?;
                        started = true;
                    }
                    let recv =
                        tokio::time::timeout(Duration::from_millis(10), peer.recv_from(&mut buf))
                            .await;
                    let Ok(Ok((len, from2))) = recv else {
                        continue;
                    };
                    assert_eq!(from2, from);
                    if !started {
                        continue;
                    }
                    if &buf[..len] == DEFAULT_PROBE_TEXT.as_bytes() {
                        continue;
                    }
                    if gate.load(std::sync::atomic::Ordering::Relaxed) {
                        peer.send_to(&buf[..len], from).await?;
                        echoed_tokens += 1;
                        if echoed_tokens >= 3 {
                            return Ok::<_, anyhow::Error>(());
                        }
                    }
                }
            })
        };

        let session = HardNatSessionParams {
            proto_version: HARD_NAT_PROTO_VERSION,
            session_id: 399,
            lease_timeout_ms: 1_000,
            keepalive_interval_ms: 500,
            batch_port_count: 1,
            ip_try_timeout_ms: 50,
            connected_ttl: HARD_NAT_DEFAULT_CONNECTED_TTL,
            nat4_candidate_ips: vec!["127.0.0.1".into()],
            nat3_public_addrs: vec![peer_addr.to_string()],
            ..Default::default()
        };

        let conn = tokio::time::timeout(
            Duration::from_secs(2),
            run_nat4_controlled_once(
                Nat4RunConfig {
                    content: None,
                    target: peer_addr,
                    count: 1,
                    ttl: Some(4),
                    interval: Duration::from_millis(20),
                    dump_public_addrs: false,
                    debug_keep_recv: false,
                    debug_promote_hit_ttl: None,
                    debug_converge_lease: false,
                },
                session,
                vec!["127.0.0.1".into()],
                vec![43001],
                Some("127.0.0.1".into()),
                move |env| {
                    let control_tx = control_tx.clone();
                    let gate = gate.clone();
                    async move {
                        if matches!(
                            env.msg,
                            Some(crate::proto::hard_nat_control_envelope::Msg::NextBatch(_))
                        ) {
                            gate.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        control_tx
                            .send(env)
                            .await
                            .map_err(|e| anyhow::anyhow!("send hardnat control failed: {e}"))?;
                        Ok(())
                    }
                },
            ),
        )
        .await
        .with_context(|| "timed out waiting for nat4 controlled connect after next-batch")??;

        peer_task.await??;

        let mut saw_next_batch = false;
        loop {
            let env = tokio::time::timeout(Duration::from_secs(1), control_rx.recv())
                .await
                .with_context(|| "timed out waiting for nat4 next-batch control")?
                .with_context(|| "nat4 control channel closed unexpectedly")?;
            match env.msg {
                Some(crate::proto::hard_nat_control_envelope::Msg::NextBatch(msg)) => {
                    saw_next_batch = true;
                    assert!(
                        msg.next_batch_id >= 2,
                        "expected next batch id >= 2, got {}",
                        msg.next_batch_id
                    );
                    assert_eq!(msg.ports.len(), 1);
                }
                Some(crate::proto::hard_nat_control_envelope::Msg::Connected(msg)) => {
                    assert!(saw_next_batch, "expected next-batch before connected");
                    assert_eq!(msg.selected_nat3_addr.to_string(), peer_addr.to_string());
                    break;
                }
                Some(crate::proto::hard_nat_control_envelope::Msg::StartBatch(_))
                | Some(crate::proto::hard_nat_control_envelope::Msg::LeaseKeepAlive(_)) => {}
                other => panic!("unexpected nat4 control msg during next-batch test: {other:?}"),
            }
        }

        assert_eq!(conn.role, HardNatRole::Nat4);
        assert_eq!(conn.remote_addr, peer_addr);
        Ok(())
    }
}
