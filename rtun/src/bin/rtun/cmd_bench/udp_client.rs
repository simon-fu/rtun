use std::{
    cmp::{max, min},
    net::SocketAddr,
    num::NonZeroU64,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, ensure, Context, Result};
use clap::Parser;
use tokio::{net::UdpSocket, time::Instant};
use tracing::info;

use super::udp_perf::{
    UdpPerfControlPacket, UdpPerfDataHeader, UdpPerfDataPacket, UdpPerfDirection, UdpPerfHello,
    UdpPerfMode, UdpPerfReport, UdpPerfStart, UdpPerfStop, UdpPerfSummary, UdpPerfWirePacket,
};

const DEFAULT_PPS: u64 = 1_000;
const FINAL_REPORT_TIMEOUT: Duration = Duration::from_secs(2);
const IO_WAIT_MAX: Duration = Duration::from_millis(50);
const IO_WAIT_MIN: Duration = Duration::from_millis(1);

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

pub async fn run(args: CmdArgs) -> Result<()> {
    let out = run_inner(args).await?;
    print!("{}", out.output);
    Ok(())
}

async fn run_inner(args: CmdArgs) -> Result<ClientRunOutput> {
    ensure!(
        args.bitrate.is_none() || args.pps.is_none(),
        "--bitrate and --pps are mutually exclusive"
    );
    ensure!(args.len > 0, "--len must be greater than 0");
    let payload_len = u16::try_from(args.len).context("--len must be <= 65535")?;
    let target: SocketAddr = args
        .target
        .parse()
        .with_context(|| format!("invalid --target value [{}], expected ip:port", args.target))?;
    let pps = resolve_pps(&args)?;
    let duration_micros = args
        .time
        .checked_mul(1_000_000)
        .context("--time is too large")?;
    let report_interval_micros = args
        .interval
        .get()
        .checked_mul(1_000)
        .context("--interval is too large")?;
    let duration = Duration::from_micros(duration_micros);

    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .with_context(|| "udp-client bind failed")?;
    socket
        .connect(target)
        .await
        .with_context(|| format!("udp-client connect to [{target}] failed"))?;

    let session_id = next_session_id();
    if !args.json {
        info!(
            "udp bench client start: target [{}], mode [{}], time [{}s], len [{}], pps [{}]",
            target, args.mode, args.time, args.len, pps
        );
    }
    send_control(
        &socket,
        UdpPerfControlPacket::Hello(UdpPerfHello {
            session_id,
            mode: args.mode,
        }),
    )
    .await?;

    if args.warmup > 0 {
        tokio::time::sleep(Duration::from_secs(args.warmup)).await;
    }

    send_control(
        &socket,
        UdpPerfControlPacket::Start(UdpPerfStart {
            session_id,
            mode: args.mode,
            payload_len,
            packets_per_second: pps,
            duration_micros,
            report_interval_micros,
        }),
    )
    .await?;

    let started_at = Instant::now();
    let stop_at = started_at
        .checked_add(duration)
        .context("local stop deadline overflow")?;
    let mut stop_sent = false;
    let mut final_deadline: Option<Instant> = None;
    let mut next_send_at = first_send_deadline(started_at, args.mode, pps);
    let mut send_seq: u64 = 0;
    let mut client_rx = ClientRxStats::default();
    let mut final_report: Option<UdpPerfReport> = None;
    let mut recv_buf = vec![0u8; 64 * 1024];
    let mut output = String::new();

    loop {
        let now = Instant::now();

        if !stop_sent {
            while let Some(due) = next_send_at {
                if due > now {
                    break;
                }
                send_seq = send_seq.saturating_add(1);
                send_forward_data(&socket, session_id, send_seq, payload_len).await?;
                next_send_at = next_send_deadline(due, pps);
            }
        }

        if !stop_sent && now >= stop_at {
            send_control(
                &socket,
                UdpPerfControlPacket::Stop(UdpPerfStop { session_id }),
            )
            .await?;
            stop_sent = true;
            next_send_at = None;
            final_deadline = Some(
                now.checked_add(FINAL_REPORT_TIMEOUT)
                    .context("final report deadline overflow")?,
            );
        }

        if let Some(deadline) = final_deadline {
            if now >= deadline {
                bail!("timeout waiting final report");
            }
        }

        if let Some(report) = final_report.take() {
            let final_summary = build_client_view_summary(&report.summary, &client_rx);
            output.push_str(&render_final_summary(&final_summary, args.json)?);
            return Ok(ClientRunOutput {
                final_report: report,
                client_rx,
                output,
            });
        }

        let wait = compute_wait_timeout(
            now,
            next_send_at,
            (!stop_sent).then_some(stop_at),
            final_deadline,
        );
        match tokio::time::timeout(wait, socket.recv(&mut recv_buf)).await {
            Ok(Ok(n)) => {
                if let Some(report) =
                    handle_incoming_packet(&recv_buf[..n], &mut output, !args.json, &mut client_rx)?
                {
                    if report.is_final {
                        final_report = Some(report);
                    }
                }
            }
            Ok(Err(err)) => {
                return Err(err).with_context(|| "udp-client recv failed");
            }
            Err(_) => {}
        }
    }
}

#[cfg(test)]
async fn run_for_test(args: CmdArgs) -> Result<ClientRunOutput> {
    run_inner(args).await
}

#[derive(Debug)]
struct ClientRunOutput {
    final_report: UdpPerfReport,
    client_rx: ClientRxStats,
    output: String,
}

#[derive(Debug, Default, Clone, Copy)]
struct ClientRxStats {
    reverse_packets: u64,
    reverse_bytes: u64,
}

impl ClientRxStats {
    fn on_data_packet(&mut self, data: &UdpPerfDataPacket) {
        if matches!(data.header.direction, UdpPerfDirection::Reverse) {
            self.reverse_packets = self.reverse_packets.saturating_add(1);
            self.reverse_bytes = self.reverse_bytes.saturating_add(data.payload.len() as u64);
        }
    }
}

fn resolve_pps(args: &CmdArgs) -> Result<u64> {
    if let Some(pps) = args.pps {
        return Ok(pps);
    }
    if let Some(bitrate) = args.bitrate {
        if bitrate == 0 {
            return Ok(0);
        }
        let bits_per_packet = (args.len as u64).saturating_mul(8);
        ensure!(bits_per_packet > 0, "invalid --len for bitrate calculation");
        let pps = bitrate / bits_per_packet;
        return Ok(pps.max(1));
    }
    Ok(DEFAULT_PPS)
}

fn first_send_deadline(started_at: Instant, mode: UdpPerfMode, pps: u64) -> Option<Instant> {
    if pps == 0 {
        return None;
    }
    if matches!(mode, UdpPerfMode::Forward | UdpPerfMode::Bidir) {
        Some(started_at)
    } else {
        None
    }
}

fn next_send_deadline(now: Instant, pps: u64) -> Option<Instant> {
    if pps == 0 {
        return None;
    }
    let interval_micros = (1_000_000 / pps).max(1);
    now.checked_add(Duration::from_micros(interval_micros))
}

async fn send_control(socket: &UdpSocket, pkt: UdpPerfControlPacket) -> Result<()> {
    let bytes = UdpPerfWirePacket::Control(pkt).encode()?;
    socket
        .send(&bytes)
        .await
        .with_context(|| "udp-client send control packet failed")?;
    Ok(())
}

async fn send_forward_data(
    socket: &UdpSocket,
    session_id: u64,
    seq: u64,
    payload_len: u16,
) -> Result<()> {
    let payload_len = payload_len as usize;
    let packet = UdpPerfDataPacket {
        header: UdpPerfDataHeader {
            session_id,
            stream_id: 1,
            direction: UdpPerfDirection::Forward,
            seq,
            send_ts_micros: now_unix_micros(),
            payload_len: payload_len as u16,
        },
        payload: vec![0x5a; payload_len],
    };

    let bytes = UdpPerfWirePacket::Data(packet).encode()?;
    socket
        .send(&bytes)
        .await
        .with_context(|| "udp-client send data packet failed")?;
    Ok(())
}

fn handle_incoming_packet(
    bytes: &[u8],
    output: &mut String,
    emit_interval: bool,
    client_rx: &mut ClientRxStats,
) -> Result<Option<UdpPerfReport>> {
    let wire = UdpPerfWirePacket::decode(bytes)?;
    match wire {
        UdpPerfWirePacket::Control(UdpPerfControlPacket::Report(report)) => {
            if emit_interval && !report.is_final {
                output.push_str(&render_interval_summary(&report.summary));
            }
            Ok(Some(report))
        }
        UdpPerfWirePacket::Data(data) => {
            client_rx.on_data_packet(&data);
            Ok(None)
        }
        UdpPerfWirePacket::Control(_) => Ok(None),
    }
}

fn render_interval_summary(summary: &UdpPerfSummary) -> String {
    format!(
        "interval mode={} total_pkts={} fwd_pkts={} rev_pkts={}\n",
        summary.mode,
        summary.interval.packets,
        summary.forward.interval.packets,
        summary.reverse.interval.packets
    )
}

fn render_final_summary(summary: &UdpPerfSummary, as_json: bool) -> Result<String> {
    if as_json {
        return Ok(format!("{}\n", serde_json::to_string_pretty(summary)?));
    }
    Ok(format!(
        "final mode={} total_pkts={} fwd_pkts={} rev_pkts={}\n",
        summary.mode,
        summary.total.packets,
        summary.forward.total.packets,
        summary.reverse.total.packets
    ))
}

fn build_client_view_summary(server: &UdpPerfSummary, client_rx: &ClientRxStats) -> UdpPerfSummary {
    let mut merged = server.clone();
    if matches!(server.mode, UdpPerfMode::Reverse | UdpPerfMode::Bidir) {
        let reverse_total = counters_from_local(
            client_rx.reverse_bytes,
            client_rx.reverse_packets,
            server.total_elapsed_micros,
        );
        let reverse_interval = counters_from_local(
            client_rx.reverse_bytes,
            client_rx.reverse_packets,
            server.interval_elapsed_micros,
        );
        merged.reverse.total = reverse_total.clone();
        merged.reverse.interval = reverse_interval;
        merged.total.bytes = server
            .forward
            .total
            .bytes
            .saturating_add(reverse_total.bytes);
        merged.total.packets = server
            .forward
            .total
            .packets
            .saturating_add(reverse_total.packets);
    }
    merged
}

fn counters_from_local(
    bytes: u64,
    packets: u64,
    elapsed_micros: u64,
) -> super::udp_perf::UdpPerfCountersSummary {
    super::udp_perf::UdpPerfCountersSummary {
        bytes,
        packets,
        loss: 0,
        reorder: 0,
        duplicate: 0,
        mbps: bytes_to_mbps_local(bytes, elapsed_micros),
        pps: packets_to_pps_local(packets, elapsed_micros),
    }
}

fn bytes_to_mbps_local(bytes: u64, elapsed_micros: u64) -> f64 {
    if elapsed_micros == 0 {
        return 0.0;
    }
    (bytes as f64 * 8.0) / elapsed_micros as f64
}

fn packets_to_pps_local(packets: u64, elapsed_micros: u64) -> f64 {
    if elapsed_micros == 0 {
        return 0.0;
    }
    (packets as f64 * 1_000_000.0) / elapsed_micros as f64
}

fn compute_wait_timeout(
    now: Instant,
    next_send_at: Option<Instant>,
    stop_at: Option<Instant>,
    final_deadline: Option<Instant>,
) -> Duration {
    let mut next = next_send_at;
    if let Some(t) = stop_at {
        next = Some(next.map_or(t, |cur| min(cur, t)));
    }
    if let Some(t) = final_deadline {
        next = Some(next.map_or(t, |cur| min(cur, t)));
    }
    let wait = next
        .map(|t| t.saturating_duration_since(now))
        .unwrap_or(IO_WAIT_MAX);
    min(max(wait, IO_WAIT_MIN), IO_WAIT_MAX)
}

fn now_unix_micros() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d
            .as_secs()
            .saturating_mul(1_000_000)
            .saturating_add(d.subsec_micros() as u64),
        Err(_) => 0,
    }
}

fn next_session_id() -> u64 {
    NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed)
}

#[derive(Parser, Debug)]
#[clap(name = "udp-client", author, about, version)]
pub struct CmdArgs {
    #[clap(long = "target")]
    pub target: String,

    #[clap(long = "mode", value_enum, default_value_t = UdpPerfMode::Forward)]
    pub mode: UdpPerfMode,

    #[clap(long = "time", default_value_t = 10)]
    pub time: u64,

    #[clap(long = "len", default_value_t = 1200)]
    pub len: usize,

    #[clap(long = "bitrate", conflicts_with = "pps")]
    pub bitrate: Option<u64>,

    #[clap(long = "pps", conflicts_with = "bitrate")]
    pub pps: Option<u64>,

    #[clap(long = "interval", default_value = "1000")]
    pub interval: NonZeroU64,

    #[clap(long = "warmup", default_value_t = 0)]
    pub warmup: u64,

    #[clap(long = "json")]
    pub json: bool,
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, num::NonZeroU64, time::Duration};

    use anyhow::{Context, Result};
    use serde_json::Value;

    use super::super::udp_server;
    use super::{CmdArgs, UdpPerfMode};

    #[tokio::test]
    async fn udp_client_forward_mode_completes_session() -> Result<()> {
        let (target, _server_addr, server_task) = spawn_server(15).await?;
        let args = test_args(target, UdpPerfMode::Forward);
        let out = super::run_for_test(args).await?;
        let expected_min = (300u64.saturating_mul(1)).saturating_div(3);

        assert!(out.final_report.is_final);
        assert_eq!(out.final_report.summary.mode, UdpPerfMode::Forward);
        assert!(
            out.final_report.summary.forward.total.packets >= expected_min,
            "forward packets too low, got={}, expected_min={expected_min}",
            out.final_report.summary.forward.total.packets
        );

        server_task.abort();
        Ok(())
    }

    #[tokio::test]
    async fn udp_client_reverse_mode_receives_reported_stream() -> Result<()> {
        let (target, _server_addr, server_task) = spawn_server(15).await?;
        let mut args = test_args(target, UdpPerfMode::Reverse);
        args.pps = Some(300);
        let out = super::run_for_test(args).await?;

        assert!(out.final_report.is_final);
        assert_eq!(out.final_report.summary.mode, UdpPerfMode::Reverse);
        assert!(
            out.client_rx.reverse_packets > 0,
            "client should receive reverse data packets"
        );
        assert!(
            out.client_rx.reverse_bytes > 0,
            "client should receive reverse data bytes"
        );

        server_task.abort();
        Ok(())
    }

    #[tokio::test]
    async fn udp_client_bidir_mode_collects_dual_direction_stats() -> Result<()> {
        let (target, _server_addr, server_task) = spawn_server(15).await?;
        let mut args = test_args(target, UdpPerfMode::Bidir);
        args.pps = Some(300);
        let out = super::run_for_test(args).await?;

        assert!(out.final_report.is_final);
        assert_eq!(out.final_report.summary.mode, UdpPerfMode::Bidir);
        assert!(out.final_report.summary.forward.total.packets > 0);
        assert!(
            out.client_rx.reverse_packets > 0,
            "client should receive reverse data packets in bidir"
        );

        server_task.abort();
        Ok(())
    }

    #[tokio::test]
    async fn udp_client_json_summary_contains_directions() -> Result<()> {
        let (target, _server_addr, server_task) = spawn_server(2_000).await?;
        let mut args = test_args(target, UdpPerfMode::Bidir);
        args.json = true;
        args.pps = Some(300);
        let out = super::run_for_test(args).await?;

        let json: Value = serde_json::from_str(out.output.trim())
            .with_context(|| format!("invalid json output: {}", out.output.trim()))?;
        assert!(json.get("forward").is_some(), "missing forward field");
        assert!(json.get("reverse").is_some(), "missing reverse field");
        assert!(
            !out.output.contains("interval mode="),
            "json output should not contain interval text"
        );

        server_task.abort();
        Ok(())
    }

    #[tokio::test]
    async fn udp_client_json_mode_does_not_emit_startup_log_line() -> Result<()> {
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone, Default)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl<'a> MakeWriter<'a> for Buf {
            type Writer = BufWriter;
            fn make_writer(&'a self) -> Self::Writer {
                BufWriter(self.0.clone())
            }
        }
        struct BufWriter(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for BufWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().expect("lock").extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let (target, _server_addr, server_task) = spawn_server(2_000).await?;
        let mut args = test_args(target, UdpPerfMode::Bidir);
        args.json = true;
        args.pps = Some(300);

        let writer = Buf::default();
        let captured = writer.0.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(writer)
            .with_ansi(false)
            .without_time()
            .finish();

        let _guard = tracing::subscriber::set_default(subscriber);
        let _out = super::run_for_test(args).await?;
        let logs = String::from_utf8(captured.lock().expect("lock").clone())
            .context("captured logs should be utf8")?;
        assert!(
            !logs.contains("udp bench client start"),
            "json mode should not emit startup log, got logs: {logs}"
        );

        server_task.abort();
        Ok(())
    }

    async fn spawn_server(
        interval_ms: u64,
    ) -> Result<(String, SocketAddr, tokio::task::JoinHandle<Result<()>>)> {
        let listen_addr = reserve_udp_addr()?;
        let server_addr = listen_addr;
        let target = server_addr.to_string();
        let interval = NonZeroU64::new(interval_ms).context("interval must be non-zero")?;
        let task = tokio::spawn(async move {
            udp_server::run(udp_server::CmdArgs {
                listen: listen_addr.to_string(),
                interval,
            })
            .await
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        Ok((target, server_addr, task))
    }

    fn reserve_udp_addr() -> Result<SocketAddr> {
        let sock = std::net::UdpSocket::bind("127.0.0.1:0")
            .with_context(|| "bind test udp addr failed")?;
        let addr = sock.local_addr().with_context(|| "local_addr failed")?;
        Ok(addr)
    }

    fn test_args(target: String, mode: UdpPerfMode) -> CmdArgs {
        CmdArgs {
            target,
            mode,
            time: 1,
            len: 96,
            bitrate: None,
            pps: Some(300),
            interval: NonZeroU64::new(100).expect("nonzero"),
            warmup: 0,
            json: false,
        }
    }
}
