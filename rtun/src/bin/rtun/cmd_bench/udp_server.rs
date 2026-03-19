use std::{collections::HashMap, net::SocketAddr, num::NonZeroU64, time::Duration};

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use tokio::net::UdpSocket;
use tracing::info;

use super::udp_perf::{
    UdpPerfControlPacket, UdpPerfDataHeader, UdpPerfDataPacket, UdpPerfDirection, UdpPerfMode,
    UdpPerfReport, UdpPerfStart, UdpPerfStats, UdpPerfWirePacket,
};

type SessionKey = (SocketAddr, u64);

pub async fn run(args: CmdArgs) -> Result<()> {
    let listen: SocketAddr = args
        .listen
        .parse()
        .with_context(|| format!("invalid --listen value [{}], expected ip:port", args.listen))?;
    let socket = UdpSocket::bind(listen)
        .await
        .with_context(|| format!("udp-server bind failed at [{listen}]"))?;
    let local_addr = socket
        .local_addr()
        .with_context(|| "get local_addr failed")?;
    info!(
        "udp bench server listen [{}], report interval [{}ms]",
        local_addr,
        args.interval.get()
    );

    // For CLI, run forever. Tests provide shutdown channel.
    udp_server_loop(socket, Duration::from_millis(args.interval.get()), None).await
}

#[derive(Parser, Debug)]
#[clap(name = "udp-server", author, about, version)]
pub struct CmdArgs {
    #[clap(long = "listen", default_value = "0.0.0.0:9001")]
    pub listen: String,

    #[clap(long = "interval", default_value = "1000")]
    pub interval: NonZeroU64,
}

#[derive(Debug)]
struct Session {
    peer: SocketAddr,
    mode: UdpPerfMode,
    started: bool,
    started_at: tokio::time::Instant,
    last_report_at: tokio::time::Instant,
    report_interval: Duration,
    next_report_at: Option<tokio::time::Instant>,
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
    fn new_hello(
        peer: SocketAddr,
        mode: UdpPerfMode,
        default_report_interval: Duration,
    ) -> Result<Self> {
        ensure_nonzero_interval(default_report_interval)?;
        let now = tokio::time::Instant::now();
        Ok(Self {
            peer,
            mode,
            started: false,
            started_at: now,
            last_report_at: now,
            report_interval: default_report_interval,
            next_report_at: None,
            report_seq: 0,
            stats: UdpPerfStats::default(),
            send_payload_len: 0,
            send_pps: 0,
            send_seq: 0,
            send_end_at: None,
            next_send_at: None,
        })
    }

    fn new_start(
        peer: SocketAddr,
        start: &UdpPerfStart,
        default_interval: Duration,
    ) -> Result<Self> {
        ensure_nonzero_interval(default_interval)?;
        let now = tokio::time::Instant::now();

        let report_interval = if start.report_interval_micros != 0 {
            let d = Duration::from_micros(start.report_interval_micros);
            ensure_nonzero_interval(d).with_context(|| "invalid START.report_interval_micros")?;
            d
        } else {
            default_interval
        };

        let next_report_at =
            checked_add(now, report_interval).with_context(|| "report deadline overflow")?;

        let mut sess = Self {
            peer,
            mode: start.mode,
            started: true,
            started_at: now,
            last_report_at: now,
            report_interval,
            next_report_at: Some(next_report_at),
            report_seq: 0,
            stats: UdpPerfStats::default(),
            send_payload_len: start.payload_len as usize,
            send_pps: start.packets_per_second,
            send_seq: 0,
            send_end_at: None,
            next_send_at: None,
        };

        sess.apply_send_cfg(now, start)?;
        Ok(sess)
    }

    fn apply_send_cfg(&mut self, now: tokio::time::Instant, start: &UdpPerfStart) -> Result<()> {
        // If pps/duration are zero, do not send reverse data (and do not "send one immediately").
        if start.packets_per_second == 0 || start.duration_micros == 0 {
            self.send_end_at = None;
            self.next_send_at = None;
            return Ok(());
        }

        let end = checked_add(now, Duration::from_micros(start.duration_micros))
            .with_context(|| "duration deadline overflow")?;
        self.send_end_at = Some(end);
        match start.mode {
            UdpPerfMode::Forward => self.next_send_at = None,
            UdpPerfMode::Reverse | UdpPerfMode::Bidir => self.next_send_at = Some(now),
        }
        Ok(())
    }

    fn next_deadline(&self) -> Option<tokio::time::Instant> {
        let r = self.next_report_at;
        let s = self.next_send_at;
        match (r, s) {
            (None, None) => None,
            (Some(r), None) => Some(r),
            (None, Some(s)) => Some(s),
            (Some(r), Some(s)) => Some(std::cmp::min(r, s)),
        }
    }

    fn should_send_now(&self, now: tokio::time::Instant) -> bool {
        match self.next_send_at {
            Some(t) => t <= now,
            None => false,
        }
    }

    fn should_report_now(&self, now: tokio::time::Instant) -> bool {
        match self.next_report_at {
            Some(t) => t <= now,
            None => false,
        }
    }
}

async fn udp_server_loop(
    socket: UdpSocket,
    default_report_interval: Duration,
    mut shutdown: Option<tokio::sync::oneshot::Receiver<()>>,
) -> Result<()> {
    ensure_nonzero_interval(default_report_interval)?;
    let mut sessions: HashMap<SessionKey, Session> = HashMap::new();
    let mut buf = vec![0u8; 64 * 1024];

    loop {
        let next_deadline = sessions.values().filter_map(|s| s.next_deadline()).min();

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
    sessions: &mut HashMap<SessionKey, Session>,
    default_report_interval: Duration,
    packet: &[u8],
    from: SocketAddr,
) -> Result<()> {
    let wire = UdpPerfWirePacket::decode(packet)?;
    match wire {
        UdpPerfWirePacket::Control(ctrl) => match ctrl {
            UdpPerfControlPacket::Hello(hello) => {
                let key = session_key(from, hello.session_id);
                let sess = sessions
                    .entry(key)
                    .or_insert(Session::new_hello(
                        from,
                        hello.mode,
                        default_report_interval,
                    )?);
                // Only allow HELLO to update mode before START.
                if !sess.started {
                    sess.mode = hello.mode;
                }
            }
            UdpPerfControlPacket::Start(start) => {
                // START begins a new measurement round: replace session (reset total+interval).
                let key = session_key(from, start.session_id);
                let new_sess = Session::new_start(from, &start, default_report_interval)?;
                sessions.insert(key, new_sess);

                // For reverse/bidir with valid send cfg, send one packet immediately so client sees DATA.
                if let Some(sess) = sessions.get_mut(&key) {
                    if matches!(start.mode, UdpPerfMode::Reverse | UdpPerfMode::Bidir)
                        && sess.next_send_at.is_some()
                    {
                        send_one(socket, start.session_id, sess, tokio::time::Instant::now())
                            .await?;
                    }
                }
            }
            UdpPerfControlPacket::Stop(stop) => {
                if let Some(mut sess) = sessions.remove(&session_key(from, stop.session_id)) {
                    send_final_report(
                        socket,
                        stop.session_id,
                        &mut sess,
                        tokio::time::Instant::now(),
                    )
                    .await?;
                }
            }
            UdpPerfControlPacket::Report(_) => {
                // client->server report is ignored.
            }
        },
        UdpPerfWirePacket::Data(data) => {
            if let Some(sess) = sessions.get_mut(&session_key(from, data.header.session_id)) {
                if !sess.started {
                    return Ok(());
                }
                sess.stats.record_data_packet(&data);
            }
        }
    }

    Ok(())
}

async fn advance_sessions(
    socket: &UdpSocket,
    sessions: &mut HashMap<SessionKey, Session>,
    now: tokio::time::Instant,
) -> Result<()> {
    // Avoid long per-tick work; especially if pps is huge.
    const MAX_ACTIONS_PER_TICK: usize = 128;
    let mut actions = 0usize;

    // To avoid borrow issues, operate on a snapshot of ids.
    let session_keys: Vec<SessionKey> = sessions.keys().copied().collect();
    for (peer, session_id) in session_keys {
        if actions >= MAX_ACTIONS_PER_TICK {
            break;
        }
        let key = session_key(peer, session_id);
        let Some(sess) = sessions.get_mut(&key) else {
            continue;
        };

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
    let Some(next_send_at) = sess.next_send_at else {
        return Ok(());
    };
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
    if sess.send_pps == 0 {
        sess.next_send_at = None;
        return Ok(());
    }
    let interval_micros = 1_000_000u64 / sess.send_pps;
    let interval = Duration::from_micros(interval_micros.max(1));
    sess.next_send_at = checked_add(now, interval);
    Ok(())
}

async fn send_report(
    socket: &UdpSocket,
    session_id: u64,
    sess: &mut Session,
    now: tokio::time::Instant,
    is_final: bool,
) -> Result<()> {
    if sess.report_interval.is_zero() {
        // Defensive: never schedule a zero-interval report storm.
        sess.next_report_at = None;
        return Err(anyhow!("report interval is zero"));
    }
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
    sess.next_report_at = checked_add(now, sess.report_interval);
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

fn ensure_nonzero_interval(d: Duration) -> Result<()> {
    if d.is_zero() {
        bail!("report interval must be non-zero");
    }
    Ok(())
}

fn checked_add(base: tokio::time::Instant, delta: Duration) -> Option<tokio::time::Instant> {
    base.checked_add(delta)
}

fn session_key(peer: SocketAddr, session_id: u64) -> SessionKey {
    (peer, session_id)
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, time::Duration};

    use anyhow::Result;
    use clap::Parser;
    use tokio::{net::UdpSocket, sync::oneshot};

    use super::super::udp_perf::{
        UdpPerfControlPacket, UdpPerfDataHeader, UdpPerfDataPacket, UdpPerfDirection, UdpPerfMode,
        UdpPerfStart, UdpPerfStop, UdpPerfWirePacket,
    };

    #[test]
    fn udp_server_cli_rejects_zero_interval() {
        let err = super::CmdArgs::try_parse_from(["udp-server", "--interval", "0"])
            .expect_err("interval=0 should be rejected");
        let s = err.to_string();
        assert!(s.contains("interval"), "unexpected clap error: {s}");
    }

    #[tokio::test]
    async fn udp_server_loop_rejects_zero_default_interval() -> Result<()> {
        let socket = UdpSocket::bind("127.0.0.1:0").await?;
        let (_tx, rx) = oneshot::channel();
        let err = super::udp_server_loop(socket, Duration::from_millis(0), Some(rx))
            .await
            .expect_err("zero interval should error");
        assert!(err.to_string().contains("interval"));
        Ok(())
    }

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
            .send(&wire_control(UdpPerfControlPacket::Stop(UdpPerfStop {
                session_id,
            }))?)
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
            .send(&wire_control(UdpPerfControlPacket::Stop(UdpPerfStop {
                session_id,
            }))?)
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

    #[tokio::test]
    async fn udp_server_hello_does_not_emit_report_before_start() -> Result<()> {
        let (server_addr, shutdown_tx, server_task) = spawn_server_for_test(5).await?;

        let client = UdpSocket::bind("127.0.0.1:0").await?;
        client.connect(server_addr).await?;

        let session_id = 505;
        client
            .send(&wire_control(UdpPerfControlPacket::Hello(
                super::super::udp_perf::UdpPerfHello {
                    session_id,
                    mode: UdpPerfMode::Forward,
                },
            ))?)
            .await?;

        // No START yet: should not receive any report.
        let mut buf = vec![0u8; 2048];
        let r = tokio::time::timeout(Duration::from_millis(40), client.recv(&mut buf)).await;
        assert!(r.is_err(), "unexpected packet before START");

        shutdown_tx.send(()).ok();
        server_task.abort();
        Ok(())
    }

    #[tokio::test]
    async fn udp_server_reverse_mode_pps_zero_does_not_send_data() -> Result<()> {
        let (server_addr, shutdown_tx, server_task) = spawn_server_for_test(10).await?;

        let client = UdpSocket::bind("127.0.0.1:0").await?;
        client.connect(server_addr).await?;

        let session_id = 606;
        client
            .send(&wire_control(UdpPerfControlPacket::Start(UdpPerfStart {
                session_id,
                mode: UdpPerfMode::Reverse,
                payload_len: 32,
                packets_per_second: 0,
                duration_micros: 200_000,
                report_interval_micros: 10_000,
            }))?)
            .await?;

        let pkt = recv_wire(&client, Duration::from_millis(80)).await?;
        match pkt {
            UdpPerfWirePacket::Control(UdpPerfControlPacket::Report(_)) => {}
            other => panic!("expected report (no data), got {other:?}"),
        }

        shutdown_tx.send(()).ok();
        server_task.abort();
        Ok(())
    }

    #[tokio::test]
    async fn udp_server_reverse_mode_duration_zero_does_not_send_data() -> Result<()> {
        let (server_addr, shutdown_tx, server_task) = spawn_server_for_test(10).await?;

        let client = UdpSocket::bind("127.0.0.1:0").await?;
        client.connect(server_addr).await?;

        let session_id = 607;
        client
            .send(&wire_control(UdpPerfControlPacket::Start(UdpPerfStart {
                session_id,
                mode: UdpPerfMode::Reverse,
                payload_len: 32,
                packets_per_second: 5_000,
                duration_micros: 0,
                report_interval_micros: 10_000,
            }))?)
            .await?;

        let pkt = recv_wire(&client, Duration::from_millis(80)).await?;
        match pkt {
            UdpPerfWirePacket::Control(UdpPerfControlPacket::Report(_)) => {}
            other => panic!("expected report (no data), got {other:?}"),
        }

        shutdown_tx.send(()).ok();
        server_task.abort();
        Ok(())
    }

    #[tokio::test]
    async fn udp_server_repeated_start_resets_total_stats() -> Result<()> {
        let (server_addr, shutdown_tx, server_task) = spawn_server_for_test(10).await?;

        let client = UdpSocket::bind("127.0.0.1:0").await?;
        client.connect(server_addr).await?;

        let session_id = 707;
        client
            .send(&wire_control(UdpPerfControlPacket::Start(UdpPerfStart {
                session_id,
                mode: UdpPerfMode::Forward,
                payload_len: 16,
                packets_per_second: 1_000,
                duration_micros: 200_000,
                report_interval_micros: 10_000,
            }))?)
            .await?;

        for seq in 1..=3 {
            client
                .send(&wire_data(session_id, UdpPerfDirection::Forward, seq, 16)?)
                .await?;
        }

        let r1 = recv_report(&client, Duration::from_millis(200)).await?;
        assert!(r1.summary.total.packets >= 3);

        // Restart measurement: total should reset.
        client
            .send(&wire_control(UdpPerfControlPacket::Start(UdpPerfStart {
                session_id,
                mode: UdpPerfMode::Forward,
                payload_len: 16,
                packets_per_second: 1_000,
                duration_micros: 200_000,
                report_interval_micros: 10_000,
            }))?)
            .await?;

        client
            .send(&wire_data(session_id, UdpPerfDirection::Forward, 1, 16)?)
            .await?;

        let r2 = recv_report(&client, Duration::from_millis(200)).await?;
        assert_eq!(r2.summary.total.packets, 1);

        shutdown_tx.send(()).ok();
        server_task.abort();
        Ok(())
    }

    #[tokio::test]
    async fn udp_server_ignores_overflowing_start_and_keeps_running() -> Result<()> {
        let (server_addr, shutdown_tx, server_task) = spawn_server_for_test(10).await?;

        let client = UdpSocket::bind("127.0.0.1:0").await?;
        client.connect(server_addr).await?;

        let session_id = 808;
        // This should be rejected (deadline overflow) and produce no packet.
        client
            .send(&wire_control(UdpPerfControlPacket::Start(UdpPerfStart {
                session_id,
                mode: UdpPerfMode::Forward,
                payload_len: 16,
                packets_per_second: 1,
                duration_micros: u64::MAX,
                report_interval_micros: u64::MAX,
            }))?)
            .await?;

        let mut buf = vec![0u8; 2048];
        let r = tokio::time::timeout(Duration::from_millis(40), client.recv(&mut buf)).await;
        assert!(r.is_err(), "unexpected packet from invalid START");

        // Then a valid START should work.
        client
            .send(&wire_control(UdpPerfControlPacket::Start(UdpPerfStart {
                session_id,
                mode: UdpPerfMode::Forward,
                payload_len: 16,
                packets_per_second: 1_000,
                duration_micros: 200_000,
                report_interval_micros: 10_000,
            }))?)
            .await?;
        client
            .send(&wire_data(session_id, UdpPerfDirection::Forward, 1, 16)?)
            .await?;
        let report = recv_report(&client, Duration::from_millis(200)).await?;
        assert_eq!(report.summary.session_id, session_id);

        shutdown_tx.send(()).ok();
        server_task.abort();
        Ok(())
    }

    #[tokio::test]
    async fn udp_server_data_from_other_peer_does_not_hijack_session_peer() -> Result<()> {
        let (server_addr, shutdown_tx, server_task) = spawn_server_for_test(10).await?;

        let client1 = UdpSocket::bind("127.0.0.1:0").await?;
        client1.connect(server_addr).await?;

        let client2 = UdpSocket::bind("127.0.0.1:0").await?;

        let session_id = 909;
        client1
            .send(&wire_control(UdpPerfControlPacket::Start(UdpPerfStart {
                session_id,
                mode: UdpPerfMode::Forward,
                payload_len: 16,
                packets_per_second: 1_000,
                duration_micros: 200_000,
                report_interval_micros: 10_000,
            }))?)
            .await?;

        client1
            .send(&wire_data(session_id, UdpPerfDirection::Forward, 1, 16)?)
            .await?;

        // Spoof data from client2 using same session_id.
        client2
            .send_to(
                &wire_data(session_id, UdpPerfDirection::Forward, 2, 16)?,
                server_addr,
            )
            .await?;

        // Report should still go to client1, not client2.
        let report1 = recv_report(&client1, Duration::from_millis(200)).await?;
        assert_eq!(report1.summary.session_id, session_id);

        let mut buf = vec![0u8; 2048];
        let r = tokio::time::timeout(Duration::from_millis(40), client2.recv_from(&mut buf)).await;
        assert!(r.is_err(), "unexpected packet delivered to hijacker");

        shutdown_tx.send(()).ok();
        server_task.abort();
        Ok(())
    }

    #[tokio::test]
    async fn udp_server_same_session_id_keeps_reverse_data_isolated_per_peer() -> Result<()> {
        let (server_addr, shutdown_tx, server_task) = spawn_server_for_test(1_000).await?;

        let client1 = UdpSocket::bind("127.0.0.1:0").await?;
        client1.connect(server_addr).await?;

        let client2 = UdpSocket::bind("127.0.0.1:0").await?;
        client2.connect(server_addr).await?;

        let session_id = 1_001;
        let start = UdpPerfStart {
            session_id,
            mode: UdpPerfMode::Reverse,
            payload_len: 32,
            packets_per_second: 5_000,
            duration_micros: 300_000,
            report_interval_micros: 200_000,
        };

        client1
            .send(&wire_control(UdpPerfControlPacket::Start(start.clone()))?)
            .await?;
        let first1 = recv_data(&client1, Duration::from_millis(200)).await?;
        assert_eq!(first1.header.session_id, session_id);
        assert_eq!(first1.header.direction, UdpPerfDirection::Reverse);

        client2
            .send(&wire_control(UdpPerfControlPacket::Start(start))?)
            .await?;
        let first2 = recv_data(&client2, Duration::from_millis(200)).await?;
        assert_eq!(first2.header.session_id, session_id);
        assert_eq!(first2.header.direction, UdpPerfDirection::Reverse);

        let next1 = recv_data(&client1, Duration::from_millis(200)).await?;
        let next2 = recv_data(&client2, Duration::from_millis(200)).await?;
        assert!(next1.header.seq > first1.header.seq);
        assert!(next2.header.seq > first2.header.seq);

        shutdown_tx.send(()).ok();
        server_task.abort();
        Ok(())
    }

    #[tokio::test]
    async fn udp_server_same_session_id_keeps_stop_and_final_report_isolated_per_peer() -> Result<()>
    {
        let (server_addr, shutdown_tx, server_task) = spawn_server_for_test(1_000).await?;

        let client1 = UdpSocket::bind("127.0.0.1:0").await?;
        client1.connect(server_addr).await?;

        let client2 = UdpSocket::bind("127.0.0.1:0").await?;
        client2.connect(server_addr).await?;

        let session_id = 1_002;
        let start = UdpPerfStart {
            session_id,
            mode: UdpPerfMode::Forward,
            payload_len: 16,
            packets_per_second: 1_000,
            duration_micros: 300_000,
            report_interval_micros: 200_000,
        };

        client1
            .send(&wire_control(UdpPerfControlPacket::Start(start.clone()))?)
            .await?;
        client2
            .send(&wire_control(UdpPerfControlPacket::Start(start))?)
            .await?;

        client1
            .send(&wire_data(session_id, UdpPerfDirection::Forward, 1, 16)?)
            .await?;
        client2
            .send(&wire_data(session_id, UdpPerfDirection::Forward, 1, 16)?)
            .await?;

        client1
            .send(&wire_control(UdpPerfControlPacket::Stop(UdpPerfStop {
                session_id,
            }))?)
            .await?;
        let report1 = recv_report(&client1, Duration::from_millis(200)).await?;
        assert!(report1.is_final);
        assert_eq!(report1.summary.session_id, session_id);
        assert_eq!(report1.summary.forward.total.packets, 1);

        let mut buf = vec![0u8; 2048];
        let no_cross_report =
            tokio::time::timeout(Duration::from_millis(40), client2.recv(&mut buf)).await;
        assert!(
            no_cross_report.is_err(),
            "unexpected packet delivered to peer2 after peer1 STOP"
        );

        client2
            .send(&wire_data(session_id, UdpPerfDirection::Forward, 2, 16)?)
            .await?;
        client2
            .send(&wire_control(UdpPerfControlPacket::Stop(UdpPerfStop {
                session_id,
            }))?)
            .await?;
        let report2 = recv_report(&client2, Duration::from_millis(200)).await?;
        assert!(report2.is_final);
        assert_eq!(report2.summary.session_id, session_id);
        assert_eq!(report2.summary.forward.total.packets, 2);

        shutdown_tx.send(()).ok();
        server_task.abort();
        Ok(())
    }

    async fn spawn_server_for_test(
        default_interval_ms: u64,
    ) -> Result<(
        SocketAddr,
        oneshot::Sender<()>,
        tokio::task::JoinHandle<Result<()>>,
    )> {
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

    fn wire_data(
        session_id: u64,
        direction: UdpPerfDirection,
        seq: u64,
        payload_len: usize,
    ) -> Result<Vec<u8>> {
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

    async fn recv_report(
        client: &UdpSocket,
        timeout: Duration,
    ) -> Result<super::super::udp_perf::UdpPerfReport> {
        loop {
            let pkt = recv_wire(client, timeout).await?;
            if let UdpPerfWirePacket::Control(UdpPerfControlPacket::Report(r)) = pkt {
                return Ok(r);
            }
        }
    }

    async fn recv_data(
        client: &UdpSocket,
        timeout: Duration,
    ) -> Result<super::super::udp_perf::UdpPerfDataPacket> {
        loop {
            let pkt = recv_wire(client, timeout).await?;
            if let UdpPerfWirePacket::Data(data) = pkt {
                return Ok(data);
            }
        }
    }
}
