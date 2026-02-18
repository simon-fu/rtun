use std::{
    cmp::Ordering,
    collections::HashMap,
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
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use regex::Regex;
use rtun::{
    async_rt::{run_multi_thread, spawn_with_name},
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
const DEFAULT_P2P_PACKET_LIMIT: usize = 1400;
const UDP_RELAY_META_LEN_LEGACY: usize = 8;
const UDP_RELAY_META_LEN_OBFS: usize = 9;
const UDP_RELAY_FLOW_ID_MASK: u64 = (1_u64 << 48) - 1;
const LOOP_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const FLOW_CLEANUP_INTERVAL: Duration = Duration::from_secs(1);
const UDP_RELAY_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(3);
const UDP_RELAY_HEARTBEAT_FLOW_ID: u64 = 0;
const DEFAULT_P2P_MIN_CHANNELS: usize = 1;
const DEFAULT_P2P_MAX_CHANNELS: usize = 1;
const DEFAULT_P2P_CHANNEL_LIFETIME_SECS: u64 = 120;
const P2P_EXPAND_CONNECT_TIMEOUT_INITIAL: Duration = Duration::from_secs(5);
const P2P_EXPAND_CONNECT_TIMEOUT_MAX: Duration = Duration::from_secs(30);
const FLOW_ROUTE_RESELECT_INTERVAL: Duration = Duration::from_secs(5);
const FLOW_MIGRATE_STALE_GAP: Duration = Duration::from_secs(6);
const FLOW_MIGRATE_LOAD_GAP: usize = 2;
const TUNNEL_STALE_THRESHOLD: Duration = Duration::from_secs(12);
const RELAY_TUI_REFRESH_INTERVAL: Duration = Duration::from_millis(300);
const RELAY_TUI_MAX_EVENT_LINES: usize = 8;

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
        mode: String,
        source: &'static str,
    },
    TunnelRotated {
        old_tunnel_idx: usize,
        new_tunnel_idx: usize,
    },
    TunnelClosed {
        tunnel_idx: usize,
        reason: String,
    },
    FlowCreated {
        flow_id: u64,
        src: SocketAddr,
        target: SocketAddr,
        tunnel_idx: usize,
    },
    FlowMigrated {
        flow_id: u64,
        src: SocketAddr,
        old_tunnel_idx: usize,
        new_tunnel_idx: usize,
        reason: &'static str,
    },
    FlowClosed {
        flow_id: u64,
        src: SocketAddr,
        tunnel_idx: usize,
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
    idle_for_ms: u64,
    route_age_ms: u64,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
struct RelayStateSnapshot {
    listen: SocketAddr,
    target: SocketAddr,
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
    fn new(listen: SocketAddr, target: SocketAddr) -> Self {
        let (event_tx, _) = broadcast::channel(1024);
        let snapshot = RelayStateSnapshot {
            listen,
            target,
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
            if !alive && state.is_none() && activity.is_none() {
                continue;
            }

            let allocatable = state.is_some_and(|x| x.allocatable);
            let expires_in_ms =
                state.map(|x| x.expire_at.saturating_duration_since(now).as_millis() as u64);
            let last_active_ago_ms =
                activity.map(|x| now.saturating_duration_since(x).as_millis() as u64);
            let mode = tunnels
                .get(tunnel_idx)
                .and_then(|x| x.as_ref())
                .map(|x| x.codec.mode_name().to_string());
            let flow_count = flow_loads.get(tunnel_idx).copied().unwrap_or(0);

            tunnel_snaps.push(RelayTunnelSnapshot {
                tunnel_idx,
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
    rule: String,
    name: String,
    instance_id: String,
    addr: String,
    expire_at: u64,
    connected: bool,
}

#[derive(Debug)]
struct RelayTuiTunnelRow {
    rule: String,
    tunnel_idx: usize,
    mode: String,
    state: &'static str,
    flow_count: usize,
    last_active_ago_ms: Option<u64>,
    expires_in_ms: Option<u64>,
}

#[derive(Debug)]
struct RelayTuiFlowRow {
    rule: String,
    flow_id: u64,
    src: SocketAddr,
    target: SocketAddr,
    tunnel_idx: usize,
    idle_for_ms: u64,
    route_age_ms: u64,
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
    while recent_events.len() > RELAY_TUI_MAX_EVENT_LINES {
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
            mode,
            source,
        } => {
            format!("tunnel opened: idx={tunnel_idx} mode={mode} source={source}")
        }
        RelayLifecycleEvent::TunnelRotated {
            old_tunnel_idx,
            new_tunnel_idx,
        } => {
            format!("tunnel rotated: {old_tunnel_idx} -> {new_tunnel_idx}")
        }
        RelayLifecycleEvent::TunnelClosed { tunnel_idx, reason } => {
            format!("tunnel closed: idx={tunnel_idx} reason={reason}")
        }
        RelayLifecycleEvent::FlowCreated {
            flow_id,
            src,
            target,
            tunnel_idx,
        } => {
            format!("flow created: id={flow_id} src={src} target={target} tunnel={tunnel_idx}")
        }
        RelayLifecycleEvent::FlowMigrated {
            flow_id,
            src,
            old_tunnel_idx,
            new_tunnel_idx,
            reason,
        } => {
            format!(
                "flow migrated: id={flow_id} src={src} tunnel={old_tunnel_idx}->{new_tunnel_idx} reason={reason}"
            )
        }
        RelayLifecycleEvent::FlowClosed {
            flow_id,
            src,
            tunnel_idx,
            reason,
        } => {
            format!("flow closed: id={flow_id} src={src} tunnel={tunnel_idx} reason={reason}")
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

    let mut agent_rows = Vec::<RelayTuiAgentRow>::new();
    let mut tunnel_rows = Vec::<RelayTuiTunnelRow>::new();
    let mut flow_rows = Vec::<RelayTuiFlowRow>::new();

    for snapshot in snapshots.iter() {
        let rule = format!("{}=>{}", snapshot.listen, snapshot.target);
        if let Some(agent) = snapshot.selected_agent.as_ref() {
            agent_rows.push(RelayTuiAgentRow {
                rule: rule.clone(),
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
                rule: rule.clone(),
                tunnel_idx: tunnel.tunnel_idx,
                mode: tunnel.mode.clone().unwrap_or_else(|| "-".to_string()),
                state,
                flow_count: tunnel.flow_count,
                last_active_ago_ms: tunnel.last_active_ago_ms,
                expires_in_ms: tunnel.expires_in_ms,
            });
        }

        for flow in snapshot.flows.iter() {
            flow_rows.push(RelayTuiFlowRow {
                rule: rule.clone(),
                flow_id: flow.flow_id,
                src: flow.src,
                target: flow.target,
                tunnel_idx: flow.tunnel_idx,
                idle_for_ms: flow.idle_for_ms,
                route_age_ms: flow.route_age_ms,
            });
        }
    }

    agent_rows.sort_by(|a, b| {
        b.expire_at
            .cmp(&a.expire_at)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.rule.cmp(&b.rule))
    });
    tunnel_rows.sort_by(|a, b| {
        b.flow_count
            .cmp(&a.flow_count)
            .then_with(|| a.tunnel_idx.cmp(&b.tunnel_idx))
            .then_with(|| a.rule.cmp(&b.rule))
    });
    flow_rows.sort_by(|a, b| {
        a.idle_for_ms
            .cmp(&b.idle_for_ms)
            .then_with(|| a.flow_id.cmp(&b.flow_id))
            .then_with(|| a.rule.cmp(&b.rule))
    });

    let mut lines = Vec::<String>::new();
    lines.push(format!(
        "rtun relay tui  {}  q/esc:quit  r:refresh  rules:{} agents:{} tunnels:{} flows:{}",
        Local::now().format("%H:%M:%S"),
        snapshots.len(),
        agent_rows.len(),
        tunnel_rows.len(),
        flow_rows.len()
    ));
    lines.push(String::new());

    lines.push("Agents".to_string());
    lines.push("rule | name | instance | addr | expire_in | connected".to_string());
    if agent_rows.is_empty() {
        lines.push("-".to_string());
    } else {
        for row in agent_rows.iter().take(12) {
            let expire_in = row.expire_at.saturating_sub(now_ms) / 1000;
            lines.push(format!(
                "{} | {} | {} | {} | {}s | {}",
                row.rule,
                row.name,
                row.instance_id,
                row.addr,
                expire_in,
                if row.connected { "yes" } else { "no" }
            ));
        }
        if agent_rows.len() > 12 {
            lines.push(format!("... {} more agents", agent_rows.len() - 12));
        }
    }
    lines.push(String::new());

    lines.push("Tunnels".to_string());
    lines.push("rule | idx | mode | state | flows | last_active | expires_in".to_string());
    if tunnel_rows.is_empty() {
        lines.push("-".to_string());
    } else {
        for row in tunnel_rows.iter().take(24) {
            lines.push(format!(
                "{} | {} | {} | {} | {} | {} | {}",
                row.rule,
                row.tunnel_idx,
                row.mode,
                row.state,
                row.flow_count,
                format_millis_ago(row.last_active_ago_ms),
                format_millis_in(row.expires_in_ms),
            ));
        }
        if tunnel_rows.len() > 24 {
            lines.push(format!("... {} more tunnels", tunnel_rows.len() - 24));
        }
    }
    lines.push(String::new());

    lines.push("Flows".to_string());
    lines.push("rule | flow_id | src | target | tunnel | idle | route_age".to_string());
    if flow_rows.is_empty() {
        lines.push("-".to_string());
    } else {
        for row in flow_rows.iter().take(32) {
            lines.push(format!(
                "{} | {} | {} | {} | {} | {} | {}",
                row.rule,
                row.flow_id,
                row.src,
                row.target,
                row.tunnel_idx,
                format_millis_ago(Some(row.idle_for_ms)),
                format_millis_ago(Some(row.route_age_ms)),
            ));
        }
        if flow_rows.len() > 32 {
            lines.push(format!("... {} more flows", flow_rows.len() - 32));
        }
    }

    if !recent_events.is_empty() {
        lines.push(String::new());
        lines.push("Recent Events".to_string());
        for line in recent_events.iter() {
            lines.push(line.clone());
        }
    }

    let mut render_lines = lines;
    if height > 0 && render_lines.len() > height {
        render_lines.truncate(height.saturating_sub(1));
        render_lines.push("...".to_string());
    }

    execute!(stdout, MoveTo(0, 0), Clear(ClearType::All))?;
    for line in render_lines.iter() {
        writeln!(stdout, "{}", clip_line_width(line, width))?;
    }
    stdout.flush()?;
    Ok(())
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
    if ms >= 60_000 {
        let minutes = ms / 60_000;
        let seconds = (ms % 60_000) / 1_000;
        format!("{minutes}m{seconds}s")
    } else if ms >= 1_000 {
        let seconds = ms / 1_000;
        let millis = ms % 1_000;
        format!("{seconds}.{millis:03}s")
    } else {
        format!("{ms}ms")
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
        let state_hub = RelayStateHub::new(rule.listen, rule.target);
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
    loop {
        let selected = match next_agent_hint.take() {
            Some(agent) => Ok(agent),
            None => {
                select_latest_agent(
                    &worker.signal_url,
                    &worker.agent_regex,
                    worker.quic_insecure,
                )
                .await
            }
        };

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
        worker.state_hub.set_selected_agent(&selected, false);
        worker.state_hub.emit(RelayLifecycleEvent::AgentSelected {
            name: selected.name.clone(),
            instance_id: selected.instance_id.clone(),
            addr: selected.addr.clone(),
            expire_at: selected.expire_at,
        });

        let (stop_tx, mut session_task) =
            spawn_relay_session_task(worker.clone(), local.clone(), selected.clone());
        let expire_wait = duration_until_expire(selected.expire_at);

        tokio::select! {
            r = &mut session_task => {
                handle_session_task_result(&worker.state_hub, &selected, r);
            }
            _ = time::sleep(expire_wait) => {
                tracing::info!(
                    "current agent reached expire_at, preparing replacement: agent [{}], instance [{:?}]",
                    selected.name,
                    selected.instance_id
                );

                match wait_replacement_agent_ready(&worker, &selected, &session_task).await {
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
                        handle_session_task_result(&worker.state_hub, &selected, r);
                        next_agent_hint = Some(next_agent);
                    }
                    None => {
                        let r = session_task.await;
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
) -> Option<AgentInfo> {
    loop {
        if session_task.is_finished() {
            tracing::warn!("current session closed while waiting replacement");
            return None;
        }

        match find_connectable_replacement_agent(worker, current).await {
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

fn duration_until_expire(expire_at: u64) -> Duration {
    let now_ms = Local::now().timestamp_millis() as u64;
    Duration::from_millis(expire_at.saturating_sub(now_ms))
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
        )));
        tunnel_activity.push(Some(Instant::now()));
        state_hub.emit(RelayLifecycleEvent::TunnelOpened {
            tunnel_idx,
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
        loop {
            let n = match socket.recv(&mut tun_buf).await {
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
                    let _ = inbound_tx
                        .send(TunnelRecvEvent::Closed {
                            tunnel_idx,
                            reason: format!("decode failed [{e}]"),
                        })
                        .await;
                    break;
                }
            };

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
    inbound_tx: &mpsc::Sender<TunnelRecvEvent>,
    connect_timeout: &mut Duration,
) {
    while active_tunnel_count(tunnels) < desired {
        let tunnel_idx = match first_inactive_tunnel_idx(tunnels) {
            Some(idx) => idx,
            None => tunnels.len(),
        };
        let timeout = *connect_timeout;
        match time::timeout(timeout, open_udp_relay_tunnel(ctrl, target, idle_timeout_secs, max_payload))
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
                    )));
                    tunnel_activity.push(Some(Instant::now()));
                } else {
                    tunnels[tunnel_idx] = Some(tunnel);
                    recv_tasks[tunnel_idx] = Some(recv_task);
                    tunnel_states[tunnel_idx] =
                        Some(RelayTunnelState::new(Instant::now(), p2p_channel_lifetime));
                    tunnel_activity[tunnel_idx] = Some(Instant::now());
                }
                state_hub.emit(RelayLifecycleEvent::TunnelOpened {
                    tunnel_idx,
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
    while tunnels.last().is_some_and(|x| x.is_none()) {
        tunnels.pop();
        recv_tasks.pop();
        tunnel_states.pop();
        tunnel_activity.pop();
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
                new_tunnel_idx: best_idx,
                reason: "rebalance",
            });
            flow.tunnel_idx = best_idx;
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
        match time::timeout(timeout, open_udp_relay_tunnel(ctrl, target, idle_timeout_secs, max_payload))
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
                    tunnel_states.push(Some(RelayTunnelState::new(now, p2p_channel_lifetime)));
                    tunnel_activity.push(Some(now));
                } else {
                    tunnels[new_idx] = Some(new_tunnel);
                    recv_tasks[new_idx] = Some(recv_task);
                    tunnel_states[new_idx] = Some(RelayTunnelState::new(now, p2p_channel_lifetime));
                    tunnel_activity[new_idx] = Some(now);
                }

                if let Some(old_state) = tunnel_states.get_mut(old_idx).and_then(|x| x.as_mut()) {
                    old_state.allocatable = false;
                }
                tracing::info!(
                    "relay tunnel rotated: old_idx [{}] -> new_idx [{}], old enters draining",
                    old_idx,
                    new_idx
                );
                state_hub.emit(RelayLifecycleEvent::TunnelOpened {
                    tunnel_idx: new_idx,
                    mode,
                    source: "rotate",
                });
                state_hub.emit(RelayLifecycleEvent::TunnelRotated {
                    old_tunnel_idx: old_idx,
                    new_tunnel_idx: new_idx,
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
        tunnels[tunnel_idx] = None;
        tunnel_states[tunnel_idx] = None;
        tunnel_activity[tunnel_idx] = None;
        tracing::info!("relay tunnel closed(drained): idx [{}]", tunnel_idx);
        state_hub.emit(RelayLifecycleEvent::TunnelClosed {
            tunnel_idx,
            reason: "drained".to_string(),
        });
    }
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
    inbound_tx: mpsc::Sender<TunnelRecvEvent>,
    mut inbound_rx: mpsc::Receiver<TunnelRecvEvent>,
) -> Result<()> {
    if active_tunnel_count(&tunnels) == 0 {
        bail!("no relay tunnel available");
    }
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

    let mut cleanup = time::interval(FLOW_CLEANUP_INTERVAL);
    cleanup.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut heartbeat = time::interval(UDP_RELAY_HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let result: Result<()> = loop {
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
                            new_tunnel_idx: new_idx,
                            reason: "rebound",
                        });
                        flow.tunnel_idx = new_idx;
                        flow.route_updated_at = now;
                    }
                    (flow.flow_id, flow.tunnel_idx)
                } else {
                    if active_tunnel_count(&tunnels) == 0 {
                        break Err(anyhow::anyhow!("no relay tunnel available"));
                    }
                    let flow_id = next_flow;
                    next_flow = next_nonzero_flow_id(next_flow);
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
            evt = inbound_rx.recv() => {
                let Some(evt) = evt else {
                    break Err(anyhow::anyhow!("relay tunnel recv loop channel closed"));
                };
                match evt {
                    TunnelRecvEvent::Packet(pkt) => {
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
                        tunnel_states[tunnel_idx] = None;
                        tunnel_activity[tunnel_idx] = None;
                        tracing::warn!(
                            "relay tunnel closed: idx [{}], reason [{}], active={}",
                            tunnel_idx,
                            reason,
                            active_tunnel_count(&tunnels)
                        );
                        state_hub.emit(RelayLifecycleEvent::TunnelClosed {
                            tunnel_idx,
                            reason: reason.clone(),
                        });

                        let mut removed = Vec::new();
                        for (src, flow) in src_to_flow.iter() {
                            if flow.tunnel_idx == tunnel_idx {
                                removed.push((*src, flow.flow_id));
                            }
                        }
                        for (src, flow_id) in removed {
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
                                reason: "tunnel-lost".to_string(),
                            });
                        }

                        let desired = channel_pool.desired_channels(src_to_flow.len());
                        try_expand_tunnels(
                            &ctrl,
                            state_hub,
                            target,
                            idle_timeout_secs,
                            max_payload,
                            p2p_channel_lifetime,
                            desired,
                            &mut tunnels,
                            &mut recv_tasks,
                            &mut tunnel_states,
                            &mut tunnel_activity,
                            &inbound_tx,
                            &mut p2p_expand_connect_timeout,
                        )
                        .await;

                        if active_tunnel_count(&tunnels) == 0 {
                            break Err(anyhow::anyhow!("all relay tunnels are closed"));
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
                if active_tunnel_count(&tunnels) < desired {
                    try_expand_tunnels(
                        &ctrl,
                        state_hub,
                        target,
                        idle_timeout_secs,
                        max_payload,
                        p2p_channel_lifetime,
                        desired,
                        &mut tunnels,
                        &mut recv_tasks,
                        &mut tunnel_states,
                        &mut tunnel_activity,
                        &inbound_tx,
                        &mut p2p_expand_connect_timeout,
                    )
                    .await;
                }
                try_rotate_expired_tunnels(
                    &ctrl,
                    state_hub,
                    target,
                    idle_timeout_secs,
                    max_payload,
                    p2p_channel_lifetime,
                    &mut tunnels,
                    &mut recv_tasks,
                    &mut tunnel_states,
                    &mut tunnel_activity,
                    &inbound_tx,
                    &mut p2p_expand_connect_timeout,
                )
                .await;
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
}

impl RelayTunnelState {
    fn new(now: Instant, p2p_channel_lifetime: Duration) -> Self {
        Self {
            allocatable: true,
            expire_at: now + p2p_channel_lifetime,
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
                reason: format!("idle-timeout {}s", idle_elapsed.as_secs()),
            });
        }
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
    let mut agents = query_candidate_agents(signal_url, agent_regex, quic_insecure).await?;
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
    channel_pool: ChannelPoolConfig,
    p2p_channel_lifetime: Duration,
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
        if min_channels == 0 {
            bail!("p2p min channels must be >= 1");
        }
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

#[derive(Debug, Clone, Copy)]
struct ClientFlow {
    flow_id: u64,
    updated_at: Instant,
    route_updated_at: Instant,
    tunnel_idx: usize,
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
        decode_udp_relay_packet, encode_udp_relay_packet, max_udp_payload_auto,
        pick_best_tunnel_idx_by, pick_latest_agent, resolve_udp_max_payload, ChannelPoolConfig,
        RelayRule, UdpRelayCodec,
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
    fn channel_pool_config_validation() {
        assert!(ChannelPoolConfig::new(0, 1).is_err());
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
