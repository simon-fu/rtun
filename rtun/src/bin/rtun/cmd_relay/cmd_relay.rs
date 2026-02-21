use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    fs::OpenOptions,
    io::{self, IsTerminal, Write},
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use chrono::Local;
use clap::Parser;
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{self, Event as CtEvent, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    style::{Color, Print, ResetColor, SetForegroundColor},
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use regex::Regex;
use rtun::{
    async_rt::{run_multi_thread, spawn_with_name},
    hex::BinStrLine,
    ice::ice_peer::{default_ice_servers, IceArgs, IceConfig, IcePeer},
    proto::{
        open_p2presponse::Open_p2p_rsp, p2pargs::P2p_args, ExecAgentScriptArgs,
        ExecAgentScriptResult, P2PArgs, UdpRelayArgs,
    },
    switch::{
        invoker_ctrl::{CtrlHandler, CtrlInvoker},
        session_stream::make_stream_session,
        udp_relay_codec::{
            decode_udp_relay_packet, encode_udp_relay_packet, gen_udp_relay_obfs_seed,
            packet_has_stun_magic, UdpRelayCodec,
            UDP_RELAY_FLOW_ID_MASK_OBFS, UDP_RELAY_HEARTBEAT_FLOW_ID, UDP_RELAY_META_LEN_OBFS,
        },
    },
    ws::client::ws_connect_to,
};
use tokio::{
    net::UdpSocket,
    sync::{broadcast, mpsc, oneshot},
    task::JoinHandle,
    time::{self, MissedTickBehavior},
};

use crate::{
    client_utils::get_agents,
    init_log2, quic_signal,
    rest_proto::{make_sub_url, make_ws_scheme, AgentInfo},
    secret::token_gen,
};

const DEFAULT_UDP_IDLE_TIMEOUT_SECS: u64 = 120;
const DEFAULT_P2P_PACKET_LIMIT: usize = 1452 ;
const LOOP_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const FLOW_CLEANUP_INTERVAL: Duration = Duration::from_secs(1);
const UDP_RELAY_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(3);
const UDP_RELAY_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_P2P_MIN_CHANNELS: usize = 1;
const DEFAULT_P2P_MAX_CHANNELS: usize = 1;
const DEFAULT_P2P_CHANNEL_LIFETIME_SECS: u64 = 120;
const AGENT_REPLACE_AHEAD: Duration = Duration::from_secs(10 * 60);
const P2P_EXPAND_CONNECT_TIMEOUT_INITIAL: Duration = Duration::from_secs(5);
const P2P_EXPAND_CONNECT_TIMEOUT_MAX: Duration = Duration::from_secs(30);
const FLOW_ROUTE_RESELECT_INTERVAL: Duration = Duration::from_secs(5);
const FLOW_MIGRATE_STALE_GAP: Duration = Duration::from_secs(6);
const FLOW_MIGRATE_LOAD_GAP: usize = 2;
const TUNNEL_STALE_THRESHOLD: Duration = Duration::from_secs(12);
const RELAY_TUNNEL_DOWN_TTL: Duration = Duration::from_secs(10);
const RELAY_TUI_REFRESH_INTERVAL: Duration = Duration::from_millis(300);
const RELAY_TUI_MAX_EVENT_BUFFER: usize = 256;
const DEFAULT_AGENT_SCRIPT_TIMEOUT_SECS: u64 = 30;
const AGENT_SCRIPT_STDOUT_LIMIT: u32 = 256 * 1024;
const AGENT_SCRIPT_STDERR_LIMIT: u32 = 256 * 1024;

fn next_expand_connect_timeout(curr: Duration) -> Duration {
    let doubled_ms = curr.as_millis().saturating_mul(2);
    let max_ms = P2P_EXPAND_CONNECT_TIMEOUT_MAX.as_millis();
    let next_ms = doubled_ms.min(max_ms);
    Duration::from_millis(next_ms as u64)
}

pub fn run(args: CmdArgs) -> Result<()> {
    init_relay_log(&args)?;
    run_multi_thread(do_run(args))??;
    Ok(())
}

type SharedLogFile = Arc<std::sync::Mutex<std::fs::File>>;

fn init_relay_log(args: &CmdArgs) -> Result<()> {
    let to_stdout = !args.tui;
    let log_file = match args.log_file.as_ref() {
        Some(path) => {
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .with_context(|| format!("open relay log file failed [{}]", path.display()))?;
            Some(Arc::new(std::sync::Mutex::new(file)))
        }
        None => None,
    };

    init_log2(move || RelayLogWriter::new(to_stdout, log_file.clone()));
    Ok(())
}

struct RelayLogWriter {
    to_stdout: bool,
    log_file: Option<SharedLogFile>,
}

impl RelayLogWriter {
    fn new(to_stdout: bool, log_file: Option<SharedLogFile>) -> Self {
        Self {
            to_stdout,
            log_file,
        }
    }
}

impl io::Write for RelayLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.to_stdout {
            io::stdout().write_all(buf)?;
        }
        if let Some(log_file) = self.log_file.as_ref() {
            let mut file = log_file.lock().map_err(|_| {
                io::Error::new(io::ErrorKind::Other, "relay log file lock poisoned")
            })?;
            file.write_all(buf)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.to_stdout {
            io::stdout().flush()?;
        }
        if let Some(log_file) = self.log_file.as_ref() {
            let mut file = log_file.lock().map_err(|_| {
                io::Error::new(io::ErrorKind::Other, "relay log file lock poisoned")
            })?;
            file.flush()?;
        }
        Ok(())
    }
}

fn now_millis_u64() -> u64 {
    Local::now().timestamp_millis() as u64
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
enum RelayLifecycleEvent {
    AgentSelected {
        name: String,
        instance_id: Option<String>,
        addr: String,
        expire_at: u64,
    },
    AgentSwitched {
        old_name: String,
        old_instance_id: Option<String>,
        new_name: String,
        new_instance_id: Option<String>,
    },
    AgentSessionConnected {
        name: String,
        instance_id: Option<String>,
    },
    AgentSessionClosed {
        name: String,
        instance_id: Option<String>,
        reason: String,
    },
    TunnelOpened {
        tunnel_idx: usize,
        tunnel_id: u64,
        mode: String,
        source: &'static str,
    },
    TunnelRotated {
        old_tunnel_idx: usize,
        old_tunnel_id: u64,
        new_tunnel_idx: usize,
        new_tunnel_id: u64,
    },
    TunnelClosed {
        tunnel_idx: usize,
        tunnel_id: Option<u64>,
        reason: String,
    },
    FlowCreated {
        flow_id: u64,
        src: SocketAddr,
        target: SocketAddr,
        tunnel_idx: usize,
        tunnel_id: Option<u64>,
    },
    FlowMigrated {
        flow_id: u64,
        src: SocketAddr,
        old_tunnel_idx: usize,
        old_tunnel_id: Option<u64>,
        new_tunnel_idx: usize,
        new_tunnel_id: Option<u64>,
        reason: &'static str,
    },
    FlowClosed {
        flow_id: u64,
        src: SocketAddr,
        tunnel_idx: usize,
        tunnel_id: Option<u64>,
        reason: String,
    },
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
struct RelayAgentSnapshot {
    name: String,
    instance_id: Option<String>,
    addr: String,
    expire_at: u64,
    connected: bool,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
struct RelayTunnelSnapshot {
    tunnel_idx: usize,
    tunnel_id: u64,
    alive: bool,
    allocatable: bool,
    mode: Option<String>,
    flow_count: usize,
    last_active_ago_ms: Option<u64>,
    expires_in_ms: Option<u64>,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
struct RelayFlowSnapshot {
    flow_id: u64,
    src: SocketAddr,
    target: SocketAddr,
    tunnel_idx: usize,
    tunnel_id: Option<u64>,
    idle_for_ms: u64,
    route_age_ms: u64,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
struct RelayStateSnapshot {
    listen: SocketAddr,
    target: SocketAddr,
    flow_idle_timeout_ms: u64,
    updated_at_ms: u64,
    selected_agent: Option<RelayAgentSnapshot>,
    tunnels: Vec<RelayTunnelSnapshot>,
    flows: Vec<RelayFlowSnapshot>,
}

#[derive(Debug, Clone)]
struct RelayStateHub {
    event_tx: broadcast::Sender<RelayLifecycleEvent>,
    snapshot: Arc<std::sync::RwLock<RelayStateSnapshot>>,
}

impl RelayStateHub {
    fn new(listen: SocketAddr, target: SocketAddr, flow_idle_timeout: Duration) -> Self {
        let (event_tx, _) = broadcast::channel(1024);
        let snapshot = RelayStateSnapshot {
            listen,
            target,
            flow_idle_timeout_ms: flow_idle_timeout.as_millis() as u64,
            updated_at_ms: now_millis_u64(),
            selected_agent: None,
            tunnels: vec![],
            flows: vec![],
        };
        Self {
            event_tx,
            snapshot: Arc::new(std::sync::RwLock::new(snapshot)),
        }
    }

    fn emit(&self, event: RelayLifecycleEvent) {
        let _ = self.event_tx.send(event);
    }

    fn set_selected_agent(&self, agent: &AgentInfo, connected: bool) {
        self.with_snapshot_mut(|snapshot| {
            snapshot.selected_agent = Some(RelayAgentSnapshot {
                name: agent.name.clone(),
                instance_id: agent.instance_id.clone(),
                addr: agent.addr.clone(),
                expire_at: agent.expire_at,
                connected,
            });
            snapshot.updated_at_ms = now_millis_u64();
        });
    }

    fn set_agent_connected(&self, agent: &AgentInfo, connected: bool) {
        self.with_snapshot_mut(|snapshot| {
            let Some(selected) = snapshot.selected_agent.as_mut() else {
                return;
            };
            if selected.name != agent.name || selected.instance_id != agent.instance_id {
                return;
            }
            selected.connected = connected;
            selected.expire_at = agent.expire_at;
            selected.addr = agent.addr.clone();
            snapshot.updated_at_ms = now_millis_u64();
        });
    }

    fn clear_session_runtime(&self) {
        self.with_snapshot_mut(|snapshot| {
            snapshot.tunnels.clear();
            snapshot.flows.clear();
            snapshot.updated_at_ms = now_millis_u64();
        });
    }

    fn refresh_runtime(
        &self,
        target: SocketAddr,
        tunnels: &[Option<RelayTunnel>],
        tunnel_states: &[Option<RelayTunnelState>],
        tunnel_activity: &[Option<Instant>],
        src_to_flow: &HashMap<SocketAddr, ClientFlow>,
    ) {
        let now = Instant::now();
        let flow_loads = build_tunnel_flow_loads(tunnels.len(), src_to_flow);
        let mut tunnel_snaps = Vec::new();
        for tunnel_idx in 0..tunnels.len() {
            let alive = tunnels.get(tunnel_idx).and_then(|x| x.as_ref()).is_some();
            let state = tunnel_states.get(tunnel_idx).and_then(|x| x.as_ref());
            let activity = tunnel_activity.get(tunnel_idx).and_then(|x| *x);
            if !alive
                && !state.is_some_and(|x| {
                    x.down_until
                        .is_some_and(|until| until.saturating_duration_since(now).as_millis() > 0)
                })
            {
                continue;
            }

            let allocatable = state.is_some_and(|x| x.allocatable);
            let expires_in_ms = if alive {
                state.map(|x| x.expire_at.saturating_duration_since(now).as_millis() as u64)
            } else {
                state.and_then(|x| {
                    x.down_until
                        .map(|until| until.saturating_duration_since(now).as_millis() as u64)
                })
            };
            let last_active_ago_ms =
                activity.map(|x| now.saturating_duration_since(x).as_millis() as u64);
            let mode = tunnels
                .get(tunnel_idx)
                .and_then(|x| x.as_ref())
                .map(|x| x.codec.mode_name().to_string());
            let flow_count = flow_loads.get(tunnel_idx).copied().unwrap_or(0);
            let tunnel_id = state
                .map(|x| x.tunnel_id)
                .unwrap_or_else(|| (tunnel_idx as u64).saturating_add(1));

            tunnel_snaps.push(RelayTunnelSnapshot {
                tunnel_idx,
                tunnel_id,
                alive,
                allocatable,
                mode,
                flow_count,
                last_active_ago_ms,
                expires_in_ms,
            });
        }

        let mut flow_snaps = Vec::with_capacity(src_to_flow.len());
        for (src, flow) in src_to_flow.iter() {
            flow_snaps.push(RelayFlowSnapshot {
                flow_id: flow.flow_id,
                src: *src,
                target,
                tunnel_idx: flow.tunnel_idx,
                tunnel_id: tunnel_states
                    .get(flow.tunnel_idx)
                    .and_then(|x| x.as_ref())
                    .map(|x| x.tunnel_id),
                idle_for_ms: now.saturating_duration_since(flow.updated_at).as_millis() as u64,
                route_age_ms: now
                    .saturating_duration_since(flow.route_updated_at)
                    .as_millis() as u64,
            });
        }
        flow_snaps.sort_by_key(|x| x.flow_id);
        tunnel_snaps.sort_by_key(|x| x.tunnel_idx);

        self.with_snapshot_mut(|snapshot| {
            snapshot.target = target;
            snapshot.tunnels = tunnel_snaps;
            snapshot.flows = flow_snaps;
            snapshot.updated_at_ms = now_millis_u64();
        });
    }

    #[allow(dead_code)]
    fn snapshot(&self) -> RelayStateSnapshot {
        match self.snapshot.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    #[allow(dead_code)]
    fn subscribe(&self) -> broadcast::Receiver<RelayLifecycleEvent> {
        self.event_tx.subscribe()
    }

    fn with_snapshot_mut<F>(&self, f: F)
    where
        F: FnOnce(&mut RelayStateSnapshot),
    {
        match self.snapshot.write() {
            Ok(mut guard) => f(&mut guard),
            Err(poisoned) => {
                let mut guard = poisoned.into_inner();
                f(&mut guard);
            }
        }
    }
}

struct RelayTuiTerminalGuard;

impl RelayTuiTerminalGuard {
    fn enter() -> Result<Self> {
        if !io::stdout().is_terminal() {
            bail!("--tui requires stdout attached to terminal");
        }
        terminal::enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, Hide)?;
        Ok(Self)
    }
}

impl Drop for RelayTuiTerminalGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = execute!(
            stdout,
            Show,
            LeaveAlternateScreen,
            MoveTo(0, 0),
            Clear(ClearType::All)
        );
        let _ = terminal::disable_raw_mode();
    }
}

#[derive(Debug)]
struct RelayTuiAgentRow {
    rule_idx: usize,
    name: String,
    instance_id: String,
    addr: String,
    expire_at: u64,
    connected: bool,
}

#[derive(Debug)]
struct RelayTuiRuleRow {
    rule_idx: usize,
    listen: SocketAddr,
    target: SocketAddr,
}

#[derive(Debug)]
struct RelayTuiTunnelRow {
    rule_idx: usize,
    tunnel_label: String,
    mode: String,
    state: &'static str,
    flow_count: usize,
    last_active_ago_ms: Option<u64>,
    expires_in_ms: Option<u64>,
}

#[derive(Debug)]
struct RelayTuiFlowRow {
    rule_idx: usize,
    flow_id: u64,
    src: SocketAddr,
    target: SocketAddr,
    tunnel_label: String,
    expire_in_ms: u64,
    route_in_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelayTuiLineKind {
    Title,
    Summary,
    SectionRules,
    SectionAgents,
    SectionTunnels,
    SectionFlows,
    SectionEvents,
    Header,
    Normal,
    Good,
    Warn,
    Bad,
    Event,
}

#[derive(Debug, Clone)]
struct RelayTuiRenderedLine {
    text: String,
    kind: RelayTuiLineKind,
}

#[derive(Debug, Default, Clone, Copy)]
struct RelayTuiRowBudgets {
    rules: usize,
    agents: usize,
    tunnels: usize,
    flows: usize,
    events: usize,
}

fn compute_relay_tui_row_budgets(
    height: usize,
    rules_total: usize,
    agents_total: usize,
    tunnels_total: usize,
    flows_total: usize,
    events_total: usize,
) -> RelayTuiRowBudgets {
    // title + summary + non-event section/header lines + section gaps.
    let reserved = 13;
    let available = height.saturating_sub(reserved);

    let mut b = RelayTuiRowBudgets {
        rules: usize::from(rules_total > 0),
        agents: usize::from(agents_total > 0),
        tunnels: usize::from(tunnels_total > 0),
        flows: usize::from(flows_total > 0),
        events: 0,
    };

    let mut used = b.rules + b.agents + b.tunnels + b.flows;
    while used > available {
        if b.flows > 0 {
            b.flows -= 1;
            used -= 1;
            continue;
        }
        if b.tunnels > 0 {
            b.tunnels -= 1;
            used -= 1;
            continue;
        }
        if b.agents > 0 {
            b.agents -= 1;
            used -= 1;
            continue;
        }
        if b.rules > 0 {
            b.rules -= 1;
            used -= 1;
            continue;
        }
        return b;
    }

    let mut remaining = available.saturating_sub(used);
    while remaining > 0 {
        if b.flows < flows_total {
            b.flows += 1;
            remaining -= 1;
            continue;
        }
        if b.tunnels < tunnels_total {
            b.tunnels += 1;
            remaining -= 1;
            continue;
        }
        if b.agents < agents_total {
            b.agents += 1;
            remaining -= 1;
            continue;
        }
        if b.rules < rules_total {
            b.rules += 1;
            remaining -= 1;
            continue;
        }
        break;
    }

    // Events is lowest priority: only use leftover lines from other sections.
    // Event block needs 2 fixed lines (blank + section title), plus at least 1 row.
    if events_total > 0 && remaining > 2 {
        b.events = events_total.min(remaining - 2);
    }

    b
}

async fn run_relay_tui(state_hubs: Vec<RelayStateHub>) -> Result<()> {
    tokio::task::spawn_blocking(move || run_relay_tui_blocking(state_hubs))
        .await
        .with_context(|| "relay tui task join failed")?
}

fn run_relay_tui_blocking(state_hubs: Vec<RelayStateHub>) -> Result<()> {
    let _guard = RelayTuiTerminalGuard::enter()?;
    let mut stdout = io::stdout();
    let mut event_rxs = state_hubs.iter().map(|x| x.subscribe()).collect::<Vec<_>>();
    let mut recent_events = Vec::<String>::new();
    let mut needs_render = true;
    let mut last_render_at = Instant::now()
        .checked_sub(RELAY_TUI_REFRESH_INTERVAL)
        .unwrap_or_else(Instant::now);

    loop {
        if drain_relay_tui_events(event_rxs.as_mut_slice(), &mut recent_events) {
            needs_render = true;
        }

        if needs_render || last_render_at.elapsed() >= RELAY_TUI_REFRESH_INTERVAL {
            render_relay_tui(&mut stdout, state_hubs.as_slice(), recent_events.as_slice())?;
            needs_render = false;
            last_render_at = Instant::now();
        }

        let timeout = RELAY_TUI_REFRESH_INTERVAL.saturating_sub(last_render_at.elapsed());
        if event::poll(timeout)? {
            match event::read()? {
                CtEvent::Key(key) => {
                    if key.kind == KeyEventKind::Press {
                        match key.code {
                            KeyCode::Esc | KeyCode::Char('q') => break,
                            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                break;
                            }
                            KeyCode::Char('r') => needs_render = true,
                            _ => {}
                        }
                    }
                }
                CtEvent::Resize(..) => {
                    needs_render = true;
                }
                _ => {}
            }
        }
    }

    Ok(())
}

fn drain_relay_tui_events(
    event_rxs: &mut [broadcast::Receiver<RelayLifecycleEvent>],
    recent_events: &mut Vec<String>,
) -> bool {
    let mut changed = false;
    for rx in event_rxs.iter_mut() {
        loop {
            match rx.try_recv() {
                Ok(event) => {
                    push_relay_tui_event(recent_events, format_relay_lifecycle_event(event));
                    changed = true;
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(n)) => {
                    push_relay_tui_event(recent_events, format!("event lagged +{n}"));
                    changed = true;
                }
            }
        }
    }
    changed
}

fn push_relay_tui_event(recent_events: &mut Vec<String>, line: String) {
    recent_events.push(format!("{} {line}", Local::now().format("%H:%M:%S")));
    while recent_events.len() > RELAY_TUI_MAX_EVENT_BUFFER {
        recent_events.remove(0);
    }
}

fn format_relay_lifecycle_event(event: RelayLifecycleEvent) -> String {
    match event {
        RelayLifecycleEvent::AgentSelected {
            name,
            instance_id,
            addr,
            expire_at,
        } => {
            format!("agent selected: {name} inst={instance_id:?} addr={addr} expire_at={expire_at}")
        }
        RelayLifecycleEvent::AgentSwitched {
            old_name,
            old_instance_id,
            new_name,
            new_instance_id,
        } => {
            format!(
                "agent switched: {old_name} {old_instance_id:?} -> {new_name} {new_instance_id:?}"
            )
        }
        RelayLifecycleEvent::AgentSessionConnected { name, instance_id } => {
            format!("agent session connected: {name} inst={instance_id:?}")
        }
        RelayLifecycleEvent::AgentSessionClosed {
            name,
            instance_id,
            reason,
        } => {
            format!("agent session closed: {name} inst={instance_id:?} reason={reason}")
        }
        RelayLifecycleEvent::TunnelOpened {
            tunnel_idx,
            tunnel_id,
            mode,
            source,
        } => {
            format!("tunnel opened: idx={tunnel_idx} tid=T{tunnel_id} mode={mode} source={source}")
        }
        RelayLifecycleEvent::TunnelRotated {
            old_tunnel_idx,
            old_tunnel_id,
            new_tunnel_idx,
            new_tunnel_id,
        } => {
            format!(
                "tunnel rotated: idx={old_tunnel_idx}(T{old_tunnel_id}) -> idx={new_tunnel_idx}(T{new_tunnel_id})"
            )
        }
        RelayLifecycleEvent::TunnelClosed {
            tunnel_idx,
            tunnel_id,
            reason,
        } => {
            format!(
                "tunnel closed: idx={tunnel_idx} tid={} reason={reason}",
                tunnel_id
                    .map(|x| format!("T{x}"))
                    .unwrap_or_else(|| "-".to_string())
            )
        }
        RelayLifecycleEvent::FlowCreated {
            flow_id,
            src,
            target,
            tunnel_idx,
            tunnel_id,
        } => {
            format!(
                "flow created: id={flow_id} src={src} target={target} tunnel=idx={tunnel_idx} tid={}",
                tunnel_id
                    .map(|x| format!("T{x}"))
                    .unwrap_or_else(|| "-".to_string())
            )
        }
        RelayLifecycleEvent::FlowMigrated {
            flow_id,
            src,
            old_tunnel_idx,
            old_tunnel_id,
            new_tunnel_idx,
            new_tunnel_id,
            reason,
        } => {
            format!(
                "flow migrated: id={flow_id} src={src} tunnel=idx={old_tunnel_idx}/{} -> idx={new_tunnel_idx}/{} reason={reason}",
                old_tunnel_id
                    .map(|x| format!("T{x}"))
                    .unwrap_or_else(|| "-".to_string()),
                new_tunnel_id
                    .map(|x| format!("T{x}"))
                    .unwrap_or_else(|| "-".to_string()),
            )
        }
        RelayLifecycleEvent::FlowClosed {
            flow_id,
            src,
            tunnel_idx,
            tunnel_id,
            reason,
        } => {
            format!(
                "flow closed: id={flow_id} src={src} tunnel=idx={tunnel_idx} tid={} reason={reason}",
                tunnel_id
                    .map(|x| format!("T{x}"))
                    .unwrap_or_else(|| "-".to_string())
            )
        }
    }
}

fn render_relay_tui(
    stdout: &mut io::Stdout,
    state_hubs: &[RelayStateHub],
    recent_events: &[String],
) -> Result<()> {
    let (width, height) = terminal::size().unwrap_or((120, 40));
    let width = width as usize;
    let height = height as usize;
    let snapshots = state_hubs.iter().map(|x| x.snapshot()).collect::<Vec<_>>();
    let now_ms = now_millis_u64();

    let mut rule_rows = Vec::<RelayTuiRuleRow>::new();
    let mut agent_rows = Vec::<RelayTuiAgentRow>::new();
    let mut tunnel_rows = Vec::<RelayTuiTunnelRow>::new();
    let mut flow_rows = Vec::<RelayTuiFlowRow>::new();

    for (rule_idx, snapshot) in snapshots.iter().enumerate() {
        rule_rows.push(RelayTuiRuleRow {
            rule_idx,
            listen: snapshot.listen,
            target: snapshot.target,
        });

        if let Some(agent) = snapshot.selected_agent.as_ref() {
            agent_rows.push(RelayTuiAgentRow {
                rule_idx,
                name: agent.name.clone(),
                instance_id: agent.instance_id.clone().unwrap_or_else(|| "-".to_string()),
                addr: agent.addr.clone(),
                expire_at: agent.expire_at,
                connected: agent.connected,
            });
        }

        for tunnel in snapshot.tunnels.iter() {
            let state = if tunnel.alive && tunnel.allocatable {
                "active"
            } else if tunnel.alive {
                "drain"
            } else {
                "down"
            };
            tunnel_rows.push(RelayTuiTunnelRow {
                rule_idx,
                tunnel_label: format!("{}/T{}", tunnel.tunnel_idx, tunnel.tunnel_id),
                mode: tunnel.mode.clone().unwrap_or_else(|| "-".to_string()),
                state,
                flow_count: tunnel.flow_count,
                last_active_ago_ms: tunnel.last_active_ago_ms,
                expires_in_ms: tunnel.expires_in_ms,
            });
        }

        for flow in snapshot.flows.iter() {
            let route_window_ms = FLOW_ROUTE_RESELECT_INTERVAL.as_millis() as u64;
            let route_elapsed_in_window = if route_window_ms == 0 {
                0
            } else {
                flow.route_age_ms % route_window_ms
            };
            let route_in_ms = if route_elapsed_in_window == 0 {
                0
            } else {
                route_window_ms.saturating_sub(route_elapsed_in_window)
            };
            flow_rows.push(RelayTuiFlowRow {
                rule_idx,
                flow_id: flow.flow_id,
                src: flow.src,
                target: flow.target,
                tunnel_label: format!(
                    "{}/{}",
                    flow.tunnel_idx,
                    flow.tunnel_id
                        .map(|x| format!("T{x}"))
                        .unwrap_or_else(|| "-".to_string())
                ),
                expire_in_ms: snapshot
                    .flow_idle_timeout_ms
                    .saturating_sub(flow.idle_for_ms),
                route_in_ms,
            });
        }
    }

    agent_rows.sort_by(|a, b| {
        b.connected
            .cmp(&a.connected)
            .then_with(|| b.expire_at.cmp(&a.expire_at))
            .then_with(|| a.rule_idx.cmp(&b.rule_idx))
            .then_with(|| a.name.cmp(&b.name))
    });
    tunnel_rows.sort_by(|a, b| {
        b.flow_count
            .cmp(&a.flow_count)
            .then_with(|| a.rule_idx.cmp(&b.rule_idx))
            .then_with(|| a.tunnel_label.cmp(&b.tunnel_label))
    });
    flow_rows.sort_by(|a, b| {
        a.route_in_ms
            .cmp(&b.route_in_ms)
            .then_with(|| a.expire_in_ms.cmp(&b.expire_in_ms))
            .then_with(|| a.rule_idx.cmp(&b.rule_idx))
            .then_with(|| a.flow_id.cmp(&b.flow_id))
    });

    let budgets = compute_relay_tui_row_budgets(
        height,
        snapshots.len(),
        agent_rows.len(),
        tunnel_rows.len(),
        flow_rows.len(),
        recent_events.len(),
    );
    let compact_mode = width < 132 || height < 34;

    let mut lines = Vec::<RelayTuiRenderedLine>::new();
    relay_tui_push_line(
        &mut lines,
        format!(
            "rtun relay tui {}  q/esc:quit  r:refresh",
            Local::now().format("%H:%M:%S"),
        ),
        RelayTuiLineKind::Title,
    );
    relay_tui_push_line(
        &mut lines,
        format!(
            "screen:{}x{}  rules:{} agents:{} tunnels:{} flows:{} events:{}",
            width,
            height,
            snapshots.len(),
            agent_rows.len(),
            tunnel_rows.len(),
            flow_rows.len(),
            recent_events.len(),
        ),
        RelayTuiLineKind::Summary,
    );
    relay_tui_push_line(&mut lines, "[Rules]", RelayTuiLineKind::SectionRules);
    relay_tui_push_line(
        &mut lines,
        format_relay_tui_rule_header(width, compact_mode),
        RelayTuiLineKind::Header,
    );
    let rule_limit = budgets.rules.max(1);
    for row in rule_rows.iter().take(rule_limit) {
        relay_tui_push_line(
            &mut lines,
            format_relay_tui_rule_row(row, width, compact_mode),
            RelayTuiLineKind::Normal,
        );
    }
    if rule_rows.len() > rule_limit {
        relay_tui_push_line(
            &mut lines,
            format!("... {} more rules", rule_rows.len() - rule_limit),
            RelayTuiLineKind::Summary,
        );
    }

    relay_tui_push_line(&mut lines, "", RelayTuiLineKind::Summary);
    relay_tui_push_line(&mut lines, "[Agents]", RelayTuiLineKind::SectionAgents);
    relay_tui_push_line(
        &mut lines,
        format_relay_tui_agent_header(width, compact_mode),
        RelayTuiLineKind::Header,
    );
    if agent_rows.is_empty() {
        relay_tui_push_line(&mut lines, "-", RelayTuiLineKind::Summary);
    } else {
        for row in agent_rows.iter().take(budgets.agents.max(1)) {
            relay_tui_push_line(
                &mut lines,
                format_relay_tui_agent_row(row, now_ms, width, compact_mode),
                if row.connected {
                    RelayTuiLineKind::Good
                } else {
                    RelayTuiLineKind::Bad
                },
            );
        }
        if agent_rows.len() > budgets.agents.max(1) {
            relay_tui_push_line(
                &mut lines,
                format!(
                    "... {} more agents",
                    agent_rows.len() - budgets.agents.max(1)
                ),
                RelayTuiLineKind::Summary,
            );
        }
    }

    relay_tui_push_line(&mut lines, "", RelayTuiLineKind::Summary);
    relay_tui_push_line(&mut lines, "[Tunnels]", RelayTuiLineKind::SectionTunnels);
    relay_tui_push_line(
        &mut lines,
        format_relay_tui_tunnel_header(width, compact_mode),
        RelayTuiLineKind::Header,
    );
    if tunnel_rows.is_empty() {
        relay_tui_push_line(&mut lines, "-", RelayTuiLineKind::Summary);
    } else {
        for row in tunnel_rows.iter().take(budgets.tunnels.max(1)) {
            let row_kind = match row.state {
                "active" => RelayTuiLineKind::Good,
                "drain" => RelayTuiLineKind::Warn,
                _ => RelayTuiLineKind::Bad,
            };
            relay_tui_push_line(
                &mut lines,
                format_relay_tui_tunnel_row(row, width, compact_mode),
                row_kind,
            );
        }
        if tunnel_rows.len() > budgets.tunnels.max(1) {
            relay_tui_push_line(
                &mut lines,
                format!(
                    "... {} more tunnels",
                    tunnel_rows.len() - budgets.tunnels.max(1)
                ),
                RelayTuiLineKind::Summary,
            );
        }
    }

    relay_tui_push_line(&mut lines, "", RelayTuiLineKind::Summary);
    relay_tui_push_line(&mut lines, "[Flows]", RelayTuiLineKind::SectionFlows);
    relay_tui_push_line(
        &mut lines,
        format_relay_tui_flow_header(width, compact_mode),
        RelayTuiLineKind::Header,
    );
    if flow_rows.is_empty() {
        relay_tui_push_line(&mut lines, "-", RelayTuiLineKind::Summary);
    } else {
        for row in flow_rows.iter().take(budgets.flows.max(1)) {
            relay_tui_push_line(
                &mut lines,
                format_relay_tui_flow_row(row, width, compact_mode),
                RelayTuiLineKind::Normal,
            );
        }
        if flow_rows.len() > budgets.flows.max(1) {
            relay_tui_push_line(
                &mut lines,
                format!("... {} more flows", flow_rows.len() - budgets.flows.max(1)),
                RelayTuiLineKind::Summary,
            );
        }
    }

    if !recent_events.is_empty() && budgets.events > 0 {
        relay_tui_push_line(&mut lines, "", RelayTuiLineKind::Summary);
        relay_tui_push_line(
            &mut lines,
            "[Recent Events]",
            RelayTuiLineKind::SectionEvents,
        );
        let event_limit = budgets.events;
        let start = recent_events.len().saturating_sub(event_limit);
        for line in recent_events.iter().skip(start).rev() {
            relay_tui_push_line(
                &mut lines,
                line.clone(),
                relay_tui_event_line_kind(line.as_str()),
            );
        }
        if recent_events.len() > event_limit {
            relay_tui_push_line(
                &mut lines,
                format!("... {} older events", recent_events.len() - event_limit),
                RelayTuiLineKind::Summary,
            );
        }
    }

    let mut render_lines = lines;
    if height > 0 && render_lines.len() > height {
        render_lines.truncate(height.saturating_sub(1));
        relay_tui_push_line(&mut render_lines, "...", RelayTuiLineKind::Summary);
    }

    execute!(stdout, MoveTo(0, 0), Clear(ClearType::All))?;
    for line in render_lines.iter() {
        let clipped = clip_line_width(line.text.as_str(), width);
        if let Some(color) = relay_tui_color_of_line(line.kind) {
            execute!(
                stdout,
                SetForegroundColor(color),
                Print(clipped),
                ResetColor,
                Print("\r\n")
            )?;
        } else {
            write!(stdout, "{clipped}\r\n")?;
        }
    }
    stdout.flush()?;
    Ok(())
}

fn relay_tui_push_line(
    lines: &mut Vec<RelayTuiRenderedLine>,
    text: impl Into<String>,
    kind: RelayTuiLineKind,
) {
    lines.push(RelayTuiRenderedLine {
        text: text.into(),
        kind,
    });
}

fn relay_tui_color_of_line(kind: RelayTuiLineKind) -> Option<Color> {
    match kind {
        RelayTuiLineKind::Title => Some(Color::Cyan),
        RelayTuiLineKind::Summary => Some(Color::DarkGrey),
        RelayTuiLineKind::SectionRules => Some(Color::Blue),
        RelayTuiLineKind::SectionAgents => Some(Color::Green),
        RelayTuiLineKind::SectionTunnels => Some(Color::Magenta),
        RelayTuiLineKind::SectionFlows => Some(Color::Cyan),
        RelayTuiLineKind::SectionEvents => Some(Color::Yellow),
        RelayTuiLineKind::Header => Some(Color::DarkCyan),
        RelayTuiLineKind::Good => Some(Color::Green),
        RelayTuiLineKind::Warn => Some(Color::Yellow),
        RelayTuiLineKind::Bad => Some(Color::Red),
        RelayTuiLineKind::Event => Some(Color::Grey),
        RelayTuiLineKind::Normal => None,
    }
}

fn relay_tui_event_line_kind(line: &str) -> RelayTuiLineKind {
    let lower = line.to_ascii_lowercase();
    if lower.contains("failed")
        || lower.contains("closed")
        || lower.contains("timeout")
        || lower.contains("down")
        || lower.contains("lagged")
    {
        RelayTuiLineKind::Warn
    } else if lower.contains("connected")
        || lower.contains("created")
        || lower.contains("opened")
        || lower.contains("selected")
    {
        RelayTuiLineKind::Good
    } else {
        RelayTuiLineKind::Event
    }
}

fn format_relay_tui_rule_header(width: usize, compact: bool) -> String {
    if compact {
        return "rule listen target".to_string();
    }
    let target_w = width.saturating_sub(4 + 1 + 21 + 1);
    if target_w < 8 {
        return "rule listen target".to_string();
    }
    format!(
        "{} {} {}",
        relay_tui_cell("rule", 4),
        relay_tui_cell("listen", 21),
        relay_tui_cell("target", target_w),
    )
}

fn format_relay_tui_rule_row(row: &RelayTuiRuleRow, width: usize, compact: bool) -> String {
    if compact {
        return format!("R{} {} {}", row.rule_idx, row.listen, row.target);
    }
    let target_w = width.saturating_sub(4 + 1 + 21 + 1);
    if target_w < 8 {
        return format!("R{} {} {}", row.rule_idx, row.listen, row.target);
    }
    format!(
        "{} {} {}",
        relay_tui_cell(format!("R{}", row.rule_idx).as_str(), 4),
        relay_tui_cell(row.listen.to_string().as_str(), 21),
        relay_tui_cell(row.target.to_string().as_str(), target_w),
    )
}

fn format_relay_tui_agent_header(width: usize, compact: bool) -> String {
    let rule_w = 4;
    let name_w = 12;
    let inst_w = 14;
    let conn_w = 5;
    let expire_w = 12;
    if compact {
        return "rule name conn expire addr inst".to_string();
    }
    let addr_w =
        width.saturating_sub(rule_w + 1 + name_w + 1 + inst_w + 1 + conn_w + 1 + expire_w + 1);
    if addr_w < 12 {
        return "rule name conn expire addr inst".to_string();
    }
    format!(
        "{} {} {} {} {} {}",
        relay_tui_cell("rule", rule_w),
        relay_tui_cell("name", name_w),
        relay_tui_cell("instance", inst_w),
        relay_tui_cell("conn", conn_w),
        relay_tui_cell("expire", expire_w),
        relay_tui_cell("addr", addr_w),
    )
}

fn format_relay_tui_agent_row(
    row: &RelayTuiAgentRow,
    now_ms: u64,
    width: usize,
    compact: bool,
) -> String {
    let rule_w = 4;
    let name_w = 12;
    let inst_w = 14;
    let conn_w = 5;
    let expire_w = 12;
    let expire_human = format_millis(row.expire_at.saturating_sub(now_ms));
    if compact {
        return format!(
            "R{} {} {} {} {} {}",
            row.rule_idx,
            row.name,
            if row.connected { "up" } else { "down" },
            expire_human,
            row.addr,
            row.instance_id,
        );
    }

    let addr_w =
        width.saturating_sub(rule_w + 1 + name_w + 1 + inst_w + 1 + conn_w + 1 + expire_w + 1);
    if addr_w < 12 {
        return format!(
            "R{} {} {} {} {} {}",
            row.rule_idx,
            row.name,
            if row.connected { "up" } else { "down" },
            expire_human,
            row.addr,
            row.instance_id,
        );
    }

    format!(
        "{} {} {} {} {} {}",
        relay_tui_cell(format!("R{}", row.rule_idx).as_str(), rule_w),
        relay_tui_cell(&row.name, name_w),
        relay_tui_cell(&row.instance_id, inst_w),
        relay_tui_cell(if row.connected { "up" } else { "down" }, conn_w),
        relay_tui_cell(&expire_human, expire_w),
        relay_tui_cell(&row.addr, addr_w),
    )
}

fn format_relay_tui_tunnel_header(width: usize, compact: bool) -> String {
    if compact || width < 84 {
        return "rule idx/tid st mode flows active exp".to_string();
    }
    format!(
        "{} {} {} {} {} {} {}",
        relay_tui_cell("rule", 4),
        relay_tui_cell("idx/tid", 14),
        relay_tui_cell("state", 6),
        relay_tui_cell("mode", 8),
        relay_tui_cell("flows", 5),
        relay_tui_cell("active", 12),
        relay_tui_cell("expire", 12),
    )
}

fn format_relay_tui_tunnel_row(row: &RelayTuiTunnelRow, width: usize, compact: bool) -> String {
    let active = format_millis_ago(row.last_active_ago_ms);
    let expire = format_millis_in(row.expires_in_ms);
    if compact || width < 84 {
        return format!(
            "R{} {} {} {} {} {} {}",
            row.rule_idx, row.tunnel_label, row.state, row.mode, row.flow_count, active, expire
        );
    }
    format!(
        "{} {} {} {} {} {} {}",
        relay_tui_cell(format!("R{}", row.rule_idx).as_str(), 4),
        relay_tui_cell(&row.tunnel_label, 14),
        relay_tui_cell(row.state, 6),
        relay_tui_cell(&row.mode, 8),
        relay_tui_cell(format!("{}", row.flow_count).as_str(), 5),
        relay_tui_cell(&active, 12),
        relay_tui_cell(&expire, 12),
    )
}

fn format_relay_tui_flow_header(width: usize, compact: bool) -> String {
    if compact {
        return "rule flow idx/tid expire route src -> target".to_string();
    }
    let path_w = width.saturating_sub(4 + 1 + 11 + 1 + 14 + 1 + 10 + 1 + 10 + 1);
    if path_w < 18 {
        return "rule flow idx/tid expire route src -> target".to_string();
    }
    format!(
        "{} {} {} {} {} {}",
        relay_tui_cell("rule", 4),
        relay_tui_cell("flow_id", 11),
        relay_tui_cell("idx/tid", 14),
        relay_tui_cell("expire", 10),
        relay_tui_cell("route", 10),
        relay_tui_cell("src -> target", path_w),
    )
}

fn format_relay_tui_flow_row(row: &RelayTuiFlowRow, width: usize, compact: bool) -> String {
    let expire = format_millis(row.expire_in_ms);
    let route = format_millis(row.route_in_ms);
    let path = format!("{} -> {}", row.src, row.target);
    if compact {
        return format!(
            "R{} {} {} {} {} {}",
            row.rule_idx, row.flow_id, row.tunnel_label, expire, route, path
        );
    }
    let path_w = width.saturating_sub(4 + 1 + 11 + 1 + 14 + 1 + 10 + 1 + 10 + 1);
    if path_w < 18 {
        return format!(
            "R{} {} {} {} {} {}",
            row.rule_idx, row.flow_id, row.tunnel_label, expire, route, path
        );
    }
    format!(
        "{} {} {} {} {} {}",
        relay_tui_cell(format!("R{}", row.rule_idx).as_str(), 4),
        relay_tui_cell(format!("{}", row.flow_id).as_str(), 11),
        relay_tui_cell(&row.tunnel_label, 14),
        relay_tui_cell(&expire, 10),
        relay_tui_cell(&route, 10),
        relay_tui_cell(&path, path_w),
    )
}

fn relay_tui_cell(v: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let clipped = relay_tui_truncate(v, width);
    let len = clipped.chars().count();
    if len >= width {
        return clipped;
    }

    let mut out = String::with_capacity(width);
    out.push_str(&clipped);
    out.push_str(&" ".repeat(width - len));
    out
}

fn relay_tui_truncate(v: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let char_count = v.chars().count();
    if char_count <= width {
        return v.to_string();
    }
    if width == 1 {
        return "…".to_string();
    }
    let mut out = String::with_capacity(width);
    for (idx, ch) in v.chars().enumerate() {
        if idx + 1 >= width {
            break;
        }
        out.push(ch);
    }
    out.push('…');
    out
}

fn clip_line_width(line: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }

    let max_chars = width.saturating_sub(1);
    let mut clipped = String::with_capacity(line.len().min(width));
    for (idx, ch) in line.chars().enumerate() {
        if idx >= max_chars {
            clipped.push('…');
            return clipped;
        }
        clipped.push(ch);
    }
    clipped
}

fn format_millis_ago(v: Option<u64>) -> String {
    v.map(format_millis)
        .map(|x| format!("{x} ago"))
        .unwrap_or_else(|| "-".to_string())
}

fn format_millis_in(v: Option<u64>) -> String {
    v.map(format_millis)
        .map(|x| format!("in {x}"))
        .unwrap_or_else(|| "-".to_string())
}

fn format_millis(ms: u64) -> String {
    if ms < 1_000 {
        return "<1s".to_string();
    }

    let mut seconds = ms / 1_000;
    let days = seconds / 86_400;
    if days > 0 {
        return ">=24h".to_string();
    }
    let hours = seconds / 3_600;
    seconds %= 3_600;
    let minutes = seconds / 60;
    seconds %= 60;

    if hours > 0 {
        format!("{hours}h{minutes}m{seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m{seconds}s")
    } else {
        format!("{seconds}s")
    }
}

async fn do_run(args: CmdArgs) -> Result<()> {
    let tui_enabled = args.tui;
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
    let p2p_channel_lifetime_secs = if args.p2p_channel_lifetime == 0 {
        DEFAULT_P2P_CHANNEL_LIFETIME_SECS
    } else {
        args.p2p_channel_lifetime
    };
    let p2p_channel_lifetime = Duration::from_secs(p2p_channel_lifetime_secs);
    let max_payload = resolve_udp_max_payload(args.udp_max_payload)?;
    let channel_pool = ChannelPoolConfig::new(args.p2p_min_channels, args.p2p_max_channels)?;
    let agent_script_timeout_secs = if args.agent_script_timeout == 0 {
        DEFAULT_AGENT_SCRIPT_TIMEOUT_SECS
    } else {
        args.agent_script_timeout
    };
    let agent_script_timeout = Duration::from_secs(agent_script_timeout_secs);
    let agent_scripts = Arc::new(load_agent_scripts(&args)?);
    if !agent_scripts.is_empty() {
        tracing::info!(
            "relay agent scripts enabled: count={}, timeout={}s, fail_policy={:?}",
            agent_scripts.len(),
            agent_script_timeout_secs,
            args.agent_script_fail_policy
        );
    }

    let mut rules = Vec::with_capacity(args.local_rules.len());
    for rule in args.local_rules {
        let rule = RelayRule::parse(rule.as_str())?;
        if !rule.is_udp() {
            bail!("only udp relay is supported");
        }
        rules.push(rule);
    }

    tracing::info!(
        "relay defaults: udp_idle_timeout={}s, udp_max_payload={} bytes, p2p_channels={}/{}, p2p_channel_lifetime={}s",
        idle_timeout_secs,
        max_payload,
        channel_pool.min_channels,
        channel_pool.max_channels,
        p2p_channel_lifetime_secs
    );

    let mut state_hubs = Vec::with_capacity(rules.len());
    for (idx, rule) in rules.into_iter().enumerate() {
        let state_hub = RelayStateHub::new(rule.listen, rule.target, idle_timeout);
        state_hubs.push(state_hub.clone());
        let worker = RelayWorker {
            signal_url: signal_url.clone(),
            secret: args.secret.clone(),
            quic_insecure: args.quic_insecure,
            agent_regex: agent_regex.clone(),
            idle_timeout,
            max_payload,
            channel_pool,
            p2p_channel_lifetime,
            agent_scripts: agent_scripts.clone(),
            agent_script_timeout,
            agent_script_fail_policy: args.agent_script_fail_policy,
            rule,
            state_hub,
        };

        let task_name = format!("relay-udp-{idx}");
        spawn_with_name(task_name, async move {
            let r = run_worker(worker).await;
            tracing::warn!("relay worker exited [{r:?}]");
            r
        });
    }

    if tui_enabled {
        return run_relay_tui(state_hubs).await;
    }

    futures::future::pending::<()>().await;
    #[allow(unreachable_code)]
    Ok(())
}

async fn run_worker(worker: RelayWorker) -> Result<()> {
    let desired_channels = worker.channel_pool.desired_channels(0);
    tracing::debug!(
        "relay channel pool bootstrap: active_flows=0, desired_channels={desired_channels}"
    );

    let local = Arc::new(
        UdpSocket::bind(worker.rule.listen)
            .await
            .with_context(|| format!("bind local udp failed [{}]", worker.rule.listen))?,
    );

    tracing::info!(
        "relay(udp) listen on [{}] -> [{}]",
        worker.rule.listen,
        worker.rule.target
    );

    let mut next_agent_hint: Option<AgentInfo> = None;
    let mut blocked_agents: HashSet<String> = HashSet::new();
    loop {
        let selected = match next_agent_hint.take() {
            Some(agent) if !blocked_agents.contains(&agent_instance_key(&agent)) => Ok(agent),
            None => {
                select_latest_agent(
                    &worker.signal_url,
                    &worker.agent_regex,
                    worker.quic_insecure,
                    &blocked_agents,
                )
                .await
            }
            _ => {
                select_latest_agent(
                    &worker.signal_url,
                    &worker.agent_regex,
                    worker.quic_insecure,
                    &blocked_agents,
                )
                .await
            }
        };

        let selected = match selected {
            Ok(v) => v,
            Err(e) => {
                if !blocked_agents.is_empty() {
                    tracing::warn!(
                        "select agent failed with blocked set, clear once and retry [{e}]"
                    );
                    blocked_agents.clear();
                } else {
                    tracing::debug!("select agent failed [{e}]");
                }
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
        worker.state_hub.set_selected_agent(&selected, false);
        worker.state_hub.emit(RelayLifecycleEvent::AgentSelected {
            name: selected.name.clone(),
            instance_id: selected.instance_id.clone(),
            addr: selected.addr.clone(),
            expire_at: selected.expire_at,
        });

        let (stop_tx, mut session_task) =
            spawn_relay_session_task(worker.clone(), local.clone(), selected.clone());
        let expire_wait = duration_until_replacement_window(selected.expire_at, AGENT_REPLACE_AHEAD);

        tokio::select! {
            r = &mut session_task => {
                if let Some(reason) = script_switch_reason(&r) {
                    let key = agent_instance_key(&selected);
                    tracing::warn!(
                        "mark agent unavailable for this process due to script failure: agent [{}], instance [{:?}], key [{}], reason [{}]",
                        selected.name,
                        selected.instance_id,
                        key,
                        reason
                    );
                    blocked_agents.insert(key);
                }
                handle_session_task_result(&worker.state_hub, &selected, r);
            }
            _ = time::sleep(expire_wait) => {
                tracing::info!(
                    "current agent entered replacement window (ahead={}s), preparing replacement: agent [{}], instance [{:?}]",
                    AGENT_REPLACE_AHEAD.as_secs(),
                    selected.name,
                    selected.instance_id
                );

                match wait_replacement_agent_ready(&worker, &selected, &session_task, &blocked_agents).await {
                    Some(next_agent) => {
                        tracing::info!(
                            "replacement agent ready, switching: old [{}]-[{:?}] -> new [{}]-[{:?}]",
                            selected.name,
                            selected.instance_id,
                            next_agent.name,
                            next_agent.instance_id
                        );
                        worker.state_hub.emit(RelayLifecycleEvent::AgentSwitched {
                            old_name: selected.name.clone(),
                            old_instance_id: selected.instance_id.clone(),
                            new_name: next_agent.name.clone(),
                            new_instance_id: next_agent.instance_id.clone(),
                        });
                        let _ = stop_tx.send(());
                        let r = session_task.await;
                        if let Some(reason) = script_switch_reason(&r) {
                            let key = agent_instance_key(&selected);
                            tracing::warn!(
                                "mark agent unavailable for this process due to script failure: agent [{}], instance [{:?}], key [{}], reason [{}]",
                                selected.name,
                                selected.instance_id,
                                key,
                                reason
                            );
                            blocked_agents.insert(key);
                        }
                        handle_session_task_result(&worker.state_hub, &selected, r);
                        next_agent_hint = Some(next_agent);
                    }
                    None => {
                        let r = session_task.await;
                        if let Some(reason) = script_switch_reason(&r) {
                            let key = agent_instance_key(&selected);
                            tracing::warn!(
                                "mark agent unavailable for this process due to script failure: agent [{}], instance [{:?}], key [{}], reason [{}]",
                                selected.name,
                                selected.instance_id,
                                key,
                                reason
                            );
                            blocked_agents.insert(key);
                        }
                        handle_session_task_result(&worker.state_hub, &selected, r);
                    }
                }
            }
        };

        time::sleep(LOOP_RETRY_INTERVAL).await;
    }
}

fn spawn_relay_session_task(
    worker: RelayWorker,
    local: Arc<UdpSocket>,
    selected: AgentInfo,
) -> (oneshot::Sender<()>, JoinHandle<Result<()>>) {
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let task_name = format!("relay-session-{}", selected.name);
    let handle = spawn_with_name(task_name, async move {
        if worker.signal_url.scheme().eq_ignore_ascii_case("quic") {
            run_with_quic_signal(&worker, local.as_ref(), &selected, stop_rx).await
        } else {
            run_with_ws_signal(&worker, local.as_ref(), &selected, stop_rx).await
        }
    });
    (stop_tx, handle)
}

async fn wait_replacement_agent_ready(
    worker: &RelayWorker,
    current: &AgentInfo,
    session_task: &JoinHandle<Result<()>>,
    blocked_agents: &HashSet<String>,
) -> Option<AgentInfo> {
    loop {
        if session_task.is_finished() {
            tracing::warn!("current session closed while waiting replacement");
            return None;
        }

        match find_connectable_replacement_agent(worker, current, blocked_agents).await {
            Ok(Some(agent)) => return Some(agent),
            Ok(None) => {
                tracing::debug!("no replacement agent ready yet");
            }
            Err(e) => {
                tracing::debug!("find replacement agent failed [{e}]");
            }
        }

        time::sleep(LOOP_RETRY_INTERVAL).await;
    }
}

async fn find_connectable_replacement_agent(
    worker: &RelayWorker,
    current: &AgentInfo,
    blocked_agents: &HashSet<String>,
) -> Result<Option<AgentInfo>> {
    let mut candidates = query_candidate_agents(
        &worker.signal_url,
        &worker.agent_regex,
        worker.quic_insecure,
    )
    .await?;

    for candidate in candidates.drain(..) {
        if is_same_agent_instance(&candidate, current) {
            continue;
        }
        if blocked_agents.contains(&agent_instance_key(&candidate)) {
            continue;
        }

        match probe_agent_connection(worker, &candidate).await {
            Ok(()) => return Ok(Some(candidate)),
            Err(e) => {
                tracing::debug!(
                    "replacement probe failed: agent [{}], instance [{:?}], err [{}]",
                    candidate.name,
                    candidate.instance_id,
                    e
                );
            }
        }
    }

    Ok(None)
}

async fn probe_agent_connection(worker: &RelayWorker, selected: &AgentInfo) -> Result<()> {
    if worker.signal_url.scheme().eq_ignore_ascii_case("quic") {
        let sub_url = make_quic_sub_url(&worker.signal_url, selected, worker.secret.as_deref())?;
        let _stream = quic_signal::connect_sub_with_opts(&sub_url, worker.quic_insecure)
            .await
            .with_context(|| format!("connect to replacement agent failed [{}]", sub_url))?;
    } else {
        let sub_url = make_ws_sub_url(&worker.signal_url, selected, worker.secret.as_deref())?;
        let (_stream, _rsp) = ws_connect_to(sub_url.as_str())
            .await
            .with_context(|| format!("connect to replacement agent failed [{}]", sub_url))?;
    }
    Ok(())
}

fn is_same_agent_instance(a: &AgentInfo, b: &AgentInfo) -> bool {
    if a.name != b.name {
        return false;
    }

    match (a.instance_id.as_deref(), b.instance_id.as_deref()) {
        (Some(x), Some(y)) => x == y,
        _ => a.addr == b.addr,
    }
}

fn agent_instance_key(agent: &AgentInfo) -> String {
    match agent.instance_id.as_deref() {
        Some(instance_id) => format!("{}#{}", agent.name, instance_id),
        None => format!("{}@{}", agent.name, agent.addr),
    }
}

fn script_switch_reason(r: &Result<Result<()>, tokio::task::JoinError>) -> Option<String> {
    match r {
        Ok(Err(e)) => e
            .downcast_ref::<AgentScriptSwitchAgentError>()
            .map(|x| x.reason.clone()),
        _ => None,
    }
}

fn duration_until_replacement_window(expire_at: u64, replace_ahead: Duration) -> Duration {
    duration_until_replacement_window_at(expire_at, replace_ahead, now_millis_u64())
}

fn duration_until_replacement_window_at(
    expire_at: u64,
    replace_ahead: Duration,
    now_ms: u64,
) -> Duration {
    let replace_at_ms = expire_at.saturating_sub(replace_ahead.as_millis() as u64);
    Duration::from_millis(replace_at_ms.saturating_sub(now_ms))
}

fn handle_session_task_result(
    state_hub: &RelayStateHub,
    selected: &AgentInfo,
    r: Result<Result<()>, tokio::task::JoinError>,
) {
    let reason = match &r {
        Ok(Ok(())) => "closed".to_string(),
        Ok(Err(e)) => format!("failed [{e}]"),
        Err(e) => format!("panicked [{e}]"),
    };
    match r {
        Ok(Ok(())) => {
            tracing::warn!("relay session closed");
        }
        Ok(Err(e)) => {
            tracing::warn!("relay session failed [{e}]");
        }
        Err(e) => {
            tracing::warn!("relay session task panicked [{e}]");
        }
    }
    state_hub.set_agent_connected(selected, false);
    state_hub.clear_session_runtime();
    state_hub.emit(RelayLifecycleEvent::AgentSessionClosed {
        name: selected.name.clone(),
        instance_id: selected.instance_id.clone(),
        reason,
    });
}

async fn run_with_ws_signal(
    worker: &RelayWorker,
    local: &UdpSocket,
    selected: &AgentInfo,
    mut stop_rx: oneshot::Receiver<()>,
) -> Result<()> {
    let sub_url = make_ws_sub_url(&worker.signal_url, selected, worker.secret.as_deref())?;
    let (stream, _rsp) = ws_connect_to(sub_url.as_str())
        .await
        .with_context(|| format!("connect to agent failed [{}]", sub_url))?;
    let mut session = make_stream_session(stream.split(), false).await?;
    tracing::info!("relay session connected, agent [{}]", selected.name);
    worker.state_hub.set_agent_connected(selected, true);
    worker
        .state_hub
        .emit(RelayLifecycleEvent::AgentSessionConnected {
            name: selected.name.clone(),
            instance_id: selected.instance_id.clone(),
        });

    let ctrl = session.ctrl_client().clone_invoker();
    run_agent_scripts(
        &ctrl,
        selected,
        worker.agent_scripts.as_slice(),
        worker.agent_script_timeout,
        worker.agent_script_fail_policy,
    )
    .await?;
    tokio::select! {
        r = run_relay_session(
            ctrl,
            local,
            selected,
            &worker.state_hub,
            worker.rule.target,
            worker.idle_timeout,
            worker.max_payload,
            worker.channel_pool,
            worker.p2p_channel_lifetime,
        ) => r,
        r = session.wait_for_completed() => {
            r?;
            bail!("signal session closed")
        }
        _ = &mut stop_rx => {
            tracing::info!(
                "relay session stop requested: agent [{}], instance [{:?}]",
                selected.name,
                selected.instance_id
            );
            Ok(())
        }
    }
}

async fn run_with_quic_signal(
    worker: &RelayWorker,
    local: &UdpSocket,
    selected: &AgentInfo,
    mut stop_rx: oneshot::Receiver<()>,
) -> Result<()> {
    let sub_url = make_quic_sub_url(&worker.signal_url, selected, worker.secret.as_deref())?;
    let stream = quic_signal::connect_sub_with_opts(&sub_url, worker.quic_insecure)
        .await
        .with_context(|| format!("connect to agent failed [{}]", sub_url))?;
    let mut session = make_stream_session(stream.split(), false).await?;
    tracing::info!("relay session connected, agent [{}]", selected.name);
    worker.state_hub.set_agent_connected(selected, true);
    worker
        .state_hub
        .emit(RelayLifecycleEvent::AgentSessionConnected {
            name: selected.name.clone(),
            instance_id: selected.instance_id.clone(),
        });

    let ctrl = session.ctrl_client().clone_invoker();
    run_agent_scripts(
        &ctrl,
        selected,
        worker.agent_scripts.as_slice(),
        worker.agent_script_timeout,
        worker.agent_script_fail_policy,
    )
    .await?;
    tokio::select! {
        r = run_relay_session(
            ctrl,
            local,
            selected,
            &worker.state_hub,
            worker.rule.target,
            worker.idle_timeout,
            worker.max_payload,
            worker.channel_pool,
            worker.p2p_channel_lifetime,
        ) => r,
        r = session.wait_for_completed() => {
            r?;
            bail!("signal session closed")
        }
        _ = &mut stop_rx => {
            tracing::info!(
                "relay session stop requested: agent [{}], instance [{:?}]",
                selected.name,
                selected.instance_id
            );
            Ok(())
        }
    }
}

async fn run_agent_scripts<H: CtrlHandler>(
    ctrl: &CtrlInvoker<H>,
    selected: &AgentInfo,
    scripts: &[RelayAgentScriptSpec],
    timeout: Duration,
    fail_policy: AgentScriptFailPolicy,
) -> Result<()> {
    if scripts.is_empty() {
        return Ok(());
    }

    let timeout_secs = timeout.as_secs().min(u32::MAX as u64) as u32;
    for (idx, script) in scripts.iter().enumerate() {
        let display_name = script.display_name();
        tracing::info!(
            "running agent script [{}/{}]: [{}], agent [{}], instance [{:?}]",
            idx + 1,
            scripts.len(),
            display_name,
            selected.name,
            selected.instance_id
        );

        let rsp = ctrl
            .exec_agent_script(ExecAgentScriptArgs {
                name: script.request_name().into(),
                content: script.content.to_vec().into(),
                argv: script
                    .argv
                    .iter()
                    .map(|x| protobuf::Chars::from(x.as_str()))
                    .collect(),
                timeout_secs,
                max_stdout_bytes: AGENT_SCRIPT_STDOUT_LIMIT,
                max_stderr_bytes: AGENT_SCRIPT_STDERR_LIMIT,
                ..Default::default()
            })
            .await
            .with_context(|| format!("execute agent script request failed [{display_name}]"))?;

        let summary = format_exec_agent_script_summary(&rsp);
        let stdout = String::from_utf8_lossy(rsp.stdout.as_ref());
        let stderr = String::from_utf8_lossy(rsp.stderr.as_ref());
        if !stdout.is_empty() {
            tracing::info!("agent script stdout [{}]:\n{}", display_name, stdout);
        }
        if !stderr.is_empty() {
            tracing::warn!("agent script stderr [{}]:\n{}", display_name, stderr);
        }

        if is_exec_agent_script_ok(&rsp) {
            tracing::info!("agent script succeeded [{}]: {}", display_name, summary);
            continue;
        }

        let reason = format!(
            "script [{}] failed on agent [{}]-[{:?}]: {}",
            display_name, selected.name, selected.instance_id, summary
        );
        match fail_policy {
            AgentScriptFailPolicy::Ignore => {
                tracing::warn!("{reason}, ignore by fail-policy");
            }
            AgentScriptFailPolicy::SwitchAgent => {
                return Err(AgentScriptSwitchAgentError { reason }.into());
            }
        }
    }
    Ok(())
}

fn is_exec_agent_script_ok(rsp: &ExecAgentScriptResult) -> bool {
    rsp.status_code == 0 && !rsp.timed_out && rsp.exit_code == 0
}

fn format_exec_agent_script_summary(rsp: &ExecAgentScriptResult) -> String {
    format!(
        "status_code={}, exit_code={}, timed_out={}, stdout={}B{}, stderr={}B{}, reason={}",
        rsp.status_code,
        rsp.exit_code,
        rsp.timed_out,
        rsp.stdout.len(),
        if rsp.stdout_truncated {
            "(truncated)"
        } else {
            ""
        },
        rsp.stderr.len(),
        if rsp.stderr_truncated {
            "(truncated)"
        } else {
            ""
        },
        rsp.reason
    )
}

async fn run_relay_session<H: CtrlHandler>(
    ctrl: CtrlInvoker<H>,
    local: &UdpSocket,
    selected: &AgentInfo,
    state_hub: &RelayStateHub,
    target: SocketAddr,
    idle_timeout: Duration,
    max_payload: usize,
    channel_pool: ChannelPoolConfig,
    p2p_channel_lifetime: Duration,
) -> Result<()> {
    let idle_timeout_secs = u32::try_from(idle_timeout.as_secs())
        .with_context(|| format!("udp idle timeout too large [{}s]", idle_timeout.as_secs()))?;
    let desired = channel_pool.desired_channels(0);
    let mut tunnels: Vec<Option<RelayTunnel>> = Vec::with_capacity(desired);
    let mut recv_tasks: Vec<Option<JoinHandle<()>>> = Vec::with_capacity(desired);
    let mut tunnel_states: Vec<Option<RelayTunnelState>> = Vec::with_capacity(desired);
    let mut tunnel_activity: Vec<Option<Instant>> = Vec::with_capacity(desired);
    let mut next_tunnel_id = 1_u64;
    let (inbound_tx, inbound_rx) = mpsc::channel::<TunnelRecvEvent>(1024);
    for _ in 0..desired {
        let tunnel_idx = tunnels.len();
        let tunnel = open_udp_relay_tunnel(&ctrl, target, idle_timeout_secs, max_payload).await?;
        tracing::info!(
            "relay tunnel connected: idx [{}], codec [{}]",
            tunnel_idx,
            tunnel.codec.mode_name()
        );
        let recv_task = spawn_tunnel_recv_task(
            tunnel_idx,
            tunnel.socket.clone(),
            tunnel.codec,
            max_payload,
            inbound_tx.clone(),
        );
        recv_tasks.push(Some(recv_task));
        tunnels.push(Some(tunnel));
        tunnel_states.push(Some(RelayTunnelState::new(
            Instant::now(),
            p2p_channel_lifetime,
            next_tunnel_id,
        )));
        next_tunnel_id = next_tunnel_id.saturating_add(1);
        tunnel_activity.push(Some(Instant::now()));
        let tunnel_id = tunnel_states[tunnel_idx]
            .as_ref()
            .map(|x| x.tunnel_id)
            .unwrap_or(0);
        state_hub.emit(RelayLifecycleEvent::TunnelOpened {
            tunnel_idx,
            tunnel_id,
            mode: tunnels[tunnel_idx]
                .as_ref()
                .map(|x| x.codec.mode_name().to_string())
                .unwrap_or_else(|| "unknown".to_string()),
            source: "bootstrap",
        });
    }
    state_hub.refresh_runtime(
        target,
        &tunnels,
        &tunnel_states,
        &tunnel_activity,
        &HashMap::new(),
    );

    relay_loop(
        ctrl,
        local,
        selected,
        state_hub,
        target,
        idle_timeout,
        max_payload,
        channel_pool,
        tunnels,
        recv_tasks,
        tunnel_states,
        tunnel_activity,
        p2p_channel_lifetime,
        next_tunnel_id,
        inbound_tx,
        inbound_rx,
    )
    .await
}

async fn open_udp_relay_tunnel<H: CtrlHandler>(
    ctrl: &CtrlInvoker<H>,
    target: SocketAddr,
    idle_timeout_secs: u32,
    max_payload: usize,
) -> Result<RelayTunnel> {
    let mut peer = IcePeer::with_config(IceConfig {
        servers: default_ice_servers(),
        ..Default::default()
    });

    let local_ice = peer.client_gather().await?;
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
    Ok(RelayTunnel {
        socket: Arc::new(socket),
        codec,
    })
}

fn spawn_tunnel_recv_task(
    tunnel_idx: usize,
    socket: Arc<UdpSocket>,
    codec: UdpRelayCodec,
    max_payload: usize,
    inbound_tx: mpsc::Sender<TunnelRecvEvent>,
) -> JoinHandle<()> {
    let task_name = format!("relay-tunnel-rx-{tunnel_idx}");
    spawn_with_name(task_name, async move {
        let mut tun_buf = vec![0_u8; 64 * 1024];
        let mut timeout_check = time::interval(Duration::from_secs(1));
        timeout_check.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut last_valid_recv_at = Instant::now();
        loop {
            tokio::select! {
                _ = timeout_check.tick() => {
                    let idle = Instant::now().saturating_duration_since(last_valid_recv_at);
                    if idle >= UDP_RELAY_HEARTBEAT_TIMEOUT {
                        let _ = inbound_tx
                            .send(TunnelRecvEvent::Closed {
                                tunnel_idx,
                                reason: format!(
                                    "heartbeat timeout: no valid packet for {}ms (timeout={}ms)",
                                    idle.as_millis(),
                                    UDP_RELAY_HEARTBEAT_TIMEOUT.as_millis()
                                ),
                            })
                            .await;
                        break;
                    }
                }
                r = socket.recv(&mut tun_buf) => {
                    let n = match r {
                        Ok(n) => n,
                        Err(e) => {
                            let _ = inbound_tx
                                .send(TunnelRecvEvent::Closed {
                                    tunnel_idx,
                                    reason: e.to_string(),
                                })
                                .await;
                            break;
                        }
                    };
                    if n == 0 {
                        let _ = inbound_tx
                            .send(TunnelRecvEvent::Closed {
                                tunnel_idx,
                                reason: "recv 0".to_string(),
                            })
                            .await;
                        break;
                    }

                    let (flow_id, payload) = match decode_udp_relay_packet(&tun_buf[..n], codec) {
                        Ok(v) => v,
                        Err(e) => {
                            let packet = &tun_buf[..n];
                            if packet_has_stun_magic(packet) {
                                tracing::debug!(
                                    "relay tunnel decode dropped stun-like packet: tunnel [{}], codec [{}], header [{}], bytes [{}], packet {}, err [{}]",
                                    tunnel_idx,
                                    codec.mode_name(),
                                    codec.header_len(),
                                    n,
                                    packet.dump_bin_limit(24),
                                    e
                                );
                            } else {
                                tracing::warn!(
                                    "relay tunnel decode failed: tunnel [{}], codec [{}], header [{}], bytes [{}], packet {}, err [{}]",
                                    tunnel_idx,
                                    codec.mode_name(),
                                    codec.header_len(),
                                    n,
                                    packet.dump_bin_limit(24),
                                    e
                                );
                            }
                            continue;
                        }
                    };
                    last_valid_recv_at = Instant::now();

                    if flow_id == UDP_RELAY_HEARTBEAT_FLOW_ID && payload.is_empty() {
                        if inbound_tx
                            .send(TunnelRecvEvent::Heartbeat { tunnel_idx })
                            .await
                            .is_err()
                        {
                            break;
                        }
                        continue;
                    }
                    if payload.len() > max_payload {
                        tracing::warn!(
                            "drop oversized tunnel udp packet: tunnel [{}], flow [{}], size [{}], max [{}]",
                            tunnel_idx,
                            flow_id,
                            payload.len(),
                            max_payload
                        );
                        continue;
                    }

                    if inbound_tx
                        .send(TunnelRecvEvent::Packet(TunnelInboundPacket {
                            tunnel_idx,
                            flow_id,
                            payload: payload.to_vec(),
                        }))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    })
}

async fn try_expand_tunnels<H: CtrlHandler>(
    ctrl: &CtrlInvoker<H>,
    state_hub: &RelayStateHub,
    target: SocketAddr,
    idle_timeout_secs: u32,
    max_payload: usize,
    p2p_channel_lifetime: Duration,
    desired: usize,
    tunnels: &mut Vec<Option<RelayTunnel>>,
    recv_tasks: &mut Vec<Option<JoinHandle<()>>>,
    tunnel_states: &mut Vec<Option<RelayTunnelState>>,
    tunnel_activity: &mut Vec<Option<Instant>>,
    next_tunnel_id: &mut u64,
    inbound_tx: &mpsc::Sender<TunnelRecvEvent>,
    connect_timeout: &mut Duration,
) {
    while active_tunnel_count(tunnels) < desired {
        let tunnel_idx = match first_inactive_tunnel_idx(tunnels) {
            Some(idx) => idx,
            None => tunnels.len(),
        };
        let timeout = *connect_timeout;
        match time::timeout(
            timeout,
            open_udp_relay_tunnel(ctrl, target, idle_timeout_secs, max_payload),
        )
        .await
        {
            Ok(Ok(tunnel)) => {
                *connect_timeout = P2P_EXPAND_CONNECT_TIMEOUT_INITIAL;
                let mode = tunnel.codec.mode_name().to_string();
                tracing::info!(
                    "relay tunnel connected(scale-up): idx [{}], codec [{}], active={}",
                    tunnel_idx,
                    mode,
                    active_tunnel_count(tunnels) + 1
                );
                let recv_task = spawn_tunnel_recv_task(
                    tunnel_idx,
                    tunnel.socket.clone(),
                    tunnel.codec,
                    max_payload,
                    inbound_tx.clone(),
                );
                if tunnel_idx == tunnels.len() {
                    tunnels.push(Some(tunnel));
                    recv_tasks.push(Some(recv_task));
                    tunnel_states.push(Some(RelayTunnelState::new(
                        Instant::now(),
                        p2p_channel_lifetime,
                        *next_tunnel_id,
                    )));
                    *next_tunnel_id = (*next_tunnel_id).saturating_add(1);
                    tunnel_activity.push(Some(Instant::now()));
                } else {
                    tunnels[tunnel_idx] = Some(tunnel);
                    recv_tasks[tunnel_idx] = Some(recv_task);
                    tunnel_states[tunnel_idx] = Some(RelayTunnelState::new(
                        Instant::now(),
                        p2p_channel_lifetime,
                        *next_tunnel_id,
                    ));
                    *next_tunnel_id = (*next_tunnel_id).saturating_add(1);
                    tunnel_activity[tunnel_idx] = Some(Instant::now());
                }
                let tunnel_id = tunnel_states[tunnel_idx]
                    .as_ref()
                    .map(|x| x.tunnel_id)
                    .unwrap_or(0);
                state_hub.emit(RelayLifecycleEvent::TunnelOpened {
                    tunnel_idx,
                    tunnel_id,
                    mode,
                    source: "scale-up",
                });
            }
            Ok(Err(e)) => {
                let next_timeout = next_expand_connect_timeout(*connect_timeout);
                tracing::warn!(
                    "relay tunnel scale-up failed: idx [{}], desired [{}], timeout={}ms, next_timeout={}ms, err [{}]",
                    tunnel_idx,
                    desired,
                    timeout.as_millis(),
                    next_timeout.as_millis(),
                    e
                );
                *connect_timeout = next_timeout;
                break;
            }
            Err(_) => {
                let next_timeout = next_expand_connect_timeout(*connect_timeout);
                tracing::debug!(
                    "relay tunnel scale-up timeout: idx [{}], desired [{}], timeout={}ms, next_timeout={}ms",
                    tunnel_idx,
                    desired,
                    timeout.as_millis(),
                    next_timeout.as_millis()
                );
                *connect_timeout = next_timeout;
                break;
            }
        }
    }
}

fn shrink_tunnels(
    state_hub: &RelayStateHub,
    desired: usize,
    tunnels: &mut Vec<Option<RelayTunnel>>,
    recv_tasks: &mut Vec<Option<JoinHandle<()>>>,
    tunnel_states: &mut Vec<Option<RelayTunnelState>>,
    tunnel_activity: &mut Vec<Option<Instant>>,
) {
    while active_tunnel_count(tunnels) > desired {
        let Some(tunnel_idx) = last_active_tunnel_idx(tunnels) else {
            break;
        };
        let tunnel_id = tunnel_states[tunnel_idx].as_ref().map(|x| x.tunnel_id);
        if let Some(task) = recv_tasks[tunnel_idx].take() {
            task.abort();
        }
        tunnels[tunnel_idx] = None;
        tunnel_states[tunnel_idx] = None;
        tunnel_activity[tunnel_idx] = None;
        tracing::info!(
            "relay tunnel closed(scale-down): idx [{}], active={}",
            tunnel_idx,
            active_tunnel_count(tunnels)
        );
        state_hub.emit(RelayLifecycleEvent::TunnelClosed {
            tunnel_idx,
            tunnel_id,
            reason: "scale-down".to_string(),
        });
    }
    compact_tunnel_slots(tunnels, recv_tasks, tunnel_states, tunnel_activity);
}

fn active_tunnel_count(tunnels: &[Option<RelayTunnel>]) -> usize {
    tunnels.iter().filter(|x| x.is_some()).count()
}

fn first_inactive_tunnel_idx(tunnels: &[Option<RelayTunnel>]) -> Option<usize> {
    tunnels.iter().position(|x| x.is_none())
}

fn last_active_tunnel_idx(tunnels: &[Option<RelayTunnel>]) -> Option<usize> {
    tunnels.iter().rposition(|x| x.is_some())
}

fn tunnel_id_at(tunnel_states: &[Option<RelayTunnelState>], tunnel_idx: usize) -> Option<u64> {
    tunnel_states
        .get(tunnel_idx)
        .and_then(|x| x.as_ref())
        .map(|x| x.tunnel_id)
}

fn build_tunnel_flow_loads(
    tunnel_slots: usize,
    src_to_flow: &HashMap<SocketAddr, ClientFlow>,
) -> Vec<usize> {
    let mut loads = vec![0_usize; tunnel_slots];
    for flow in src_to_flow.values() {
        if flow.tunnel_idx < loads.len() {
            loads[flow.tunnel_idx] = loads[flow.tunnel_idx].saturating_add(1);
        }
    }
    loads
}

fn tunnel_activity_age(
    now: Instant,
    tunnel_activity: &[Option<Instant>],
    tunnel_idx: usize,
) -> Duration {
    tunnel_activity
        .get(tunnel_idx)
        .and_then(|x| *x)
        .map(|x| now.saturating_duration_since(x))
        .unwrap_or(Duration::MAX)
}

fn pick_best_active_tunnel_idx(
    tunnels: &[Option<RelayTunnel>],
    tunnel_states: &[Option<RelayTunnelState>],
    tunnel_activity: &[Option<Instant>],
    flow_loads: &[usize],
    cursor: &mut usize,
    now: Instant,
) -> Option<usize> {
    pick_best_tunnel_idx_by(
        tunnels.len(),
        |idx| is_tunnel_allocatable(idx, tunnels, tunnel_states),
        tunnel_activity,
        flow_loads,
        cursor,
        now,
    )
}

fn pick_best_any_active_tunnel_idx(
    tunnels: &[Option<RelayTunnel>],
    tunnel_activity: &[Option<Instant>],
    flow_loads: &[usize],
    cursor: &mut usize,
    now: Instant,
) -> Option<usize> {
    pick_best_tunnel_idx_by(
        tunnels.len(),
        |idx| tunnels[idx].is_some(),
        tunnel_activity,
        flow_loads,
        cursor,
        now,
    )
}

fn is_tunnel_allocatable(
    tunnel_idx: usize,
    tunnels: &[Option<RelayTunnel>],
    tunnel_states: &[Option<RelayTunnelState>],
) -> bool {
    if tunnels.get(tunnel_idx).and_then(|x| x.as_ref()).is_none() {
        return false;
    }
    tunnel_states
        .get(tunnel_idx)
        .and_then(|x| x.as_ref())
        .is_some_and(|x| x.allocatable)
}

fn pick_best_tunnel_idx_by<F>(
    len: usize,
    mut is_active: F,
    tunnel_activity: &[Option<Instant>],
    flow_loads: &[usize],
    cursor: &mut usize,
    now: Instant,
) -> Option<usize>
where
    F: FnMut(usize) -> bool,
{
    if len == 0 {
        return None;
    }

    let start = *cursor % len;
    let mut best_idx = None;
    let mut best_key = None;
    for offset in 0..len {
        let idx = (start + offset) % len;
        if !is_active(idx) {
            continue;
        }

        let age = tunnel_activity_age(now, tunnel_activity, idx);
        let stale = age >= TUNNEL_STALE_THRESHOLD;
        let load = flow_loads.get(idx).copied().unwrap_or(0);
        let key = (stale, load, age, offset);
        if best_key.is_none_or(|x| key < x) {
            best_key = Some(key);
            best_idx = Some(idx);
        }
    }
    if let Some(idx) = best_idx {
        *cursor = (idx + 1) % len;
    }
    best_idx
}

fn compact_tunnel_slots(
    tunnels: &mut Vec<Option<RelayTunnel>>,
    recv_tasks: &mut Vec<Option<JoinHandle<()>>>,
    tunnel_states: &mut Vec<Option<RelayTunnelState>>,
    tunnel_activity: &mut Vec<Option<Instant>>,
) {
    while tunnels.last().is_some_and(|x| x.is_none())
        && tunnel_states.last().is_some_and(|x| x.is_none())
        && tunnel_activity.last().is_some_and(|x| x.is_none())
    {
        tunnels.pop();
        recv_tasks.pop();
        tunnel_states.pop();
        tunnel_activity.pop();
    }
}

fn cleanup_stale_down_tunnel_markers(
    tunnels: &mut [Option<RelayTunnel>],
    recv_tasks: &mut [Option<JoinHandle<()>>],
    tunnel_states: &mut [Option<RelayTunnelState>],
    tunnel_activity: &mut [Option<Instant>],
) {
    let now = Instant::now();
    for tunnel_idx in 0..tunnels.len() {
        if tunnels[tunnel_idx].is_some() {
            continue;
        }
        let down_expired = tunnel_states[tunnel_idx]
            .as_ref()
            .and_then(|x| x.down_until)
            .is_some_and(|until| now >= until);
        if down_expired {
            if let Some(task) = recv_tasks[tunnel_idx].take() {
                task.abort();
            }
            tunnel_states[tunnel_idx] = None;
            tunnel_activity[tunnel_idx] = None;
        }
    }
}

fn rebalance_client_flows(
    state_hub: &RelayStateHub,
    src_to_flow: &mut HashMap<SocketAddr, ClientFlow>,
    tunnels: &[Option<RelayTunnel>],
    tunnel_states: &[Option<RelayTunnelState>],
    tunnel_activity: &[Option<Instant>],
    cursor: &mut usize,
    now: Instant,
) {
    if src_to_flow.is_empty() || active_tunnel_count(tunnels) <= 1 {
        return;
    }

    let mut flow_loads = build_tunnel_flow_loads(tunnels.len(), src_to_flow);
    let mut migrated = 0_usize;

    for (src, flow) in src_to_flow.iter_mut() {
        if now.duration_since(flow.route_updated_at) < FLOW_ROUTE_RESELECT_INTERVAL {
            continue;
        }
        let current_idx = flow.tunnel_idx;
        let mut best_idx = pick_best_active_tunnel_idx(
            tunnels,
            tunnel_states,
            tunnel_activity,
            flow_loads.as_slice(),
            cursor,
            now,
        );
        if best_idx.is_none() {
            best_idx = pick_best_any_active_tunnel_idx(
                tunnels,
                tunnel_activity,
                flow_loads.as_slice(),
                cursor,
                now,
            );
        }
        let Some(best_idx) = best_idx else {
            break;
        };

        let current_alive = tunnels.get(current_idx).and_then(|x| x.as_ref()).is_some();
        let current_allocatable = is_tunnel_allocatable(current_idx, tunnels, tunnel_states);
        let current_age = tunnel_activity_age(now, tunnel_activity, current_idx);
        let best_age = tunnel_activity_age(now, tunnel_activity, best_idx);
        let current_load = flow_loads.get(current_idx).copied().unwrap_or(usize::MAX);
        let best_load = flow_loads.get(best_idx).copied().unwrap_or(usize::MAX);

        let stale_gap_enough =
            current_age > best_age && (current_age - best_age) >= FLOW_MIGRATE_STALE_GAP;
        let load_gap_enough = current_load >= best_load.saturating_add(FLOW_MIGRATE_LOAD_GAP);
        if current_idx != best_idx
            && (!current_alive || !current_allocatable || stale_gap_enough || load_gap_enough)
        {
            if current_idx < flow_loads.len() && flow_loads[current_idx] > 0 {
                flow_loads[current_idx] -= 1;
            }
            if best_idx < flow_loads.len() {
                flow_loads[best_idx] = flow_loads[best_idx].saturating_add(1);
            }
            tracing::debug!(
                "relay flow migrated: id [{}], src [{}], old_tunnel [{}], new_tunnel [{}], old_age={}ms, new_age={}ms, old_load={}, new_load={}",
                flow.flow_id,
                src,
                current_idx,
                best_idx,
                current_age.as_millis(),
                best_age.as_millis(),
                current_load,
                best_load
            );
            state_hub.emit(RelayLifecycleEvent::FlowMigrated {
                flow_id: flow.flow_id,
                src: *src,
                old_tunnel_idx: current_idx,
                old_tunnel_id: Some(flow.tunnel_id),
                new_tunnel_idx: best_idx,
                new_tunnel_id: tunnel_id_at(tunnel_states, best_idx),
                reason: "rebalance",
            });
            flow.tunnel_idx = best_idx;
            flow.tunnel_id = tunnel_id_at(tunnel_states, best_idx)
                .unwrap_or_else(|| (best_idx as u64).saturating_add(1));
            migrated += 1;
        }
        flow.route_updated_at = now;
    }

    if migrated > 0 {
        tracing::debug!(
            "relay flow rebalance completed: migrated [{}], active_flows [{}]",
            migrated,
            src_to_flow.len()
        );
    }
}

async fn try_rotate_expired_tunnels<H: CtrlHandler>(
    ctrl: &CtrlInvoker<H>,
    state_hub: &RelayStateHub,
    target: SocketAddr,
    idle_timeout_secs: u32,
    max_payload: usize,
    p2p_channel_lifetime: Duration,
    tunnels: &mut Vec<Option<RelayTunnel>>,
    recv_tasks: &mut Vec<Option<JoinHandle<()>>>,
    tunnel_states: &mut Vec<Option<RelayTunnelState>>,
    tunnel_activity: &mut Vec<Option<Instant>>,
    next_tunnel_id: &mut u64,
    inbound_tx: &mpsc::Sender<TunnelRecvEvent>,
    connect_timeout: &mut Duration,
) {
    let now = Instant::now();
    for old_idx in 0..tunnels.len() {
        if !is_tunnel_allocatable(old_idx, tunnels, tunnel_states) {
            continue;
        }
        let Some(state) = tunnel_states.get(old_idx).and_then(|x| x.as_ref()) else {
            continue;
        };
        if now < state.expire_at {
            continue;
        }

        let timeout = *connect_timeout;
        match time::timeout(
            timeout,
            open_udp_relay_tunnel(ctrl, target, idle_timeout_secs, max_payload),
        )
        .await
        {
            Ok(Ok(new_tunnel)) => {
                *connect_timeout = P2P_EXPAND_CONNECT_TIMEOUT_INITIAL;
                let mode = new_tunnel.codec.mode_name().to_string();
                let new_idx = match first_inactive_tunnel_idx(tunnels) {
                    Some(idx) => idx,
                    None => tunnels.len(),
                };
                let recv_task = spawn_tunnel_recv_task(
                    new_idx,
                    new_tunnel.socket.clone(),
                    new_tunnel.codec,
                    max_payload,
                    inbound_tx.clone(),
                );

                if new_idx == tunnels.len() {
                    tunnels.push(Some(new_tunnel));
                    recv_tasks.push(Some(recv_task));
                    tunnel_states.push(Some(RelayTunnelState::new(
                        now,
                        p2p_channel_lifetime,
                        *next_tunnel_id,
                    )));
                    *next_tunnel_id = (*next_tunnel_id).saturating_add(1);
                    tunnel_activity.push(Some(now));
                } else {
                    tunnels[new_idx] = Some(new_tunnel);
                    recv_tasks[new_idx] = Some(recv_task);
                    tunnel_states[new_idx] = Some(RelayTunnelState::new(
                        now,
                        p2p_channel_lifetime,
                        *next_tunnel_id,
                    ));
                    *next_tunnel_id = (*next_tunnel_id).saturating_add(1);
                    tunnel_activity[new_idx] = Some(now);
                }

                if let Some(old_state) = tunnel_states.get_mut(old_idx).and_then(|x| x.as_mut()) {
                    old_state.allocatable = false;
                }
                let new_tunnel_id = tunnel_states
                    .get(new_idx)
                    .and_then(|x| x.as_ref())
                    .map(|x| x.tunnel_id)
                    .unwrap_or(0);
                let old_tunnel_id = tunnel_states
                    .get(old_idx)
                    .and_then(|x| x.as_ref())
                    .map(|x| x.tunnel_id)
                    .unwrap_or(0);
                tracing::info!(
                    "relay tunnel rotated: old_idx [{}] -> new_idx [{}], old enters draining",
                    old_idx,
                    new_idx
                );
                state_hub.emit(RelayLifecycleEvent::TunnelOpened {
                    tunnel_idx: new_idx,
                    tunnel_id: new_tunnel_id,
                    mode,
                    source: "rotate",
                });
                state_hub.emit(RelayLifecycleEvent::TunnelRotated {
                    old_tunnel_idx: old_idx,
                    old_tunnel_id,
                    new_tunnel_idx: new_idx,
                    new_tunnel_id,
                });
            }
            Ok(Err(e)) => {
                let next_timeout = next_expand_connect_timeout(*connect_timeout);
                tracing::warn!(
                    "relay tunnel rotate failed(open replacement): old_idx [{}], timeout={}ms, next_timeout={}ms, err [{}]",
                    old_idx,
                    timeout.as_millis(),
                    next_timeout.as_millis(),
                    e
                );
                *connect_timeout = next_timeout;
            }
            Err(_) => {
                let next_timeout = next_expand_connect_timeout(*connect_timeout);
                tracing::debug!(
                    "relay tunnel rotate timeout(open replacement): old_idx [{}], timeout={}ms, next_timeout={}ms",
                    old_idx,
                    timeout.as_millis(),
                    next_timeout.as_millis()
                );
                *connect_timeout = next_timeout;
            }
        }
    }
}

fn close_drained_tunnels(
    state_hub: &RelayStateHub,
    src_to_flow: &HashMap<SocketAddr, ClientFlow>,
    tunnels: &mut [Option<RelayTunnel>],
    recv_tasks: &mut [Option<JoinHandle<()>>],
    tunnel_states: &mut [Option<RelayTunnelState>],
    tunnel_activity: &mut [Option<Instant>],
) {
    let flow_loads = build_tunnel_flow_loads(tunnels.len(), src_to_flow);
    for tunnel_idx in 0..tunnels.len() {
        let Some(state) = tunnel_states.get(tunnel_idx).and_then(|x| x.as_ref()) else {
            continue;
        };
        if state.allocatable {
            continue;
        }
        if flow_loads.get(tunnel_idx).copied().unwrap_or(0) > 0 {
            continue;
        }
        if tunnels[tunnel_idx].is_none() {
            continue;
        }

        if let Some(task) = recv_tasks[tunnel_idx].take() {
            task.abort();
        }
        let tunnel_id = Some(state.tunnel_id);
        tunnels[tunnel_idx] = None;
        tunnel_states[tunnel_idx] = None;
        tunnel_activity[tunnel_idx] = None;
        tracing::info!("relay tunnel closed(drained): idx [{}]", tunnel_idx);
        state_hub.emit(RelayLifecycleEvent::TunnelClosed {
            tunnel_idx,
            tunnel_id,
            reason: "drained".to_string(),
        });
    }
}

#[derive(Debug, Clone, Copy)]
enum TunnelOpenAction {
    ScaleUp { desired: usize },
    Rotate { old_idx: usize },
    DemandOpen,
}

#[derive(Debug)]
enum TunnelOpenTaskResult {
    Opened(RelayTunnel),
    Timeout,
}

#[derive(Debug)]
struct PendingTunnelOpen {
    action: TunnelOpenAction,
    timeout: Duration,
    handle: JoinHandle<Result<TunnelOpenTaskResult>>,
}

fn first_expired_allocatable_tunnel_idx(
    tunnels: &[Option<RelayTunnel>],
    tunnel_states: &[Option<RelayTunnelState>],
    now: Instant,
) -> Option<usize> {
    for old_idx in 0..tunnels.len() {
        if !is_tunnel_allocatable(old_idx, tunnels, tunnel_states) {
            continue;
        }
        let Some(state) = tunnel_states.get(old_idx).and_then(|x| x.as_ref()) else {
            continue;
        };
        if now >= state.expire_at {
            return Some(old_idx);
        }
    }
    None
}

fn pick_tunnel_open_action(
    channel_pool: ChannelPoolConfig,
    active_flows: usize,
    demand_open_pending: bool,
    tunnels: &[Option<RelayTunnel>],
    tunnel_states: &[Option<RelayTunnelState>],
    now: Instant,
) -> Option<TunnelOpenAction> {
    let desired = channel_pool.desired_channels(active_flows);
    if active_tunnel_count(tunnels) < desired {
        return Some(TunnelOpenAction::ScaleUp { desired });
    }

    if let Some(old_idx) = first_expired_allocatable_tunnel_idx(tunnels, tunnel_states, now) {
        return Some(TunnelOpenAction::Rotate { old_idx });
    }

    if demand_open_pending && active_tunnel_count(tunnels) == 0 && channel_pool.min_channels == 0 {
        return Some(TunnelOpenAction::DemandOpen);
    }

    None
}

fn spawn_tunnel_open_task<H: CtrlHandler>(
    ctrl: CtrlInvoker<H>,
    action: TunnelOpenAction,
    timeout: Duration,
    target: SocketAddr,
    idle_timeout_secs: u32,
    max_payload: usize,
) -> PendingTunnelOpen {
    let task_name = match action {
        TunnelOpenAction::ScaleUp { .. } => "relay-tunnel-open-scale-up",
        TunnelOpenAction::Rotate { .. } => "relay-tunnel-open-rotate",
        TunnelOpenAction::DemandOpen => "relay-tunnel-open-demand",
    }
    .to_string();

    let handle = spawn_with_name(task_name, async move {
        match time::timeout(
            timeout,
            open_udp_relay_tunnel(&ctrl, target, idle_timeout_secs, max_payload),
        )
        .await
        {
            Ok(Ok(tunnel)) => Ok(TunnelOpenTaskResult::Opened(tunnel)),
            Ok(Err(e)) => Err(e),
            Err(_) => Ok(TunnelOpenTaskResult::Timeout),
        }
    });

    PendingTunnelOpen {
        action,
        timeout,
        handle,
    }
}

fn apply_opened_tunnel(
    tunnel: RelayTunnel,
    now: Instant,
    p2p_channel_lifetime: Duration,
    max_payload: usize,
    next_tunnel_id: &mut u64,
    inbound_tx: &mpsc::Sender<TunnelRecvEvent>,
    tunnels: &mut Vec<Option<RelayTunnel>>,
    recv_tasks: &mut Vec<Option<JoinHandle<()>>>,
    tunnel_states: &mut Vec<Option<RelayTunnelState>>,
    tunnel_activity: &mut Vec<Option<Instant>>,
) -> (usize, u64, String) {
    let tunnel_idx = match first_inactive_tunnel_idx(tunnels) {
        Some(idx) => idx,
        None => tunnels.len(),
    };
    let mode = tunnel.codec.mode_name().to_string();
    let recv_task = spawn_tunnel_recv_task(
        tunnel_idx,
        tunnel.socket.clone(),
        tunnel.codec,
        max_payload,
        inbound_tx.clone(),
    );

    if tunnel_idx == tunnels.len() {
        tunnels.push(Some(tunnel));
        recv_tasks.push(Some(recv_task));
        tunnel_states.push(Some(RelayTunnelState::new(
            now,
            p2p_channel_lifetime,
            *next_tunnel_id,
        )));
        tunnel_activity.push(Some(now));
    } else {
        tunnels[tunnel_idx] = Some(tunnel);
        recv_tasks[tunnel_idx] = Some(recv_task);
        tunnel_states[tunnel_idx] = Some(RelayTunnelState::new(
            now,
            p2p_channel_lifetime,
            *next_tunnel_id,
        ));
        tunnel_activity[tunnel_idx] = Some(now);
    }
    *next_tunnel_id = (*next_tunnel_id).saturating_add(1);

    let tunnel_id = tunnel_states[tunnel_idx]
        .as_ref()
        .map(|x| x.tunnel_id)
        .unwrap_or(0);
    (tunnel_idx, tunnel_id, mode)
}

async fn relay_loop<H: CtrlHandler>(
    ctrl: CtrlInvoker<H>,
    local: &UdpSocket,
    selected: &AgentInfo,
    state_hub: &RelayStateHub,
    target: SocketAddr,
    idle_timeout: Duration,
    max_payload: usize,
    channel_pool: ChannelPoolConfig,
    mut tunnels: Vec<Option<RelayTunnel>>,
    mut recv_tasks: Vec<Option<JoinHandle<()>>>,
    mut tunnel_states: Vec<Option<RelayTunnelState>>,
    mut tunnel_activity: Vec<Option<Instant>>,
    p2p_channel_lifetime: Duration,
    mut next_tunnel_id: u64,
    inbound_tx: mpsc::Sender<TunnelRecvEvent>,
    mut inbound_rx: mpsc::Receiver<TunnelRecvEvent>,
) -> Result<()> {
    state_hub.set_selected_agent(selected, true);
    let idle_timeout_secs = u32::try_from(idle_timeout.as_secs())
        .with_context(|| format!("udp idle timeout too large [{}s]", idle_timeout.as_secs()))?;

    let mut local_buf = vec![0_u8; 64 * 1024];
    let mut tunnel_send_buf = vec![0_u8; UDP_RELAY_META_LEN_OBFS + max_payload];
    let mut src_to_flow: HashMap<SocketAddr, ClientFlow> = HashMap::new();
    let mut flow_to_src: HashMap<u64, SocketAddr> = HashMap::new();
    let mut next_flow = 1_u64;
    let mut next_tunnel_for_new_flow = 0_usize;
    let mut p2p_expand_connect_timeout = P2P_EXPAND_CONNECT_TIMEOUT_INITIAL;
    let mut pending_tunnel_open: Option<PendingTunnelOpen> = None;
    let mut demand_open_pending = false;

    let mut cleanup = time::interval(FLOW_CLEANUP_INTERVAL);
    cleanup.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut heartbeat = time::interval(UDP_RELAY_HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let result: Result<()> = loop {
        if pending_tunnel_open.is_none() {
            if let Some(action) = pick_tunnel_open_action(
                channel_pool,
                src_to_flow.len(),
                demand_open_pending,
                &tunnels,
                &tunnel_states,
                Instant::now(),
            ) {
                pending_tunnel_open = Some(spawn_tunnel_open_task(
                    ctrl.clone(),
                    action,
                    p2p_expand_connect_timeout,
                    target,
                    idle_timeout_secs,
                    max_payload,
                ));
            }
        }

        tokio::select! {
            r = local.recv_from(&mut local_buf), if active_tunnel_count(&tunnels) > 0 => {
                let (n, from) = r?;
                if n == 0 {
                    continue;
                }
                if n > max_payload {
                    tracing::warn!("drop oversized local udp packet: from [{}], size [{}], max [{}]", from, n, max_payload);
                    continue;
                }

                let now = Instant::now();
                let mut flow_loads = build_tunnel_flow_loads(tunnels.len(), &src_to_flow);
                let (flow_id, tunnel_idx) = if let Some(flow) = src_to_flow.get_mut(&from) {
                    flow.updated_at = now;
                    if tunnels
                        .get(flow.tunnel_idx)
                        .and_then(|slot| slot.as_ref())
                        .is_none()
                    {
                        let mut new_idx = pick_best_active_tunnel_idx(
                            &tunnels,
                            &tunnel_states,
                            &tunnel_activity,
                            flow_loads.as_slice(),
                            &mut next_tunnel_for_new_flow,
                            now,
                        );
                        if new_idx.is_none() {
                            new_idx = pick_best_any_active_tunnel_idx(
                                &tunnels,
                                &tunnel_activity,
                                flow_loads.as_slice(),
                                &mut next_tunnel_for_new_flow,
                                now,
                            );
                        }
                        let Some(new_idx) = new_idx else {
                            break Err(anyhow::anyhow!("no relay tunnel available"));
                        };
                        if flow.tunnel_idx < flow_loads.len() && flow_loads[flow.tunnel_idx] > 0 {
                            flow_loads[flow.tunnel_idx] -= 1;
                        }
                        if new_idx < flow_loads.len() {
                            flow_loads[new_idx] = flow_loads[new_idx].saturating_add(1);
                        }
                        tracing::debug!(
                            "relay flow rebound: id [{}], src [{}], old_tunnel [{}], new_tunnel [{}]",
                            flow.flow_id,
                            from,
                            flow.tunnel_idx,
                            new_idx
                        );
                        state_hub.emit(RelayLifecycleEvent::FlowMigrated {
                            flow_id: flow.flow_id,
                            src: from,
                            old_tunnel_idx: flow.tunnel_idx,
                            old_tunnel_id: Some(flow.tunnel_id),
                            new_tunnel_idx: new_idx,
                            new_tunnel_id: tunnel_id_at(&tunnel_states, new_idx),
                            reason: "rebound",
                        });
                        flow.tunnel_idx = new_idx;
                        flow.tunnel_id = tunnel_id_at(&tunnel_states, new_idx)
                            .unwrap_or_else(|| (new_idx as u64).saturating_add(1));
                        flow.route_updated_at = now;
                    }
                    (flow.flow_id, flow.tunnel_idx)
                } else {
                    if active_tunnel_count(&tunnels) == 0 {
                        break Err(anyhow::anyhow!("no relay tunnel available"));
                    }
                    let flow_id = alloc_next_flow_id(&mut next_flow, &flow_to_src)?;
                    let mut tunnel_idx = pick_best_active_tunnel_idx(
                        &tunnels,
                        &tunnel_states,
                        &tunnel_activity,
                        flow_loads.as_slice(),
                        &mut next_tunnel_for_new_flow,
                        now,
                    );
                    if tunnel_idx.is_none() {
                        tunnel_idx = pick_best_any_active_tunnel_idx(
                            &tunnels,
                            &tunnel_activity,
                            flow_loads.as_slice(),
                            &mut next_tunnel_for_new_flow,
                            now,
                        );
                    }
                    let Some(tunnel_idx) = tunnel_idx else {
                        break Err(anyhow::anyhow!("no relay tunnel available"));
                    };
                    src_to_flow.insert(from, ClientFlow {
                        flow_id,
                        updated_at: now,
                        route_updated_at: now,
                        tunnel_idx,
                        tunnel_id: tunnel_id_at(&tunnel_states, tunnel_idx)
                            .unwrap_or_else(|| (tunnel_idx as u64).saturating_add(1)),
                    });
                    if tunnel_idx < flow_loads.len() {
                        flow_loads[tunnel_idx] = flow_loads[tunnel_idx].saturating_add(1);
                    }
                    flow_to_src.insert(flow_id, from);
                    tracing::debug!(
                        "relay flow created: id [{}], src [{}], target [{}], tunnel [{}]",
                        flow_id,
                        from,
                        target,
                        tunnel_idx
                    );
                    state_hub.emit(RelayLifecycleEvent::FlowCreated {
                        flow_id,
                        src: from,
                        target,
                        tunnel_idx,
                        tunnel_id: tunnel_id_at(&tunnel_states, tunnel_idx),
                    });
                    (flow_id, tunnel_idx)
                };

                let Some(tunnel) = tunnels.get(tunnel_idx).and_then(|slot| slot.as_ref()) else {
                    break Err(anyhow::anyhow!("relay flow has invalid tunnel index [{}]", tunnel_idx));
                };
                let packet_len = encode_udp_relay_packet(
                    &mut tunnel_send_buf,
                    flow_id,
                    &local_buf[..n],
                    tunnel.codec,
                )?;
                if let Err(e) = tunnel.socket.send(&tunnel_send_buf[..packet_len]).await {
                    tracing::warn!(
                        "relay flow closed(error): id [{}], src [{}], tunnel [{}], reason [{}]",
                        flow_id,
                        from,
                        tunnel_idx,
                        e
                    );
                    if inbound_tx
                        .send(TunnelRecvEvent::Closed {
                            tunnel_idx,
                            reason: format!("send failed [{e}]"),
                        })
                        .await
                        .is_err()
                    {
                        break Err(anyhow::anyhow!("relay tunnel close event channel closed"));
                    }
                    continue;
                }
                if tunnel_idx < tunnel_activity.len() {
                    tunnel_activity[tunnel_idx] = Some(now);
                }
            }
            _ = local.readable(), if active_tunnel_count(&tunnels) == 0 && channel_pool.min_channels == 0 && pending_tunnel_open.is_none() => {
                demand_open_pending = true;
            }
            open_result = async {
                let pending = pending_tunnel_open
                    .as_mut()
                    .expect("pending tunnel open must exist");
                (&mut pending.handle).await
            }, if pending_tunnel_open.is_some() => {
                let pending = pending_tunnel_open
                    .take()
                    .expect("pending tunnel open must exist");
                match open_result {
                    Ok(Ok(TunnelOpenTaskResult::Opened(tunnel))) => {
                        p2p_expand_connect_timeout = P2P_EXPAND_CONNECT_TIMEOUT_INITIAL;
                        let now = Instant::now();
                        let (new_idx, new_tunnel_id, mode) = apply_opened_tunnel(
                            tunnel,
                            now,
                            p2p_channel_lifetime,
                            max_payload,
                            &mut next_tunnel_id,
                            &inbound_tx,
                            &mut tunnels,
                            &mut recv_tasks,
                            &mut tunnel_states,
                            &mut tunnel_activity,
                        );

                        match pending.action {
                            TunnelOpenAction::ScaleUp { .. } => {
                                tracing::info!(
                                    "relay tunnel connected(scale-up): idx [{}], codec [{}], active={}",
                                    new_idx,
                                    mode,
                                    active_tunnel_count(&tunnels)
                                );
                                state_hub.emit(RelayLifecycleEvent::TunnelOpened {
                                    tunnel_idx: new_idx,
                                    tunnel_id: new_tunnel_id,
                                    mode,
                                    source: "scale-up",
                                });
                            }
                            TunnelOpenAction::Rotate { old_idx } => {
                                let mut old_tunnel_id = 0_u64;
                                let mut rotated = false;
                                if let Some(old_state) =
                                    tunnel_states.get_mut(old_idx).and_then(|x| x.as_mut())
                                {
                                    old_tunnel_id = old_state.tunnel_id;
                                    if old_state.allocatable {
                                        old_state.allocatable = false;
                                        rotated = true;
                                    }
                                }

                                tracing::info!(
                                    "relay tunnel connected(rotate): idx [{}], codec [{}], active={}",
                                    new_idx,
                                    mode,
                                    active_tunnel_count(&tunnels)
                                );
                                state_hub.emit(RelayLifecycleEvent::TunnelOpened {
                                    tunnel_idx: new_idx,
                                    tunnel_id: new_tunnel_id,
                                    mode,
                                    source: "rotate",
                                });

                                if rotated {
                                    tracing::info!(
                                        "relay tunnel rotated: old_idx [{}] -> new_idx [{}], old enters draining",
                                        old_idx,
                                        new_idx
                                    );
                                    state_hub.emit(RelayLifecycleEvent::TunnelRotated {
                                        old_tunnel_idx: old_idx,
                                        old_tunnel_id,
                                        new_tunnel_idx: new_idx,
                                        new_tunnel_id,
                                    });
                                } else {
                                    tracing::debug!(
                                        "relay tunnel rotate fallback: old_idx [{}] no longer allocatable/alive",
                                        old_idx
                                    );
                                }
                            }
                            TunnelOpenAction::DemandOpen => {
                                demand_open_pending = false;
                                tracing::info!(
                                    "relay tunnel connected(demand-open): idx [{}], codec [{}], active={}",
                                    new_idx,
                                    mode,
                                    active_tunnel_count(&tunnels)
                                );
                                state_hub.emit(RelayLifecycleEvent::TunnelOpened {
                                    tunnel_idx: new_idx,
                                    tunnel_id: new_tunnel_id,
                                    mode,
                                    source: "demand-open",
                                });
                            }
                        }
                    }
                    Ok(Ok(TunnelOpenTaskResult::Timeout)) => {
                        let next_timeout = next_expand_connect_timeout(p2p_expand_connect_timeout);
                        match pending.action {
                            TunnelOpenAction::ScaleUp { desired } => {
                                let target_idx = first_inactive_tunnel_idx(&tunnels)
                                    .unwrap_or(tunnels.len());
                                tracing::debug!(
                                    "relay tunnel scale-up timeout: idx [{}], desired [{}], timeout={}ms, next_timeout={}ms",
                                    target_idx,
                                    desired,
                                    pending.timeout.as_millis(),
                                    next_timeout.as_millis()
                                );
                            }
                            TunnelOpenAction::Rotate { old_idx } => {
                                tracing::debug!(
                                    "relay tunnel rotate timeout(open replacement): old_idx [{}], timeout={}ms, next_timeout={}ms",
                                    old_idx,
                                    pending.timeout.as_millis(),
                                    next_timeout.as_millis()
                                );
                            }
                            TunnelOpenAction::DemandOpen => {
                                tracing::debug!(
                                    "relay tunnel demand-open timeout: timeout={}ms, next_timeout={}ms",
                                    pending.timeout.as_millis(),
                                    next_timeout.as_millis()
                                );
                            }
                        }
                        p2p_expand_connect_timeout = next_timeout;
                    }
                    Ok(Err(e)) => {
                        let next_timeout = next_expand_connect_timeout(p2p_expand_connect_timeout);
                        match pending.action {
                            TunnelOpenAction::ScaleUp { desired } => {
                                let target_idx = first_inactive_tunnel_idx(&tunnels)
                                    .unwrap_or(tunnels.len());
                                tracing::warn!(
                                    "relay tunnel scale-up failed: idx [{}], desired [{}], timeout={}ms, next_timeout={}ms, err [{}]",
                                    target_idx,
                                    desired,
                                    pending.timeout.as_millis(),
                                    next_timeout.as_millis(),
                                    e
                                );
                            }
                            TunnelOpenAction::Rotate { old_idx } => {
                                tracing::warn!(
                                    "relay tunnel rotate failed(open replacement): old_idx [{}], timeout={}ms, next_timeout={}ms, err [{}]",
                                    old_idx,
                                    pending.timeout.as_millis(),
                                    next_timeout.as_millis(),
                                    e
                                );
                            }
                            TunnelOpenAction::DemandOpen => {
                                tracing::warn!(
                                    "relay tunnel demand-open failed: timeout={}ms, next_timeout={}ms, err [{}]",
                                    pending.timeout.as_millis(),
                                    next_timeout.as_millis(),
                                    e
                                );
                            }
                        }
                        p2p_expand_connect_timeout = next_timeout;
                    }
                    Err(e) => {
                        let next_timeout = next_expand_connect_timeout(p2p_expand_connect_timeout);
                        tracing::warn!(
                            "relay tunnel open task failed: action [{:?}], timeout={}ms, next_timeout={}ms, err [{}]",
                            pending.action,
                            pending.timeout.as_millis(),
                            next_timeout.as_millis(),
                            e
                        );
                        p2p_expand_connect_timeout = next_timeout;
                    }
                }
            }
            evt = inbound_rx.recv() => {
                let Some(evt) = evt else {
                    break Err(anyhow::anyhow!("relay tunnel recv loop channel closed"));
                };
                match evt {
                    TunnelRecvEvent::Packet(pkt) => {
                        if pkt.tunnel_idx >= tunnels.len() || tunnels[pkt.tunnel_idx].is_none() {
                            tracing::debug!(
                                "drop stale tunnel packet: flow [{}], tunnel [{}]",
                                pkt.flow_id,
                                pkt.tunnel_idx
                            );
                            continue;
                        }
                        if pkt.tunnel_idx < tunnel_activity.len() {
                            tunnel_activity[pkt.tunnel_idx] = Some(Instant::now());
                        }
                        if let Some(from) = flow_to_src.get(&pkt.flow_id).copied() {
                    if let Some(flow) = src_to_flow.get_mut(&from) {
                        flow.updated_at = Instant::now();
                    }
                            if let Err(e) = local.send_to(&pkt.payload, from).await {
                        tracing::warn!(
                                    "relay flow closed(error): id [{}], src [{}], tunnel [{}], reason [{}]",
                                    pkt.flow_id,
                            from,
                                    pkt.tunnel_idx,
                            e
                        );
                    }
                } else {
                            tracing::debug!(
                                "drop tunnel packet for unknown flow [{}], tunnel [{}]",
                                pkt.flow_id,
                                pkt.tunnel_idx
                            );
                        }
                    }
                    TunnelRecvEvent::Heartbeat { tunnel_idx } => {
                        if tunnel_idx >= tunnels.len() || tunnels[tunnel_idx].is_none() {
                            continue;
                        }
                        if tunnel_idx < tunnel_activity.len() {
                            tunnel_activity[tunnel_idx] = Some(Instant::now());
                        }
                    }
                    TunnelRecvEvent::Closed { tunnel_idx, reason } => {
                        if tunnel_idx >= tunnels.len() || tunnels[tunnel_idx].is_none() {
                            tracing::debug!(
                                "ignore stale tunnel closed event: idx [{}], reason [{}]",
                                tunnel_idx,
                                reason
                            );
                            continue;
                        }

                        tunnels[tunnel_idx] = None;
                        recv_tasks[tunnel_idx] = None;
                        let closed_at = Instant::now();
                        let tunnel_id = tunnel_states
                            .get(tunnel_idx)
                            .and_then(|x| x.as_ref())
                            .map(|x| x.tunnel_id)
                            .unwrap_or_else(|| (tunnel_idx as u64).saturating_add(1));
                        tunnel_states[tunnel_idx] =
                            Some(RelayTunnelState::down_marker(closed_at, tunnel_id));
                        tunnel_activity[tunnel_idx] = Some(closed_at);
                        tracing::warn!(
                            "relay tunnel closed: idx [{}], reason [{}], active={}",
                            tunnel_idx,
                            reason,
                            active_tunnel_count(&tunnels)
                        );
                        state_hub.emit(RelayLifecycleEvent::TunnelClosed {
                            tunnel_idx,
                            tunnel_id: Some(tunnel_id),
                            reason: reason.clone(),
                        });

                        let mut removed = Vec::new();
                        for (src, flow) in src_to_flow.iter() {
                            if flow.tunnel_idx == tunnel_idx {
                                removed.push((*src, flow.flow_id, flow.tunnel_id));
                            }
                        }
                        for (src, flow_id, flow_tunnel_id) in removed {
                            src_to_flow.remove(&src);
                            flow_to_src.remove(&flow_id);
                            tracing::debug!(
                                "relay flow closed(tunnel-lost): id [{}], src [{}], tunnel [{}]",
                                flow_id,
                                src,
                                tunnel_idx
                            );
                            state_hub.emit(RelayLifecycleEvent::FlowClosed {
                                flow_id,
                                src,
                                tunnel_idx,
                                tunnel_id: Some(flow_tunnel_id),
                                reason: "tunnel-lost".to_string(),
                            });
                        }

                        compact_tunnel_slots(
                            &mut tunnels,
                            &mut recv_tasks,
                            &mut tunnel_states,
                            &mut tunnel_activity,
                        );
                        if next_tunnel_for_new_flow >= tunnels.len() {
                            next_tunnel_for_new_flow = 0;
                        }
                    }
                }
            }
            _ = cleanup.tick() => {
                cleanup_client_flows(state_hub, &mut src_to_flow, &mut flow_to_src, idle_timeout);
                let desired = channel_pool.desired_channels(src_to_flow.len());
                rebalance_client_flows(
                    state_hub,
                    &mut src_to_flow,
                    &tunnels,
                    &tunnel_states,
                    &tunnel_activity,
                    &mut next_tunnel_for_new_flow,
                    Instant::now(),
                );
                close_drained_tunnels(
                    state_hub,
                    &src_to_flow,
                    &mut tunnels,
                    &mut recv_tasks,
                    &mut tunnel_states,
                    &mut tunnel_activity,
                );
                cleanup_stale_down_tunnel_markers(
                    &mut tunnels,
                    &mut recv_tasks,
                    &mut tunnel_states,
                    &mut tunnel_activity,
                );
                compact_tunnel_slots(
                    &mut tunnels,
                    &mut recv_tasks,
                    &mut tunnel_states,
                    &mut tunnel_activity,
                );
                if src_to_flow.is_empty() {
                    shrink_tunnels(
                        state_hub,
                        desired,
                        &mut tunnels,
                        &mut recv_tasks,
                        &mut tunnel_states,
                        &mut tunnel_activity,
                    );
                    if next_tunnel_for_new_flow >= tunnels.len() {
                        next_tunnel_for_new_flow = 0;
                    }
                }
                state_hub.refresh_runtime(
                    target,
                    &tunnels,
                    &tunnel_states,
                    &tunnel_activity,
                    &src_to_flow,
                );
            }
            _ = heartbeat.tick() => {
                let mut failed_tunnels = Vec::new();
                for (tunnel_idx, tunnel) in tunnels.iter().enumerate() {
                    let Some(tunnel) = tunnel.as_ref() else {
                        continue;
                    };
                    let packet_len = encode_udp_relay_packet(
                        &mut tunnel_send_buf,
                        UDP_RELAY_HEARTBEAT_FLOW_ID,
                        &[],
                        tunnel.codec,
                    )?;
                    if let Err(e) = tunnel.socket.send(&tunnel_send_buf[..packet_len]).await {
                        tracing::warn!(
                            "relay heartbeat send failed: tunnel [{}], err [{}]",
                            tunnel_idx,
                            e
                        );
                        failed_tunnels.push((tunnel_idx, e.to_string()));
                    } else if tunnel_idx < tunnel_activity.len() {
                        tunnel_activity[tunnel_idx] = Some(Instant::now());
                    }
                }
                for (tunnel_idx, reason) in failed_tunnels {
                    if inbound_tx
                        .send(TunnelRecvEvent::Closed {
                            tunnel_idx,
                            reason: format!("heartbeat send failed [{reason}]"),
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                if inbound_tx.is_closed() {
                    break Err(anyhow::anyhow!("relay tunnel close event channel closed"));
                }
            }
        }
    };

    state_hub.refresh_runtime(
        target,
        &tunnels,
        &tunnel_states,
        &tunnel_activity,
        &src_to_flow,
    );
    if let Some(pending) = pending_tunnel_open.take() {
        pending.handle.abort();
    }
    for task in recv_tasks.into_iter().flatten() {
        task.abort();
    }
    result
}

#[derive(Debug)]
struct RelayTunnel {
    socket: Arc<UdpSocket>,
    codec: UdpRelayCodec,
}

#[derive(Debug, Clone, Copy)]
struct RelayTunnelState {
    allocatable: bool,
    expire_at: Instant,
    tunnel_id: u64,
    down_until: Option<Instant>,
}

impl RelayTunnelState {
    fn new(now: Instant, p2p_channel_lifetime: Duration, tunnel_id: u64) -> Self {
        Self {
            allocatable: true,
            expire_at: now + p2p_channel_lifetime,
            tunnel_id,
            down_until: None,
        }
    }

    fn down_marker(now: Instant, tunnel_id: u64) -> Self {
        Self {
            allocatable: false,
            expire_at: now,
            tunnel_id,
            down_until: Some(now + RELAY_TUNNEL_DOWN_TTL),
        }
    }
}

#[derive(Debug)]
struct TunnelInboundPacket {
    tunnel_idx: usize,
    flow_id: u64,
    payload: Vec<u8>,
}

#[derive(Debug)]
enum TunnelRecvEvent {
    Packet(TunnelInboundPacket),
    Heartbeat { tunnel_idx: usize },
    Closed { tunnel_idx: usize, reason: String },
}

fn cleanup_client_flows(
    state_hub: &RelayStateHub,
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
        if let Some(flow) = src_to_flow.get(&src) {
            state_hub.emit(RelayLifecycleEvent::FlowClosed {
                flow_id,
                src,
                tunnel_idx: flow.tunnel_idx,
                tunnel_id: Some(flow.tunnel_id),
                reason: format!("idle-timeout {}s", idle_elapsed.as_secs()),
            });
        }
        src_to_flow.remove(&src);
        flow_to_src.remove(&flow_id);
    }
}

fn next_nonzero_flow_id(curr: u64) -> u64 {
    let mut next =
        (curr & UDP_RELAY_FLOW_ID_MASK_OBFS).wrapping_add(1) & UDP_RELAY_FLOW_ID_MASK_OBFS;
    if next == 0 {
        next = 1;
    }
    next
}

fn alloc_next_flow_id(
    next_flow: &mut u64,
    flow_to_src: &HashMap<u64, SocketAddr>,
) -> Result<u64> {
    let start = *next_flow;
    loop {
        let flow_id = *next_flow;
        *next_flow = next_nonzero_flow_id(*next_flow);
        if !flow_to_src.contains_key(&flow_id) {
            return Ok(flow_id);
        }
        if *next_flow == start {
            bail!("no available relay flow id");
        }
    }
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
    blocked_agents: &HashSet<String>,
) -> Result<AgentInfo> {
    let mut agents = query_candidate_agents(signal_url, agent_regex, quic_insecure).await?;
    if !blocked_agents.is_empty() {
        agents.retain(|x| !blocked_agents.contains(&agent_instance_key(x)));
    }
    if agents.is_empty() {
        bail!("no matched agent");
    }
    Ok(agents.swap_remove(0))
}

async fn query_candidate_agents(
    signal_url: &url::Url,
    agent_regex: &Regex,
    quic_insecure: bool,
) -> Result<Vec<AgentInfo>> {
    let agents = if signal_url.scheme().eq_ignore_ascii_case("quic") {
        quic_signal::query_sessions_with_opts(signal_url, quic_insecure).await?
    } else {
        get_agents(signal_url).await?
    };
    let now_ms = Local::now().timestamp_millis() as u64;
    Ok(filter_and_sort_agents(agents, agent_regex, now_ms))
}

fn pick_latest_agent(
    mut agents: Vec<AgentInfo>,
    agent_regex: &Regex,
    now_ms: u64,
) -> Result<AgentInfo> {
    agents = filter_and_sort_agents(agents, agent_regex, now_ms);
    if agents.is_empty() {
        bail!("no matched agent");
    }

    Ok(agents.swap_remove(0))
}

fn filter_and_sort_agents(
    mut agents: Vec<AgentInfo>,
    agent_regex: &Regex,
    now_ms: u64,
) -> Vec<AgentInfo> {
    agents.retain(|x| agent_regex.is_match(&x.name) && x.expire_at > now_ms);
    agents.sort_by(cmp_agent_priority);
    agents
}

fn cmp_agent_priority(a: &AgentInfo, b: &AgentInfo) -> Ordering {
    b.expire_at
        .cmp(&a.expire_at)
        .then_with(|| cmp_instance_id_desc(a.instance_id.as_deref(), b.instance_id.as_deref()))
        .then_with(|| a.name.cmp(&b.name))
        .then_with(|| a.addr.cmp(&b.addr))
}

fn cmp_instance_id_desc(a: Option<&str>, b: Option<&str>) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => b.cmp(a),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
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

#[derive(Debug, Clone, Copy, clap::ValueEnum, PartialEq, Eq)]
enum AgentScriptFailPolicy {
    Ignore,
    SwitchAgent,
}

#[derive(Debug, Clone)]
enum RelayAgentScriptSource {
    Embedded { name: String },
    File { path: PathBuf },
}

#[derive(Debug, Clone)]
struct RelayAgentScriptSpec {
    source: RelayAgentScriptSource,
    content: Arc<[u8]>,
    argv: Vec<String>,
}

impl RelayAgentScriptSpec {
    fn display_name(&self) -> String {
        match &self.source {
            RelayAgentScriptSource::Embedded { name } => format!("embedded:{name}"),
            RelayAgentScriptSource::File { path } => format!("file:{}", path.display()),
        }
    }

    fn request_name(&self) -> String {
        match &self.source {
            RelayAgentScriptSource::Embedded { name } => format!("embedded:{name}"),
            RelayAgentScriptSource::File { path } => {
                format!("file:{}", path.to_string_lossy())
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("agent script failed and fail-policy=switch-agent: {reason}")]
struct AgentScriptSwitchAgentError {
    reason: String,
}

#[derive(Debug, Clone)]
struct RelayAgentScriptCliSpec {
    source: RelayAgentScriptSource,
    argv: Vec<String>,
}

#[derive(Debug, Clone)]
struct RelayWorker {
    signal_url: url::Url,
    secret: Option<String>,
    quic_insecure: bool,
    agent_regex: Regex,
    idle_timeout: Duration,
    max_payload: usize,
    channel_pool: ChannelPoolConfig,
    p2p_channel_lifetime: Duration,
    agent_scripts: Arc<Vec<RelayAgentScriptSpec>>,
    agent_script_timeout: Duration,
    agent_script_fail_policy: AgentScriptFailPolicy,
    rule: RelayRule,
    state_hub: RelayStateHub,
}

#[derive(Debug, Clone, Copy)]
struct ChannelPoolConfig {
    min_channels: usize,
    max_channels: usize,
}

impl ChannelPoolConfig {
    fn new(min_channels: usize, max_channels: usize) -> Result<Self> {
        if max_channels == 0 {
            bail!("p2p max channels must be >= 1");
        }
        if min_channels > max_channels {
            bail!(
                "p2p min channels [{}] must be <= max channels [{}]",
                min_channels,
                max_channels
            );
        }
        Ok(Self {
            min_channels,
            max_channels,
        })
    }

    fn desired_channels(self, active_flows: usize) -> usize {
        if active_flows == 0 {
            self.min_channels
        } else {
            self.max_channels
        }
    }
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

fn load_agent_scripts(args: &CmdArgs) -> Result<Vec<RelayAgentScriptSpec>> {
    let ordered_specs = collect_ordered_script_specs(args)?;
    let mut scripts = Vec::with_capacity(ordered_specs.len());
    for cli_spec in ordered_specs {
        let content: Arc<[u8]> = match &cli_spec.source {
            RelayAgentScriptSource::Embedded { name } => embedded_agent_script(name)
                .with_context(|| format!("unknown embedded agent script [{name}]"))?,
            RelayAgentScriptSource::File { path } => std::fs::read(path)
                .with_context(|| format!("read agent script file failed [{}]", path.display()))?
                .into_boxed_slice()
                .into(),
        };
        scripts.push(RelayAgentScriptSpec {
            source: cli_spec.source,
            content,
            argv: cli_spec.argv,
        });
    }
    Ok(scripts)
}

fn embedded_agent_script(name: &str) -> Result<Arc<[u8]>> {
    let script = match name {
        "bootstrap_env" => Some(include_bytes!("embedded_scripts/bootstrap_env.sh").as_slice()),
        "hy2" => Some(include_bytes!("embedded_scripts/hy2.sh").as_slice()),
        "noop" => Some(include_bytes!("embedded_scripts/noop.sh").as_slice()),
        _ => None,
    }
    .with_context(|| format!("unknown embedded script [{name}]"))?;
    Ok(script.to_vec().into_boxed_slice().into())
}

fn collect_ordered_script_specs(args: &CmdArgs) -> Result<Vec<RelayAgentScriptCliSpec>> {
    let expected = args.agent_scripts.len() + args.agent_script_files.len();
    if expected == 0 {
        if !args.agent_script_arg.is_empty() || !args.agent_script_args.is_empty() {
            bail!("--agent-script-arg/--agent-script-args requires --agent-script or --agent-script-file");
        }
        return Ok(Vec::new());
    }

    let parsed = parse_script_specs_from_argv()?;
    if parsed.len() == expected {
        return Ok(parsed);
    }

    if !args.agent_script_arg.is_empty() || !args.agent_script_args.is_empty() {
        bail!(
            "failed to map --agent-script-arg/--agent-script-args to scripts: expected {} script items, parsed {} from argv",
            expected,
            parsed.len()
        );
    }

    let mut fallback = Vec::with_capacity(expected);
    for name in &args.agent_scripts {
        fallback.push(RelayAgentScriptCliSpec {
            source: RelayAgentScriptSource::Embedded { name: name.clone() },
            argv: Vec::new(),
        });
    }
    for path in &args.agent_script_files {
        fallback.push(RelayAgentScriptCliSpec {
            source: RelayAgentScriptSource::File { path: path.clone() },
            argv: Vec::new(),
        });
    }
    Ok(fallback)
}

fn parse_script_specs_from_argv() -> Result<Vec<RelayAgentScriptCliSpec>> {
    let argv: Vec<String> = std::env::args().collect();
    parse_script_specs_from_args(argv.iter().map(|x| x.as_str()))
}

fn parse_script_specs_from_args<'a, I>(args: I) -> Result<Vec<RelayAgentScriptCliSpec>>
where
    I: IntoIterator<Item = &'a str>,
{
    let argv: Vec<String> = args.into_iter().map(|x| x.to_string()).collect();
    let mut specs = Vec::new();
    let mut idx = 0_usize;
    while idx < argv.len() {
        let arg = argv[idx].as_str();
        if arg == "--agent-script" {
            let v = argv
                .get(idx + 1)
                .with_context(|| "missing value for --agent-script")?;
            specs.push(RelayAgentScriptCliSpec {
                source: RelayAgentScriptSource::Embedded { name: v.clone() },
                argv: Vec::new(),
            });
            idx += 2;
            continue;
        }
        if let Some(v) = arg.strip_prefix("--agent-script=") {
            specs.push(RelayAgentScriptCliSpec {
                source: RelayAgentScriptSource::Embedded {
                    name: v.to_string(),
                },
                argv: Vec::new(),
            });
            idx += 1;
            continue;
        }
        if arg == "--agent-script-file" {
            let v = argv
                .get(idx + 1)
                .with_context(|| "missing value for --agent-script-file")?;
            specs.push(RelayAgentScriptCliSpec {
                source: RelayAgentScriptSource::File {
                    path: PathBuf::from(v),
                },
                argv: Vec::new(),
            });
            idx += 2;
            continue;
        }
        if let Some(v) = arg.strip_prefix("--agent-script-file=") {
            specs.push(RelayAgentScriptCliSpec {
                source: RelayAgentScriptSource::File {
                    path: PathBuf::from(v),
                },
                argv: Vec::new(),
            });
            idx += 1;
            continue;
        }
        if arg == "--agent-script-arg" {
            let v = argv
                .get(idx + 1)
                .with_context(|| "missing value for --agent-script-arg")?;
            append_single_script_arg(specs.as_mut_slice(), v.to_string(), "--agent-script-arg")?;
            idx += 2;
            continue;
        }
        if let Some(v) = arg.strip_prefix("--agent-script-arg=") {
            append_single_script_arg(specs.as_mut_slice(), v.to_string(), "--agent-script-arg")?;
            idx += 1;
            continue;
        }
        if arg == "--agent-script-args" {
            let v = argv
                .get(idx + 1)
                .with_context(|| "missing value for --agent-script-args")?;
            append_shell_script_args(specs.as_mut_slice(), v, "--agent-script-args")?;
            idx += 2;
            continue;
        }
        if let Some(v) = arg.strip_prefix("--agent-script-args=") {
            append_shell_script_args(specs.as_mut_slice(), v, "--agent-script-args")?;
            idx += 1;
            continue;
        }
        idx += 1;
    }
    Ok(specs)
}

fn append_single_script_arg(
    specs: &mut [RelayAgentScriptCliSpec],
    value: String,
    flag: &str,
) -> Result<()> {
    let curr = specs.last_mut().with_context(|| {
        format!("{flag} must appear after --agent-script or --agent-script-file")
    })?;
    curr.argv.push(value);
    Ok(())
}

fn append_shell_script_args(
    specs: &mut [RelayAgentScriptCliSpec],
    raw: &str,
    flag: &str,
) -> Result<()> {
    let curr = specs.last_mut().with_context(|| {
        format!("{flag} must appear after --agent-script or --agent-script-file")
    })?;
    let parsed = shell_words::split(raw)
        .with_context(|| format!("parse {flag} failed for input [{}]", raw))?;
    curr.argv.extend(parsed);
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct ClientFlow {
    flow_id: u64,
    updated_at: Instant,
    route_updated_at: Instant,
    tunnel_idx: usize,
    tunnel_id: u64,
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
        long = "tui",
        long_help = "enable relay tui mode, suppress console logs"
    )]
    tui: bool,

    #[clap(
        long = "log-file",
        long_help = "write relay logs to file path (disabled when not set)"
    )]
    log_file: Option<PathBuf>,

    #[clap(
        long = "agent-script",
        long_help = "embedded agent script name, repeatable"
    )]
    agent_scripts: Vec<String>,

    #[clap(
        long = "agent-script-file",
        long_help = "agent script file path, repeatable"
    )]
    agent_script_files: Vec<PathBuf>,

    #[clap(
        long = "agent-script-arg",
        long_help = "single argument passed to current script, repeatable"
    )]
    agent_script_arg: Vec<String>,

    #[clap(
        long = "agent-script-args",
        long_help = "shell-style arguments string passed to current script, repeatable"
    )]
    agent_script_args: Vec<String>,

    #[clap(
        long = "agent-script-timeout",
        long_help = "single agent script timeout in seconds",
        default_value_t = DEFAULT_AGENT_SCRIPT_TIMEOUT_SECS
    )]
    agent_script_timeout: u64,

    #[clap(
        long = "agent-script-fail-policy",
        long_help = "agent script failure policy",
        value_enum,
        default_value_t = AgentScriptFailPolicy::Ignore
    )]
    agent_script_fail_policy: AgentScriptFailPolicy,

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

    #[clap(
        long = "p2p-min-channels",
        long_help = "min relay p2p channels",
        default_value_t = DEFAULT_P2P_MIN_CHANNELS
    )]
    p2p_min_channels: usize,

    #[clap(
        long = "p2p-max-channels",
        long_help = "max relay p2p channels",
        default_value_t = DEFAULT_P2P_MAX_CHANNELS
    )]
    p2p_max_channels: usize,

    #[clap(
        long = "p2p-channel-lifetime",
        long_help = "relay p2p channel lifetime in seconds before rotation",
        default_value_t = DEFAULT_P2P_CHANNEL_LIFETIME_SECS
    )]
    p2p_channel_lifetime: u64,
}

#[cfg(test)]
mod tests {
    use super::{
        decode_udp_relay_packet, duration_until_replacement_window_at, encode_udp_relay_packet,
        format_millis, max_udp_payload_auto, parse_script_specs_from_args, pick_best_tunnel_idx_by, pick_latest_agent,
        resolve_udp_max_payload, ChannelPoolConfig, RelayAgentScriptSource, RelayRule,
        UdpRelayCodec,
    };
    use crate::rest_proto::AgentInfo;
    use regex::Regex;
    use std::time::{Duration, Instant};

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
    fn format_millis_thresholds() {
        assert_eq!(format_millis(0), "<1s");
        assert_eq!(format_millis(999), "<1s");
        assert_eq!(format_millis(1_000), "1s");
        assert_eq!(format_millis(1_999), "1s");
        assert_eq!(format_millis(59_000), "59s");
    }

    #[test]
    fn format_millis_multi_units() {
        assert_eq!(format_millis(60_000), "1m0s");
        assert_eq!(format_millis(3_661_000), "1h1m1s");
        assert_eq!(format_millis(90_061_000), ">=24h");
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

    #[test]
    fn relay_packet_codec_obfs_tag_mismatch() {
        let mut buf = vec![0_u8; 64];
        let codec = UdpRelayCodec::new(123456789);
        let n = encode_udp_relay_packet(&mut buf, 12345, b"hello", codec).unwrap();
        buf[1] ^= 0x01;
        let err = decode_udp_relay_packet(&buf[..n], codec).unwrap_err();
        assert!(
            err.to_string().contains("tag mismatch"),
            "unexpected decode err: {err:?}"
        );
    }

    #[test]
    fn pick_latest_agent_prefers_expire_at() {
        let agents = vec![
            test_agent("a", "127.0.0.1:1001", 2_000, Some("i1")),
            test_agent("b", "127.0.0.1:1002", 3_000, Some("i2")),
        ];
        let regex = Regex::new(".*").unwrap();
        let selected = pick_latest_agent(agents, &regex, 1_500).unwrap();
        assert_eq!(selected.name, "b");
    }

    #[test]
    fn pick_latest_agent_filters_expired() {
        let agents = vec![
            test_agent("a", "127.0.0.1:1001", 1_000, Some("i1")),
            test_agent("b", "127.0.0.1:1002", 2_000, Some("i2")),
        ];
        let regex = Regex::new(".*").unwrap();
        let selected = pick_latest_agent(agents, &regex, 1_500).unwrap();
        assert_eq!(selected.name, "b");
    }

    #[test]
    fn pick_latest_agent_tiebreak_instance_id() {
        let agents = vec![
            test_agent("rtun", "127.0.0.1:1001", 2_000, Some("A1")),
            test_agent("rtun", "127.0.0.1:1002", 2_000, Some("Z9")),
        ];
        let regex = Regex::new("rtun").unwrap();
        let selected = pick_latest_agent(agents, &regex, 1_000).unwrap();
        assert_eq!(selected.instance_id.as_deref(), Some("Z9"));
    }

    #[test]
    fn pick_latest_agent_rejects_no_match() {
        let agents = vec![test_agent("a", "127.0.0.1:1001", 2_000, Some("i1"))];
        let regex = Regex::new("b").unwrap();
        let selected = pick_latest_agent(agents, &regex, 1_000);
        assert!(selected.is_err());
    }

    #[test]
    fn replacement_window_wait_when_expire_after_11_minutes() {
        let now_ms = 1_000_000_u64;
        let expire_at = now_ms + 11 * 60 * 1_000;
        let wait = duration_until_replacement_window_at(expire_at, Duration::from_secs(600), now_ms);
        assert_eq!(wait, Duration::from_secs(60));
    }

    #[test]
    fn replacement_window_wait_zero_when_expire_within_10_minutes() {
        let now_ms = 1_000_000_u64;
        let expire_at = now_ms + 9 * 60 * 1_000;
        let wait = duration_until_replacement_window_at(expire_at, Duration::from_secs(600), now_ms);
        assert_eq!(wait, Duration::from_secs(0));
    }

    #[test]
    fn channel_pool_config_validation() {
        assert!(ChannelPoolConfig::new(0, 1).is_ok());
        assert!(ChannelPoolConfig::new(1, 0).is_err());
        assert!(ChannelPoolConfig::new(2, 1).is_err());
        assert!(ChannelPoolConfig::new(1, 2).is_ok());
    }

    #[test]
    fn channel_pool_desired_channels() {
        let cfg = ChannelPoolConfig::new(1, 3).unwrap();
        assert_eq!(cfg.desired_channels(0), 1);
        assert_eq!(cfg.desired_channels(1), 3);
    }

    #[test]
    fn channel_pool_desired_respects_min_max() {
        let cfg = ChannelPoolConfig::new(2, 4).unwrap();
        assert_eq!(cfg.desired_channels(0), 2);
        assert_eq!(cfg.desired_channels(10), 4);
    }

    #[test]
    fn channel_pool_allows_zero_min_for_on_demand_open() {
        let cfg = ChannelPoolConfig::new(0, 3).unwrap();
        assert_eq!(cfg.desired_channels(0), 0);
        assert_eq!(cfg.desired_channels(1), 3);
    }

    #[test]
    fn pick_best_tunnel_prefers_fresh_and_low_load() {
        let now = Instant::now();
        let active = [true, true, true];
        let activity = vec![
            Some(now - Duration::from_secs(30)),
            Some(now - Duration::from_secs(1)),
            Some(now - Duration::from_secs(2)),
        ];
        let loads = [0_usize, 2, 0];
        let mut cursor = 0;

        let selected = pick_best_tunnel_idx_by(
            active.len(),
            |idx| active[idx],
            activity.as_slice(),
            loads.as_slice(),
            &mut cursor,
            now,
        );
        assert_eq!(selected, Some(2));
    }

    #[test]
    fn pick_best_tunnel_respects_cursor_tiebreak() {
        let now = Instant::now();
        let active = [true, true];
        let activity = vec![
            Some(now - Duration::from_secs(1)),
            Some(now - Duration::from_secs(1)),
        ];
        let loads = [1_usize, 1];
        let mut cursor = 1;

        let selected = pick_best_tunnel_idx_by(
            active.len(),
            |idx| active[idx],
            activity.as_slice(),
            loads.as_slice(),
            &mut cursor,
            now,
        );
        assert_eq!(selected, Some(1));
    }

    #[test]
    fn pick_best_tunnel_returns_none_when_no_active() {
        let now = Instant::now();
        let active = [false, false];
        let activity = vec![Some(now), Some(now)];
        let loads = [0_usize, 0];
        let mut cursor = 0;

        let selected = pick_best_tunnel_idx_by(
            active.len(),
            |idx| active[idx],
            activity.as_slice(),
            loads.as_slice(),
            &mut cursor,
            now,
        );
        assert_eq!(selected, None);
    }

    #[test]
    fn parse_script_specs_should_preserve_cli_order_and_bind_args() {
        let parsed = parse_script_specs_from_args([
            "rtun",
            "relay",
            "--agent-script",
            "bootstrap_env",
            "--agent-script-arg",
            "--force_dl=true",
            "--agent-script-args",
            "--version=v2.7.0 --listen=127.0.0.1:4433",
            "--agent-script-file",
            "./scripts/a.sh",
            "--agent-script=noop",
            "--agent-script-arg=--x=1",
            "--agent-script-file=./scripts/b.sh",
        ])
        .unwrap();
        assert_eq!(parsed.len(), 4);
        assert!(matches!(
            &parsed[0].source,
            RelayAgentScriptSource::Embedded { name } if name == "bootstrap_env"
        ));
        assert_eq!(
            parsed[0].argv,
            vec![
                "--force_dl=true".to_string(),
                "--version=v2.7.0".to_string(),
                "--listen=127.0.0.1:4433".to_string()
            ]
        );
        assert!(matches!(
            &parsed[1].source,
            RelayAgentScriptSource::File { path } if path == std::path::Path::new("./scripts/a.sh")
        ));
        assert!(parsed[1].argv.is_empty());
        assert!(matches!(
            &parsed[2].source,
            RelayAgentScriptSource::Embedded { name } if name == "noop"
        ));
        assert_eq!(parsed[2].argv, vec!["--x=1".to_string()]);
        assert!(matches!(
            &parsed[3].source,
            RelayAgentScriptSource::File { path } if path == std::path::Path::new("./scripts/b.sh")
        ));
        assert!(parsed[3].argv.is_empty());
    }

    #[test]
    fn parse_script_specs_should_reject_args_before_script() {
        let err = parse_script_specs_from_args([
            "rtun",
            "relay",
            "--agent-script-args",
            "--version=v2.7.0",
        ])
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains(
                "--agent-script-args must appear after --agent-script or --agent-script-file"
            ),
            "{msg}"
        );
    }

    #[test]
    fn parse_script_specs_should_report_invalid_shell_words() {
        let err = parse_script_specs_from_args([
            "rtun",
            "relay",
            "--agent-script",
            "hy2",
            "--agent-script-args",
            "\"unterminated",
        ])
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("parse --agent-script-args failed"), "{msg}");
    }

    #[test]
    fn embedded_script_lookup_should_validate_name() {
        assert!(super::embedded_agent_script("bootstrap_env").is_ok());
        assert!(super::embedded_agent_script("hy2").is_ok());
        assert!(super::embedded_agent_script("noop").is_ok());
        assert!(super::embedded_agent_script("not_found").is_err());
    }

    fn test_agent(name: &str, addr: &str, expire_at: u64, instance_id: Option<&str>) -> AgentInfo {
        AgentInfo {
            name: name.to_string(),
            addr: addr.to_string(),
            expire_at,
            instance_id: instance_id.map(|x| x.to_string()),
            ver: None,
        }
    }
}
