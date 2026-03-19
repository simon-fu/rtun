use std::{
    collections::HashMap,
    net::SocketAddr,
    time::Duration,
};

use anyhow::{Context, Result};
use clap::Parser;
use tokio::net::UdpSocket;
use tracing::info;

use super::udp_perf::{
    UdpPerfControlPacket, UdpPerfDataHeader, UdpPerfDataPacket, UdpPerfDirection, UdpPerfMode,
    UdpPerfReport, UdpPerfStart, UdpPerfStats, UdpPerfWirePacket,
};

pub async fn run(args: CmdArgs) -> Result<()> {
    let listen: SocketAddr = args.listen.parse().with_context(|| {
        format!("invalid --listen value [{}], expected ip:port", args.listen)
    })?;
    let socket = UdpSocket::bind(listen)
        .await
        .with_context(|| format!("udp-server bind failed at [{listen}]"))?;
    let local_addr = socket.local_addr().with_context(|| "get local_addr failed")?;
    info!(
        "udp bench server listen [{}], report interval [{}ms]",
        local_addr, args.interval
    );

    // For CLI, run forever. Tests provide shutdown channel.
    udp_server_loop(socket, Duration::from_millis(args.interval), None).await
}

#[derive(Parser, Debug)]
#[clap(name = "udp-server", author, about, version)]
pub struct CmdArgs {
    #[clap(long = "listen", default_value = "0.0.0.0:9001")]
    pub listen: String,

    #[clap(long = "interval", default_value_t = 1000)]
    pub interval: u64,
}

#[derive(Debug)]
struct Session {
    peer: SocketAddr,
    mode: UdpPerfMode,
    started_at: tokio::time::Instant,
    last_report_at: tokio::time::Instant,
    report_interval: Duration,
    next_report_at: tokio::time::Instant,
    report_seq: u64,
    stats: UdpPerfStats,

    // Send loop config (reverse/bidir only).
    send_payload_len: usize,
    send_pps: u64,
    send_seq: u64,
    send_end_at: Option<tokio::time::Instant>,
    next_send_at: Option<tokio::time::Instant>,
}

impl Session {
    fn new(peer: SocketAddr, mode: UdpPerfMode, default_report_interval: Duration) -> Self {
        let now = tokio::time::Instant::now();
        Self {
            peer,
            mode,
            started_at: now,
            last_report_at: now,
            report_interval: default_report_interval,
            next_report_at: now + default_report_interval,
            report_seq: 0,
            stats: UdpPerfStats::default(),
            send_payload_len: 0,
            send_pps: 0,
            send_seq: 0,
            send_end_at: None,
            next_send_at: None,
        }
    }

    fn apply_start(&mut self, peer: SocketAddr, start: &UdpPerfStart, default_interval: Duration) {
        self.peer = peer;
        self.mode = start.mode;
        if start.report_interval_micros != 0 {
            self.report_interval = Duration::from_micros(start.report_interval_micros);
        } else {
            self.report_interval = default_interval;
        }

        let now = tokio::time::Instant::now();
        // Keep accumulated stats; only (re)start time on START.
        self.started_at = now;
        self.last_report_at = now;
        self.next_report_at = now + self.report_interval;
        self.report_seq = 0;
        self.stats.reset_interval();

        self.send_payload_len = start.payload_len as usize;
        self.send_pps = start.packets_per_second;
        self.send_seq = 0;
        self.send_end_at = Some(now + Duration::from_micros(start.duration_micros));

        match start.mode {
            UdpPerfMode::Forward => self.next_send_at = None,
            UdpPerfMode::Reverse | UdpPerfMode::Bidir => {
                // Send immediately.
                self.next_send_at = Some(now);
            }
        }
    }

    fn next_deadline(&self) -> Option<tokio::time::Instant> {
        match self.next_send_at {
            None => Some(self.next_report_at),
            Some(s) => Some(std::cmp::min(self.next_report_at, s)),
        }
    }

    fn should_send_now(&self, now: tokio::time::Instant) -> bool {
        match self.next_send_at {
            Some(t) => t <= now,
            None => false,
        }
    }

    fn should_report_now(&self, now: tokio::time::Instant) -> bool {
        self.next_report_at <= now
    }
}

async fn udp_server_loop(
    socket: UdpSocket,
    default_report_interval: Duration,
    mut shutdown: Option<tokio::sync::oneshot::Receiver<()>>,
) -> Result<()> {
    let mut sessions: HashMap<u64, Session> = HashMap::new();
    let mut buf = vec![0u8; 64 * 1024];

    loop {
        let next_deadline = sessions
            .values()
            .filter_map(|s| s.next_deadline())
            .min();

        let shutdown_fut = async {
            if let Some(rx) = shutdown.as_mut() {
                let _ = rx.await;
            } else {
                std::future::pending::<()>().await;
            }
        };

        let timer_fut = async {
            if let Some(t) = next_deadline {
                tokio::time::sleep_until(t).await;
            } else {
                std::future::pending::<()>().await;
            }
        };

        tokio::select! {
            biased;
            // Always prioritize shutdown for tests.
            _ = shutdown_fut => {
                break;
            }
            _ = timer_fut => {}
            recv_res = socket.recv_from(&mut buf) => {
                let (n, from) = recv_res.with_context(|| "udp recv_from failed")?;
                if let Err(e) = on_packet(&socket, &mut sessions, default_report_interval, &buf[..n], from).await {
                    // Bench server should be robust; ignore bad packets.
                    tracing::debug!("udp-server ignore packet error: {e:?}");
                }
            }
        }

        // Timer tick: advance due sessions (send first, then report).
        let now = tokio::time::Instant::now();
        advance_sessions(&socket, &mut sessions, now).await?;
    }

    Ok(())
}

async fn on_packet(
    socket: &UdpSocket,
    sessions: &mut HashMap<u64, Session>,
    default_report_interval: Duration,
    packet: &[u8],
    from: SocketAddr,
) -> Result<()> {
    let wire = UdpPerfWirePacket::decode(packet)?;
    match wire {
        UdpPerfWirePacket::Control(ctrl) => match ctrl {
            UdpPerfControlPacket::Hello(hello) => {
                let sess = sessions
                    .entry(hello.session_id)
                    .or_insert_with(|| Session::new(from, hello.mode, default_report_interval));
                sess.peer = from;
                sess.mode = hello.mode;
            }
            UdpPerfControlPacket::Start(start) => {
                let sess = sessions
                    .entry(start.session_id)
                    .or_insert_with(|| Session::new(from, start.mode, default_report_interval));
                sess.apply_start(from, &start, default_report_interval);
                // For reverse/bidir, send one packet immediately so client sees DATA before any REPORT.
                if matches!(start.mode, UdpPerfMode::Reverse | UdpPerfMode::Bidir) {
                    send_one(socket, start.session_id, sess, tokio::time::Instant::now()).await?;
                }
            }
            UdpPerfControlPacket::Stop(stop) => {
                if let Some(mut sess) = sessions.remove(&stop.session_id) {
                    send_final_report(socket, stop.session_id, &mut sess, tokio::time::Instant::now()).await?;
                }
            }
            UdpPerfControlPacket::Report(_) => {
                // client->server report is ignored.
            }
        },
        UdpPerfWirePacket::Data(data) => {
            if let Some(sess) = sessions.get_mut(&data.header.session_id) {
                sess.peer = from;
                sess.stats.record_data_packet(&data);
            }
        }
    }

    Ok(())
}

async fn advance_sessions(
    socket: &UdpSocket,
    sessions: &mut HashMap<u64, Session>,
    now: tokio::time::Instant,
) -> Result<()> {
    // Avoid long per-tick work; especially if pps is huge.
    const MAX_ACTIONS_PER_TICK: usize = 128;
    let mut actions = 0usize;

    // To avoid borrow issues, operate on a snapshot of ids.
    let session_ids: Vec<u64> = sessions.keys().copied().collect();
    for session_id in session_ids {
        if actions >= MAX_ACTIONS_PER_TICK {
            break;
        }
        let Some(sess) = sessions.get_mut(&session_id) else { continue };

        // Stop sending when duration ends.
        if let Some(end_at) = sess.send_end_at {
            if now >= end_at {
                sess.next_send_at = None;
            }
        }

        // Send takes precedence over report for "first packet should be data" tests.
        if actions < MAX_ACTIONS_PER_TICK && sess.should_send_now(now) {
            send_one(socket, session_id, sess, now).await?;
            actions += 1;
        }

        if actions < MAX_ACTIONS_PER_TICK && sess.should_report_now(now) {
            send_report(socket, session_id, sess, now, false).await?;
            actions += 1;
        }
    }

    Ok(())
}

async fn send_one(
    socket: &UdpSocket,
    session_id: u64,
    sess: &mut Session,
    now: tokio::time::Instant,
) -> Result<()> {
    let Some(next_send_at) = sess.next_send_at else { return Ok(()) };
    if next_send_at > now {
        return Ok(());
    }
    if !matches!(sess.mode, UdpPerfMode::Reverse | UdpPerfMode::Bidir) {
        sess.next_send_at = None;
        return Ok(());
    }

    sess.send_seq = sess.send_seq.saturating_add(1);
    let payload_len = sess.send_payload_len;
    let pkt = UdpPerfDataPacket {
        header: UdpPerfDataHeader {
            session_id,
            stream_id: 1,
            direction: UdpPerfDirection::Reverse,
            seq: sess.send_seq,
            send_ts_micros: 0,
            payload_len: payload_len as u16,
        },
        payload: vec![0x5a; payload_len],
    };

    let bytes = UdpPerfWirePacket::Data(pkt.clone()).encode()?;
    socket
        .send_to(&bytes, sess.peer)
        .await
        .with_context(|| format!("udp send_to failed to [{}]", sess.peer))?;

    sess.stats.record_data_packet(&pkt);

    // Next send time.
    let pps = sess.send_pps.max(1);
    let interval_micros = 1_000_000u64 / pps;
    let interval = Duration::from_micros(interval_micros.max(1));
    sess.next_send_at = Some(now + interval);
    Ok(())
}

async fn send_report(
    socket: &UdpSocket,
    session_id: u64,
    sess: &mut Session,
    now: tokio::time::Instant,
    is_final: bool,
) -> Result<()> {
    sess.report_seq = sess.report_seq.saturating_add(1);
    let total_elapsed = now.duration_since(sess.started_at);
    let interval_elapsed = now.duration_since(sess.last_report_at);
    let summary = sess.stats.build_summary(
        session_id,
        sess.mode,
        dur_to_micros(total_elapsed),
        dur_to_micros(interval_elapsed),
    );
    let report = UdpPerfReport {
        report_seq: sess.report_seq,
        is_final,
        summary,
    };

    let bytes = UdpPerfWirePacket::Control(UdpPerfControlPacket::Report(report)).encode()?;
    socket
        .send_to(&bytes, sess.peer)
        .await
        .with_context(|| format!("udp send_to failed to [{}]", sess.peer))?;

    sess.last_report_at = now;
    sess.next_report_at = now + sess.report_interval;
    sess.stats.reset_interval();
    Ok(())
}

async fn send_final_report(
    socket: &UdpSocket,
    session_id: u64,
    sess: &mut Session,
    now: tokio::time::Instant,
) -> Result<()> {
    send_report(socket, session_id, sess, now, true).await
}

fn dur_to_micros(d: Duration) -> u64 {
    // subsec_micros is stable and avoids rounding.
    (d.as_secs() as u64)
        .saturating_mul(1_000_000)
        .saturating_add(d.subsec_micros() as u64)
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, time::Duration};

    use anyhow::Result;
    use tokio::{net::UdpSocket, sync::oneshot};

    use super::super::udp_perf::{
        UdpPerfControlPacket, UdpPerfDataHeader, UdpPerfDataPacket, UdpPerfDirection,
        UdpPerfMode, UdpPerfStart, UdpPerfStop, UdpPerfWirePacket,
    };

    #[tokio::test]
    async fn udp_server_forward_mode_receives_packets() -> Result<()> {
        let (server_addr, shutdown_tx, server_task) = spawn_server_for_test(10).await?;

        let client = UdpSocket::bind("127.0.0.1:0").await?;
        client.connect(server_addr).await?;

        let session_id = 101;
        client
            .send(&wire_control(UdpPerfControlPacket::Start(UdpPerfStart {
                session_id,
                mode: UdpPerfMode::Forward,
                payload_len: 16,
                packets_per_second: 1_000,
                duration_micros: 200_000,
                report_interval_micros: 20_000,
            }))?)
            .await?;

        for seq in 1..=3 {
            client
                .send(&wire_data(session_id, UdpPerfDirection::Forward, seq, 16)?)
                .await?;
        }

        let report = recv_report(&client, Duration::from_millis(200)).await?;
        assert!(!report.is_final);
        assert_eq!(report.summary.session_id, session_id);
        assert_eq!(report.summary.mode, UdpPerfMode::Forward);
        assert!(report.summary.forward.total.packets >= 3);

        shutdown_tx.send(()).ok();
        server_task.abort();
        Ok(())
    }

    #[tokio::test]
    async fn udp_server_reverse_mode_sends_packets() -> Result<()> {
        let (server_addr, shutdown_tx, server_task) = spawn_server_for_test(10).await?;

        let client = UdpSocket::bind("127.0.0.1:0").await?;
        client.connect(server_addr).await?;

        let session_id = 202;
        client
            .send(&wire_control(UdpPerfControlPacket::Start(UdpPerfStart {
                session_id,
                mode: UdpPerfMode::Reverse,
                payload_len: 32,
                packets_per_second: 5_000,
                duration_micros: 200_000,
                report_interval_micros: 0,
            }))?)
            .await?;

        let pkt = recv_wire(&client, Duration::from_millis(200)).await?;
        let data = match pkt {
            UdpPerfWirePacket::Data(d) => d,
            other => panic!("expected data, got {other:?}"),
        };
        assert_eq!(data.header.session_id, session_id);
        assert_eq!(data.header.direction, UdpPerfDirection::Reverse);
        assert_eq!(data.payload.len(), 32);

        shutdown_tx.send(()).ok();
        server_task.abort();
        Ok(())
    }

    #[tokio::test]
    async fn udp_server_bidir_mode_runs_both_directions() -> Result<()> {
        let (server_addr, shutdown_tx, server_task) = spawn_server_for_test(10).await?;

        let client = UdpSocket::bind("127.0.0.1:0").await?;
        client.connect(server_addr).await?;

        let session_id = 303;
        client
            .send(&wire_control(UdpPerfControlPacket::Start(UdpPerfStart {
                session_id,
                mode: UdpPerfMode::Bidir,
                payload_len: 24,
                packets_per_second: 5_000,
                duration_micros: 200_000,
                report_interval_micros: 10_000,
            }))?)
            .await?;

        // client -> server (forward)
        for seq in 1..=3 {
            client
                .send(&wire_data(session_id, UdpPerfDirection::Forward, seq, 24)?)
                .await?;
        }

        // server -> client (reverse)
        let pkt = recv_wire(&client, Duration::from_millis(200)).await?;
        let data = match pkt {
            UdpPerfWirePacket::Data(d) => d,
            other => panic!("expected data, got {other:?}"),
        };
        assert_eq!(data.header.session_id, session_id);
        assert_eq!(data.header.direction, UdpPerfDirection::Reverse);

        // stop and validate final report sees both directions
        client
            .send(&wire_control(UdpPerfControlPacket::Stop(UdpPerfStop { session_id }))?)
            .await?;
        let report = recv_report(&client, Duration::from_millis(200)).await?;
        assert!(report.is_final);
        assert!(report.summary.forward.total.packets >= 3);
        assert!(report.summary.reverse.total.packets >= 1);

        shutdown_tx.send(()).ok();
        server_task.abort();
        Ok(())
    }

    #[tokio::test]
    async fn udp_server_stop_returns_final_report_and_ends_session() -> Result<()> {
        let (server_addr, shutdown_tx, server_task) = spawn_server_for_test(1_000).await?;

        let client = UdpSocket::bind("127.0.0.1:0").await?;
        client.connect(server_addr).await?;

        let session_id = 404;
        client
            .send(&wire_control(UdpPerfControlPacket::Start(UdpPerfStart {
                session_id,
                mode: UdpPerfMode::Forward,
                payload_len: 16,
                packets_per_second: 1_000,
                duration_micros: 200_000,
                report_interval_micros: 0, // rely on server default, and keep it long to avoid extra reports
            }))?)
            .await?;

        client
            .send(&wire_data(session_id, UdpPerfDirection::Forward, 1, 16)?)
            .await?;

        client
            .send(&wire_control(UdpPerfControlPacket::Stop(UdpPerfStop { session_id }))?)
            .await?;

        let report = recv_report(&client, Duration::from_millis(200)).await?;
        assert!(report.is_final);
        assert_eq!(report.summary.session_id, session_id);

        // session removed: further traffic should not trigger reports
        client
            .send(&wire_data(session_id, UdpPerfDirection::Forward, 2, 16)?)
            .await?;
        let mut buf = vec![0u8; 2048];
        let r = tokio::time::timeout(Duration::from_millis(60), client.recv(&mut buf)).await;
        assert!(r.is_err(), "unexpected extra packet after STOP");

        shutdown_tx.send(()).ok();
        server_task.abort();
        Ok(())
    }

    async fn spawn_server_for_test(
        default_interval_ms: u64,
    ) -> Result<(SocketAddr, oneshot::Sender<()>, tokio::task::JoinHandle<Result<()>>)> {
        let socket = UdpSocket::bind("127.0.0.1:0").await?;
        let addr = socket.local_addr()?;
        let (tx, rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            super::udp_server_loop(socket, Duration::from_millis(default_interval_ms), Some(rx))
                .await
        });
        Ok((addr, tx, task))
    }

    fn wire_control(pkt: UdpPerfControlPacket) -> Result<Vec<u8>> {
        UdpPerfWirePacket::Control(pkt).encode()
    }

    fn wire_data(session_id: u64, direction: UdpPerfDirection, seq: u64, payload_len: usize) -> Result<Vec<u8>> {
        UdpPerfWirePacket::Data(UdpPerfDataPacket {
            header: UdpPerfDataHeader {
                session_id,
                stream_id: 1,
                direction,
                seq,
                send_ts_micros: seq * 1_000,
                payload_len: payload_len as u16,
            },
            payload: vec![0x5a; payload_len],
        })
        .encode()
    }

    async fn recv_wire(client: &UdpSocket, timeout: Duration) -> Result<UdpPerfWirePacket> {
        let mut buf = vec![0u8; 2048];
        let n = tokio::time::timeout(timeout, client.recv(&mut buf)).await??;
        Ok(UdpPerfWirePacket::decode(&buf[..n])?)
    }

    async fn recv_report(client: &UdpSocket, timeout: Duration) -> Result<super::super::udp_perf::UdpPerfReport> {
        loop {
            let pkt = recv_wire(client, timeout).await?;
            if let UdpPerfWirePacket::Control(UdpPerfControlPacket::Report(r)) = pkt {
                return Ok(r);
            }
        }
    }
}
