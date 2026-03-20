use std::{
    cmp::min,
    io::{self, Write as _},
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
    UdpPerfMode, UdpPerfReport, UdpPerfStart, UdpPerfStats, UdpPerfStop, UdpPerfSummary,
    UdpPerfWirePacket, MAX_UDP_DATAGRAM_PAYLOAD_LEN, MAX_UDP_SAFE_PAYLOAD_LEN,
};

const DEFAULT_PPS: u64 = 1_000;
const FINAL_REPORT_TIMEOUT: Duration = Duration::from_secs(2);
const START_RETRY_INTERVAL: Duration = Duration::from_millis(100);
const MAX_INITIAL_FORWARD_SEND_DELAY: Duration = Duration::from_millis(10);

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

pub async fn run(args: CmdArgs) -> Result<()> {
    let output_policy = OutputPolicy {
        stream_interval: !args.json,
        collect_interval: false,
    };
    let out = run_inner(args, output_policy).await?;
    print!("{}", out.output);
    Ok(())
}

async fn run_inner(args: CmdArgs, output_policy: OutputPolicy) -> Result<ClientRunOutput> {
    ensure!(
        args.bitrate.is_none() || args.pps.is_none(),
        "--bitrate and --pps are mutually exclusive"
    );
    ensure!(args.len > 0, "--len must be greater than 0");
    ensure!(
        args.len <= MAX_UDP_SAFE_PAYLOAD_LEN,
        "--len must be <= {MAX_UDP_SAFE_PAYLOAD_LEN} so encoded udp perf datagram stays within {MAX_UDP_DATAGRAM_PAYLOAD_LEN} bytes"
    );
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

    let socket = bind_socket_for_target(target).await?;
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

    let start = UdpPerfStart {
        session_id,
        mode: args.mode,
        payload_len,
        packets_per_second: pps,
        duration_micros,
        report_interval_micros,
    };
    send_control(&socket, UdpPerfControlPacket::Start(start.clone())).await?;

    let start_sent_at = Instant::now();
    let wait_for_start_confirmation = mode_waits_for_start_confirmation(args.mode, pps, duration);
    let mut stop_at = if wait_for_start_confirmation {
        None
    } else {
        Some(stop_deadline(start_sent_at, duration)?)
    };
    let mut stop_sent = false;
    let mut final_deadline: Option<Instant> = None;
    let mut next_start_retry_at = next_start_retry_deadline(start_sent_at)?;
    let mut start_confirmation_deadline = if wait_for_start_confirmation {
        Some(start_confirmation_timeout(start_sent_at, duration)?)
    } else {
        None
    };
    let mut next_send_at = if wait_for_start_confirmation {
        None
    } else {
        first_send_deadline(start_sent_at, args.mode, pps)
    };
    let mut send_seq: u64 = 0;
    let mut client_rx = ClientRxStats::default();
    let mut final_report: Option<UdpPerfReport> = None;
    let mut final_report_drain_deadline: Option<Instant> = None;
    let mut recv_buf = vec![0u8; 64 * 1024];
    let mut output = String::new();

    loop {
        let now = Instant::now();

        if !stop_sent {
            if let Some(deadline) = start_confirmation_deadline {
                if now >= deadline {
                    bail!("timeout waiting start confirmation");
                }
            }

            if stop_at.is_some_and(|deadline| now >= deadline) {
                send_control(
                    &socket,
                    UdpPerfControlPacket::Stop(UdpPerfStop { session_id }),
                )
                .await?;
                stop_sent = true;
                next_start_retry_at = None;
                next_send_at = None;
                start_confirmation_deadline = None;
                final_deadline = Some(
                    now.checked_add(FINAL_REPORT_TIMEOUT)
                        .context("final report deadline overflow")?,
                );
            }
        }

        if !stop_sent {
            if let Some(due) = next_start_retry_at {
                if due <= now {
                    let retry_sent_at =
                        send_start_retry_with_hello(&socket, session_id, args.mode, &start).await?;
                    next_start_retry_at = next_start_retry_deadline(retry_sent_at)?;
                }
            }
            if let Some(due) = next_send_at {
                if due <= now {
                    send_seq = send_seq.saturating_add(1);
                    send_forward_data(&socket, session_id, send_seq, payload_len).await?;
                    next_send_at = next_send_deadline(now, pps);
                }
            }
        }

        if let Some(report) = consume_final_report_or_timeout(
            &mut final_report,
            now,
            final_deadline,
            final_report_drain_deadline,
        )? {
            let final_summary = build_client_view_summary(&report.summary, &client_rx, None);
            output.push_str(&render_final_summary(&final_summary, args.json)?);
            return Ok(ClientRunOutput {
                final_report: report,
                client_rx,
                output,
            });
        }

        if let Some(deadline) = next_loop_deadline(
            next_send_at,
            next_start_retry_at,
            if stop_sent { None } else { stop_at },
            start_confirmation_deadline,
            if final_report.is_some() {
                final_report_drain_deadline
            } else {
                final_deadline
            },
        ) {
            let sleep = tokio::time::sleep_until(deadline);
            tokio::pin!(sleep);
            tokio::select! {
                recv = socket.recv(&mut recv_buf) => {
                    let n = recv.with_context(|| "udp-client recv failed")?;
                    let had_final_report = final_report.is_some();
                    if let Some(report) = handle_incoming_packet(
                        &recv_buf[..n],
                        &mut output,
                        !args.json,
                        output_policy,
                        &mut client_rx,
                    )? {
                        if report.is_final {
                            final_report_drain_deadline = schedule_final_report_drain_deadline(
                                &report,
                                &client_rx,
                                Instant::now(),
                            )?;
                            final_report = Some(report);
                        }
                    }
                    next_start_retry_at = None;
                    if start_confirmation_deadline.take().is_some() {
                        let anchor = Instant::now();
                        stop_at = Some(stop_deadline(anchor, duration)?);
                        next_send_at = first_send_deadline(anchor, args.mode, pps);
                    }
                    if had_final_report {
                        refresh_final_report_drain_deadline(
                            final_report.as_ref(),
                            &client_rx,
                            &mut final_report_drain_deadline,
                            Instant::now(),
                        )?;
                    }
                }
                _ = &mut sleep => {}
            }
        } else {
            let n = socket
                .recv(&mut recv_buf)
                .await
                .with_context(|| "udp-client recv failed")?;
            let had_final_report = final_report.is_some();
            if let Some(report) = handle_incoming_packet(
                &recv_buf[..n],
                &mut output,
                !args.json,
                output_policy,
                &mut client_rx,
            )? {
                if report.is_final {
                    final_report_drain_deadline =
                        schedule_final_report_drain_deadline(&report, &client_rx, Instant::now())?;
                    final_report = Some(report);
                }
            }
            next_start_retry_at = None;
            if start_confirmation_deadline.take().is_some() {
                let anchor = Instant::now();
                stop_at = Some(stop_deadline(anchor, duration)?);
                next_send_at = first_send_deadline(anchor, args.mode, pps);
            }
            if had_final_report {
                refresh_final_report_drain_deadline(
                    final_report.as_ref(),
                    &client_rx,
                    &mut final_report_drain_deadline,
                    Instant::now(),
                )?;
            }
        }
    }
}

fn consume_final_report_or_timeout(
    final_report: &mut Option<UdpPerfReport>,
    now: Instant,
    final_deadline: Option<Instant>,
    final_report_drain_deadline: Option<Instant>,
) -> Result<Option<UdpPerfReport>> {
    if final_report.is_some() {
        if final_report_drain_deadline.is_some_and(|deadline| now < deadline) {
            return Ok(None);
        }
        return Ok(final_report.take());
    }
    if let Some(deadline) = final_deadline {
        if now >= deadline {
            bail!("timeout waiting final report");
        }
    }
    Ok(None)
}

fn schedule_final_report_drain_deadline(
    report: &UdpPerfReport,
    client_rx: &ClientRxStats,
    now: Instant,
) -> Result<Option<Instant>> {
    if matches!(
        report.summary.mode,
        UdpPerfMode::Reverse | UdpPerfMode::Bidir
    ) && client_rx.has_pending_reverse_packets(report)
    {
        return Ok(Some(
            now.checked_add(reverse_tail_idle_timeout())
                .context("final reverse drain deadline overflow")?,
        ));
    }
    Ok(None)
}

fn next_loop_deadline(
    next_send_at: Option<Instant>,
    next_start_retry_at: Option<Instant>,
    stop_at: Option<Instant>,
    start_confirmation_deadline: Option<Instant>,
    final_deadline: Option<Instant>,
) -> Option<Instant> {
    let mut next = next_send_at;
    if let Some(t) = next_start_retry_at {
        next = Some(next.map_or(t, |cur| min(cur, t)));
    }
    if let Some(t) = stop_at {
        next = Some(next.map_or(t, |cur| min(cur, t)));
    }
    if let Some(t) = start_confirmation_deadline {
        next = Some(next.map_or(t, |cur| min(cur, t)));
    }
    if let Some(t) = final_deadline {
        next = Some(next.map_or(t, |cur| min(cur, t)));
    }
    next
}

fn next_start_retry_deadline(now: Instant) -> Result<Option<Instant>> {
    Ok(Some(
        now.checked_add(START_RETRY_INTERVAL)
            .context("start retry deadline overflow")?,
    ))
}

fn start_confirmation_timeout(started_at: Instant, duration: Duration) -> Result<Instant> {
    stop_deadline(stop_deadline(started_at, duration)?, FINAL_REPORT_TIMEOUT)
}

fn refresh_final_report_drain_deadline(
    final_report: Option<&UdpPerfReport>,
    client_rx: &ClientRxStats,
    final_report_drain_deadline: &mut Option<Instant>,
    now: Instant,
) -> Result<()> {
    if let Some(report) = final_report {
        *final_report_drain_deadline =
            schedule_final_report_drain_deadline(report, client_rx, now)?;
    }
    Ok(())
}

fn reverse_tail_idle_timeout() -> Duration {
    FINAL_REPORT_TIMEOUT
}

fn send_interval(pps: u64) -> Option<Duration> {
    if pps == 0 {
        return None;
    }
    let interval_micros = (1_000_000 / pps).max(1);
    Some(Duration::from_micros(interval_micros))
}

#[cfg(test)]
async fn run_for_test(args: CmdArgs) -> Result<ClientRunOutput> {
    run_inner(
        args,
        OutputPolicy {
            stream_interval: false,
            collect_interval: true,
        },
    )
    .await
}

#[derive(Debug)]
struct ClientRunOutput {
    final_report: UdpPerfReport,
    client_rx: ClientRxStats,
    output: String,
}

#[derive(Debug, Clone, Copy)]
struct OutputPolicy {
    stream_interval: bool,
    collect_interval: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IntervalReportSync {
    Synced,
    UnsyncedGap,
    Stale,
}

#[derive(Debug, Default)]
struct ClientRxStats {
    reverse_packets: u64,
    reverse_bytes: u64,
    reverse_stats: UdpPerfStats,
    last_interval_report_seq: Option<u64>,
}

impl ClientRxStats {
    fn on_data_packet(&mut self, data: &UdpPerfDataPacket) {
        if matches!(data.header.direction, UdpPerfDirection::Reverse) {
            self.reverse_packets = self.reverse_packets.saturating_add(1);
            self.reverse_bytes = self.reverse_bytes.saturating_add(data.payload.len() as u64);
            self.reverse_stats.record_data_packet(data);
        }
    }

    fn reverse_summary(
        &self,
        total_elapsed_micros: u64,
        interval_elapsed_micros: u64,
    ) -> super::udp_perf::UdpPerfDirectionSummary {
        self.reverse_stats
            .build_summary(
                0,
                UdpPerfMode::Reverse,
                total_elapsed_micros,
                interval_elapsed_micros,
            )
            .reverse
    }

    fn take_interval_summary(
        &mut self,
        total_elapsed_micros: u64,
        interval_elapsed_micros: u64,
    ) -> super::udp_perf::UdpPerfCountersSummary {
        let interval = self
            .reverse_summary(total_elapsed_micros, interval_elapsed_micros)
            .interval;
        self.reverse_stats.reset_interval();
        interval
    }

    fn observe_interval_report(&mut self, report_seq: u64) -> IntervalReportSync {
        match self.last_interval_report_seq {
            None => {
                self.last_interval_report_seq = Some(report_seq);
                if report_seq == 1 {
                    IntervalReportSync::Synced
                } else {
                    IntervalReportSync::UnsyncedGap
                }
            }
            Some(last) => {
                let expected = last.saturating_add(1);
                if report_seq == expected {
                    self.last_interval_report_seq = Some(report_seq);
                    IntervalReportSync::Synced
                } else if report_seq > expected {
                    self.last_interval_report_seq = Some(report_seq);
                    IntervalReportSync::UnsyncedGap
                } else {
                    IntervalReportSync::Stale
                }
            }
        }
    }

    fn has_pending_reverse_packets(&self, report: &UdpPerfReport) -> bool {
        if !matches!(
            report.summary.mode,
            UdpPerfMode::Reverse | UdpPerfMode::Bidir
        ) {
            return false;
        }

        let reverse = self.reverse_summary(
            report.summary.total_elapsed_micros,
            report.summary.interval_elapsed_micros,
        );
        let unique_packets = reverse
            .total
            .packets
            .saturating_sub(reverse.total.duplicate);
        unique_packets < report.summary.reverse.total.packets
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
        return Ok(pps);
    }
    Ok(DEFAULT_PPS)
}

fn mode_waits_for_start_confirmation(mode: UdpPerfMode, pps: u64, duration: Duration) -> bool {
    !duration.is_zero() && pps > 0 && matches!(mode, UdpPerfMode::Reverse | UdpPerfMode::Bidir)
}

fn stop_deadline(started_at: Instant, duration: Duration) -> Result<Instant> {
    started_at
        .checked_add(duration)
        .context("local stop deadline overflow")
}

fn first_send_deadline(started_at: Instant, mode: UdpPerfMode, pps: u64) -> Option<Instant> {
    if matches!(mode, UdpPerfMode::Forward | UdpPerfMode::Bidir) {
        let delay = std::cmp::min(send_interval(pps)?, MAX_INITIAL_FORWARD_SEND_DELAY);
        return started_at.checked_add(delay);
    } else {
        None
    }
}

fn next_send_deadline(now: Instant, pps: u64) -> Option<Instant> {
    now.checked_add(send_interval(pps)?)
}

async fn bind_socket_for_target(target: SocketAddr) -> Result<UdpSocket> {
    let bind_addr = match target {
        SocketAddr::V4(_) => "0.0.0.0:0",
        SocketAddr::V6(_) => "[::]:0",
    };
    UdpSocket::bind(bind_addr)
        .await
        .with_context(|| format!("udp-client bind failed for target family [{target}]"))
}

async fn send_control(socket: &UdpSocket, pkt: UdpPerfControlPacket) -> Result<()> {
    let bytes = UdpPerfWirePacket::Control(pkt).encode()?;
    socket
        .send(&bytes)
        .await
        .with_context(|| "udp-client send control packet failed")?;
    Ok(())
}

async fn send_start_retry_with_hello(
    socket: &UdpSocket,
    session_id: u64,
    mode: UdpPerfMode,
    start: &UdpPerfStart,
) -> Result<Instant> {
    send_control(
        socket,
        UdpPerfControlPacket::Hello(UdpPerfHello { session_id, mode }),
    )
    .await?;
    send_control(socket, UdpPerfControlPacket::Start(start.clone())).await?;
    Ok(Instant::now())
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
    output_policy: OutputPolicy,
    client_rx: &mut ClientRxStats,
) -> Result<Option<UdpPerfReport>> {
    let wire = UdpPerfWirePacket::decode(bytes)?;
    match wire {
        UdpPerfWirePacket::Control(UdpPerfControlPacket::Report(report)) => {
            if !report.is_final {
                match client_rx.observe_interval_report(report.report_seq) {
                    IntervalReportSync::Stale => {}
                    sync_state => {
                        let reverse_interval = client_rx.take_interval_summary(
                            report.summary.total_elapsed_micros,
                            report.summary.interval_elapsed_micros,
                        );
                        if emit_interval && matches!(sync_state, IntervalReportSync::Synced) {
                            let merged = build_client_view_summary(
                                &report.summary,
                                client_rx,
                                Some(reverse_interval),
                            );
                            let line = render_interval_summary(&merged);
                            if output_policy.collect_interval {
                                output.push_str(&line);
                            }
                            if output_policy.stream_interval {
                                print!("{line}");
                                io::stdout().flush().context("flush stdout failed")?;
                            }
                        }
                    }
                }
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

fn build_client_view_summary(
    server: &UdpPerfSummary,
    client_rx: &ClientRxStats,
    reverse_interval_override: Option<super::udp_perf::UdpPerfCountersSummary>,
) -> UdpPerfSummary {
    let mut merged = server.clone();
    if matches!(server.mode, UdpPerfMode::Reverse | UdpPerfMode::Bidir) {
        let reverse_summary =
            client_rx.reverse_summary(server.total_elapsed_micros, server.interval_elapsed_micros);
        merged.reverse.total = merge_reverse_total(
            &server.reverse.total,
            &reverse_summary.total,
            server.total_elapsed_micros,
        );
        merged.reverse.interval = reverse_interval_override.unwrap_or(reverse_summary.interval);
        merged.total = merge_counters(
            &merged.forward.total,
            &merged.reverse.total,
            server.total_elapsed_micros,
        );
        merged.interval = merge_counters(
            &merged.forward.interval,
            &merged.reverse.interval,
            server.interval_elapsed_micros,
        );
    }
    merged
}

fn merge_reverse_total(
    server_total: &super::udp_perf::UdpPerfCountersSummary,
    client_total: &super::udp_perf::UdpPerfCountersSummary,
    elapsed_micros: u64,
) -> super::udp_perf::UdpPerfCountersSummary {
    let unique_packets = client_total.packets.saturating_sub(client_total.duplicate);
    let packets = server_total.packets;
    let bytes = server_total.bytes;
    super::udp_perf::UdpPerfCountersSummary {
        bytes,
        packets,
        loss: packets.saturating_sub(unique_packets),
        reorder: client_total.reorder,
        duplicate: client_total.duplicate,
        mbps: bytes_to_mbps_local(bytes, elapsed_micros),
        pps: packets_to_pps_local(packets, elapsed_micros),
    }
}

fn merge_counters(
    a: &super::udp_perf::UdpPerfCountersSummary,
    b: &super::udp_perf::UdpPerfCountersSummary,
    elapsed_micros: u64,
) -> super::udp_perf::UdpPerfCountersSummary {
    let bytes = a.bytes.saturating_add(b.bytes);
    let packets = a.packets.saturating_add(b.packets);
    super::udp_perf::UdpPerfCountersSummary {
        bytes,
        packets,
        loss: a.loss.saturating_add(b.loss),
        reorder: a.reorder.saturating_add(b.reorder),
        duplicate: a.duplicate.saturating_add(b.duplicate),
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

    use anyhow::{bail, ensure, Context, Result};
    use serde_json::Value;
    use tokio::net::UdpSocket;
    use tokio::time::Instant;

    use super::super::udp_perf::{
        UdpPerfControlPacket, UdpPerfCountersSummary, UdpPerfDataHeader, UdpPerfDataPacket,
        UdpPerfDirection, UdpPerfDirectionSummary, UdpPerfReport, UdpPerfSummary,
        UdpPerfWirePacket,
    };
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

    #[test]
    fn udp_client_prefers_received_final_report_over_timeout_deadline() -> Result<()> {
        let now = Instant::now();
        let mut final_report = Some(UdpPerfReport {
            report_seq: 9,
            is_final: true,
            summary: empty_summary(UdpPerfMode::Forward),
        });
        let report =
            super::consume_final_report_or_timeout(&mut final_report, now, Some(now), None)?
                .context("expected final report to be consumed before timeout")?;
        assert!(report.is_final);
        assert_eq!(report.report_seq, 9);
        assert!(final_report.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn udp_client_json_reverse_interval_tracks_protocol_window() -> Result<()> {
        let (target, _server_addr, server_task) = spawn_server(100).await?;
        let mut args = test_args(target, UdpPerfMode::Reverse);
        args.time = 2;
        args.pps = Some(300);
        args.json = true;

        let out = super::run_for_test(args).await?;
        let json: Value = serde_json::from_str(out.output.trim())
            .with_context(|| format!("invalid json output: {}", out.output.trim()))?;
        let reverse_total = json
            .pointer("/reverse/total/packets")
            .and_then(Value::as_u64)
            .context("missing reverse.total.packets")?;
        let reverse_interval = json
            .pointer("/reverse/interval/packets")
            .and_then(Value::as_u64)
            .context("missing reverse.interval.packets")?;

        assert!(reverse_total > 0, "reverse total should be > 0");
        assert!(reverse_interval > 0, "reverse interval should be > 0");
        assert!(
            reverse_interval < reverse_total,
            "reverse interval should be smaller than total in json mode when interval < time, interval={reverse_interval}, total={reverse_total}"
        );

        server_task.abort();
        Ok(())
    }

    #[test]
    fn udp_client_bitrate_below_one_packet_resolves_to_zero_pps() -> Result<()> {
        let mut args = test_args("127.0.0.1:9".to_string(), UdpPerfMode::Forward);
        args.len = 1200;
        args.bitrate = Some(100);
        args.pps = None;
        assert_eq!(super::resolve_pps(&args)?, 0);
        Ok(())
    }

    #[test]
    fn udp_client_forward_and_bidir_first_send_deadline_adds_short_start_guard() -> Result<()> {
        let started_at = Instant::now();
        let pps = 100;
        let forward =
            super::first_send_deadline(started_at, UdpPerfMode::Forward, pps).context("forward")?;
        let bidir =
            super::first_send_deadline(started_at, UdpPerfMode::Bidir, pps).context("bidir")?;

        let expected = started_at
            .checked_add(Duration::from_millis(10))
            .context("expected deadline overflow")?;
        assert_eq!(forward, expected);
        assert_eq!(bidir, expected);
        Ok(())
    }

    #[test]
    fn udp_client_build_client_view_summary_preserves_reverse_loss_reorder_and_duplicate(
    ) -> Result<()> {
        let mut client_rx = super::ClientRxStats::default();
        for seq in [1, 3, 2, 2] {
            client_rx.on_data_packet(&UdpPerfDataPacket {
                header: UdpPerfDataHeader {
                    session_id: 1,
                    stream_id: 1,
                    direction: UdpPerfDirection::Reverse,
                    seq,
                    send_ts_micros: seq * 1_000,
                    payload_len: 32,
                },
                payload: vec![0x5a; 32],
            });
        }

        let mut server = empty_summary(UdpPerfMode::Reverse);
        server.total_elapsed_micros = 1_000_000;
        server.reverse.total = UdpPerfCountersSummary {
            packets: 3,
            bytes: 96,
            ..UdpPerfCountersSummary::default()
        };
        server.total = server.reverse.total.clone();

        let merged = super::build_client_view_summary(&server, &client_rx, None);
        assert_eq!(merged.reverse.total.packets, 3);
        assert_eq!(merged.reverse.total.bytes, 96);
        assert_eq!(merged.reverse.total.loss, 0);
        assert_eq!(merged.reverse.total.reorder, 1);
        assert_eq!(merged.reverse.total.duplicate, 1);
        Ok(())
    }

    #[test]
    fn udp_client_build_client_view_summary_keeps_server_reverse_totals_for_tail_loss() -> Result<()>
    {
        let mut client_rx = super::ClientRxStats::default();
        for seq in 1..=4 {
            client_rx.on_data_packet(&UdpPerfDataPacket {
                header: UdpPerfDataHeader {
                    session_id: 1,
                    stream_id: 1,
                    direction: UdpPerfDirection::Reverse,
                    seq,
                    send_ts_micros: seq * 1_000,
                    payload_len: 32,
                },
                payload: vec![0x5a; 32],
            });
        }

        let mut server = empty_summary(UdpPerfMode::Reverse);
        server.total_elapsed_micros = 1_000_000;
        server.reverse.total = UdpPerfCountersSummary {
            packets: 5,
            bytes: 160,
            ..UdpPerfCountersSummary::default()
        };
        server.total = server.reverse.total.clone();

        let merged = super::build_client_view_summary(&server, &client_rx, None);
        assert_eq!(merged.reverse.total.packets, 5);
        assert_eq!(merged.reverse.total.bytes, 160);
        assert_eq!(merged.reverse.total.loss, 1);
        Ok(())
    }

    #[tokio::test]
    async fn udp_client_forward_mode_low_pps_still_sends_first_packet() -> Result<()> {
        let (target, _server_addr, server_task) = spawn_server(2_000).await?;
        let mut args = test_args(target, UdpPerfMode::Forward);
        args.json = true;
        args.pps = Some(1);

        let out = super::run_for_test(args).await?;
        let json: Value = serde_json::from_str(out.output.trim())
            .with_context(|| format!("invalid json output: {}", out.output.trim()))?;
        let forward_packets = json
            .pointer("/forward/total/packets")
            .and_then(Value::as_u64)
            .context("missing forward.total.packets")?;

        assert_eq!(
            forward_packets,
            1,
            "forward low-pps run should send one packet, output={}",
            out.output.trim()
        );

        server_task.abort();
        Ok(())
    }

    #[tokio::test]
    async fn udp_client_forward_mode_low_pps_sends_one_data_packet() -> Result<()> {
        let (target, server_task) = spawn_forward_packet_counting_server().await?;
        let mut args = test_args(target, UdpPerfMode::Forward);
        args.pps = Some(1);
        args.json = true;

        let _out = super::run_for_test(args).await?;

        server_task.await??;
        Ok(())
    }

    #[tokio::test]
    async fn udp_client_can_connect_to_ipv6_target() -> Result<()> {
        let Some((target, _server_addr, server_task)) = spawn_ipv6_server(2_000).await? else {
            return Ok(());
        };
        let mut args = test_args(target, UdpPerfMode::Forward);
        args.json = true;
        args.pps = Some(1);

        let out = super::run_for_test(args).await?;
        let json: Value = serde_json::from_str(out.output.trim())
            .with_context(|| format!("invalid json output: {}", out.output.trim()))?;
        let total_packets = json
            .pointer("/total/packets")
            .and_then(Value::as_u64)
            .context("missing total.packets")?;

        assert!(
            total_packets > 0,
            "ipv6 target run should complete with traffic, output={}",
            out.output.trim()
        );

        server_task.abort();
        Ok(())
    }

    #[tokio::test]
    async fn udp_client_forward_mode_duration_zero_sends_no_data() -> Result<()> {
        let (target, _server_addr, server_task) = spawn_server(2_000).await?;
        let mut args = test_args(target, UdpPerfMode::Forward);
        args.time = 0;
        args.pps = Some(300);

        let out = super::run_for_test(args).await?;

        assert!(out.final_report.is_final);
        assert_eq!(out.final_report.summary.mode, UdpPerfMode::Forward);
        assert_eq!(
            out.final_report.summary.forward.total.packets, 0,
            "time=0 should not send any forward packets"
        );

        server_task.abort();
        Ok(())
    }

    #[tokio::test]
    async fn udp_client_final_summary_includes_reverse_tail_after_final_report() -> Result<()> {
        let (target, server_task) = spawn_reverse_tail_after_final_report_server().await?;
        let mut args = test_args(target, UdpPerfMode::Reverse);
        args.time = 0;
        args.len = 32;
        args.json = true;

        let out = tokio::time::timeout(Duration::from_secs(1), super::run_for_test(args))
            .await
            .context("udp-client should finish in time")??;

        assert_eq!(
            out.client_rx.reverse_packets, 1,
            "client should keep draining tail reverse packet after final report"
        );
        let json: Value = serde_json::from_str(out.output.trim())
            .with_context(|| format!("invalid json output: {}", out.output.trim()))?;
        assert_eq!(
            json.pointer("/reverse/total/packets")
                .and_then(Value::as_u64),
            Some(1),
            "final reverse total should include tail packet received after final report"
        );

        server_task.await??;
        Ok(())
    }

    #[tokio::test]
    async fn udp_client_retries_start_when_first_start_is_lost() -> Result<()> {
        let (target, server_task) = spawn_drop_first_start_reverse_server().await?;
        let mut args = test_args(target, UdpPerfMode::Reverse);
        args.time = 1;
        args.pps = Some(100);
        args.json = true;

        let out = tokio::time::timeout(Duration::from_secs(3), super::run_for_test(args))
            .await
            .context("udp-client should recover from one lost START")??;
        let json: Value = serde_json::from_str(out.output.trim())
            .with_context(|| format!("invalid json output: {}", out.output.trim()))?;
        let reverse_packets = json
            .pointer("/reverse/total/packets")
            .and_then(Value::as_u64)
            .context("missing reverse.total.packets")?;

        assert!(
            reverse_packets > 0,
            "reverse run should complete after START retry"
        );

        server_task.await??;
        Ok(())
    }

    #[tokio::test]
    async fn udp_client_reverse_retry_rebases_stop_deadline_on_first_server_packet() -> Result<()> {
        let (target, server_task) =
            spawn_drop_first_start_reverse_server_with_min_stop_delay(Duration::from_millis(950))
                .await?;
        let mut args = test_args(target, UdpPerfMode::Reverse);
        args.time = 1;
        args.pps = Some(100);
        args.json = true;

        let _ = tokio::time::timeout(Duration::from_secs(4), super::run_for_test(args))
            .await
            .context("udp-client should finish after retrying START")??;

        server_task.await??;
        Ok(())
    }

    #[tokio::test]
    async fn udp_client_retry_start_resends_hello_before_retry_start() -> Result<()> {
        let (target, server_task) = spawn_drop_initial_hello_and_start_forward_server().await?;
        let mut args = test_args(target, UdpPerfMode::Forward);
        args.time = 1;
        args.pps = Some(20);
        args.json = true;

        let _ = tokio::time::timeout(Duration::from_secs(4), super::run_for_test(args))
            .await
            .context("udp-client should finish after retrying HELLO/START")??;

        server_task.await??;
        Ok(())
    }

    #[tokio::test]
    async fn udp_client_reverse_tail_drain_handles_high_latency_tail() -> Result<()> {
        let (target, server_task) =
            spawn_reverse_tail_after_final_report_server_with_delay(Duration::from_millis(400))
                .await?;
        let mut args = test_args(target, UdpPerfMode::Reverse);
        args.time = 0;
        args.len = 32;
        args.json = true;

        let out = tokio::time::timeout(Duration::from_secs(3), super::run_for_test(args))
            .await
            .context("udp-client should finish in time")??;
        let json: Value = serde_json::from_str(out.output.trim())
            .with_context(|| format!("invalid json output: {}", out.output.trim()))?;
        assert_eq!(
            json.pointer("/reverse/total/packets")
                .and_then(Value::as_u64),
            Some(1),
            "final reverse total should include delayed tail packet after final report"
        );

        server_task.await??;
        Ok(())
    }

    #[test]
    fn udp_client_skips_unsynced_interval_after_lost_report() -> Result<()> {
        let output_policy = super::OutputPolicy {
            stream_interval: false,
            collect_interval: true,
        };
        let mut client_rx = super::ClientRxStats::default();
        let mut output = String::new();

        for seq in 1..=2 {
            client_rx.on_data_packet(&UdpPerfDataPacket {
                header: UdpPerfDataHeader {
                    session_id: 1,
                    stream_id: 1,
                    direction: UdpPerfDirection::Reverse,
                    seq,
                    send_ts_micros: seq * 1_000,
                    payload_len: 32,
                },
                payload: vec![0x5a; 32],
            });
        }

        let _ = super::handle_incoming_packet(
            &wire_report(interval_report(2, 100_000, 100_000))?,
            &mut output,
            true,
            output_policy,
            &mut client_rx,
        )?;
        assert!(
            output.is_empty(),
            "lost interval report should suppress unsynced interval output, got: {output}"
        );

        client_rx.on_data_packet(&UdpPerfDataPacket {
            header: UdpPerfDataHeader {
                session_id: 1,
                stream_id: 1,
                direction: UdpPerfDirection::Reverse,
                seq: 3,
                send_ts_micros: 3_000,
                payload_len: 32,
            },
            payload: vec![0x5a; 32],
        });
        let _ = super::handle_incoming_packet(
            &wire_report(interval_report(3, 200_000, 100_000))?,
            &mut output,
            true,
            output_policy,
            &mut client_rx,
        )?;
        assert!(
            output.contains("rev_pkts=1"),
            "next in-order interval should resync to one packet, got: {output}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn udp_perf_loopback_forward_end_to_end() -> Result<()> {
        let (target, _server_addr, server_task) = spawn_server(100).await?;
        let mut args = test_args(target, UdpPerfMode::Forward);
        args.json = true;

        let out = super::run_for_test(args).await?;
        let json: Value = serde_json::from_str(out.output.trim())
            .with_context(|| format!("invalid json output: {}", out.output.trim()))?;
        let total_packets = json
            .pointer("/total/packets")
            .and_then(Value::as_u64)
            .context("missing total.packets")?;
        let forward_packets = json
            .pointer("/forward/total/packets")
            .and_then(Value::as_u64)
            .context("missing forward.total.packets")?;

        assert!(
            total_packets > 0,
            "forward loopback total packets should be > 0"
        );
        assert!(
            forward_packets > 0,
            "forward loopback forward packets should be > 0"
        );

        server_task.abort();
        Ok(())
    }

    #[tokio::test]
    async fn udp_perf_loopback_reverse_end_to_end() -> Result<()> {
        let (target, _server_addr, server_task) = spawn_server(100).await?;
        let mut args = test_args(target, UdpPerfMode::Reverse);
        args.json = true;

        let out = super::run_for_test(args).await?;
        let json: Value = serde_json::from_str(out.output.trim())
            .with_context(|| format!("invalid json output: {}", out.output.trim()))?;
        let total_packets = json
            .pointer("/total/packets")
            .and_then(Value::as_u64)
            .context("missing total.packets")?;
        let reverse_packets = json
            .pointer("/reverse/total/packets")
            .and_then(Value::as_u64)
            .context("missing reverse.total.packets")?;

        assert!(
            total_packets > 0,
            "reverse loopback total packets should be > 0"
        );
        assert!(
            reverse_packets > 0,
            "reverse loopback reverse packets should be > 0"
        );

        server_task.abort();
        Ok(())
    }

    #[tokio::test]
    async fn udp_perf_loopback_bidir_end_to_end() -> Result<()> {
        let (target, _server_addr, server_task) = spawn_server(100).await?;
        let mut args = test_args(target, UdpPerfMode::Bidir);
        args.json = true;

        let out = super::run_for_test(args).await?;
        let json: Value = serde_json::from_str(out.output.trim())
            .with_context(|| format!("invalid json output: {}", out.output.trim()))?;
        let total_packets = json
            .pointer("/total/packets")
            .and_then(Value::as_u64)
            .context("missing total.packets")?;
        let forward_packets = json
            .pointer("/forward/total/packets")
            .and_then(Value::as_u64)
            .context("missing forward.total.packets")?;
        let reverse_packets = json
            .pointer("/reverse/total/packets")
            .and_then(Value::as_u64)
            .context("missing reverse.total.packets")?;

        assert!(
            total_packets > 0,
            "bidir loopback total packets should be > 0"
        );
        assert!(
            forward_packets > 0,
            "bidir loopback forward packets should be > 0"
        );
        assert!(
            reverse_packets > 0,
            "bidir loopback reverse packets should be > 0"
        );

        server_task.abort();
        Ok(())
    }

    #[tokio::test]
    async fn udp_client_rejects_unsafe_udp_payload_length() {
        let mut args = test_args("127.0.0.1:9".to_string(), UdpPerfMode::Forward);
        args.len = super::MAX_UDP_SAFE_PAYLOAD_LEN + 1;
        let err = super::run_for_test(args)
            .await
            .expect_err("should reject --len");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("--len must be <="),
            "unexpected error message: {msg}"
        );
    }

    async fn spawn_server(
        interval_ms: u64,
    ) -> Result<(String, SocketAddr, tokio::task::JoinHandle<Result<()>>)> {
        let listen_addr = reserve_udp_addr()?;
        let (target, server_addr, task) = spawn_server_at(listen_addr, interval_ms).await?;
        Ok((target, server_addr, task))
    }

    async fn spawn_ipv6_server(
        interval_ms: u64,
    ) -> Result<Option<(String, SocketAddr, tokio::task::JoinHandle<Result<()>>)>> {
        let Ok(listen_addr) = reserve_udp_addr_v6() else {
            return Ok(None);
        };
        let spawned = spawn_server_at(listen_addr, interval_ms).await?;
        Ok(Some(spawned))
    }

    async fn spawn_server_at(
        listen_addr: SocketAddr,
        interval_ms: u64,
    ) -> Result<(String, SocketAddr, tokio::task::JoinHandle<Result<()>>)> {
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
        wait_for_udp_server_ready(server_addr).await?;
        Ok((target, server_addr, task))
    }

    async fn wait_for_udp_server_ready(listen_addr: SocketAddr) -> Result<()> {
        for _ in 0..50 {
            match std::net::UdpSocket::bind(listen_addr) {
                Ok(sock) => {
                    drop(sock);
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
                    return Ok(());
                }
                Err(err) => {
                    return Err(err).with_context(|| {
                        format!("probe udp bench server readiness at [{listen_addr}] failed")
                    });
                }
            }
        }
        bail!("udp bench server did not bind to [{listen_addr}] in time")
    }

    fn reserve_udp_addr() -> Result<SocketAddr> {
        let sock = std::net::UdpSocket::bind("127.0.0.1:0")
            .with_context(|| "bind test udp addr failed")?;
        let addr = sock.local_addr().with_context(|| "local_addr failed")?;
        Ok(addr)
    }

    fn reserve_udp_addr_v6() -> Result<SocketAddr> {
        let sock = std::net::UdpSocket::bind("[::1]:0")
            .with_context(|| "bind test ipv6 udp addr failed")?;
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

    fn empty_summary(mode: UdpPerfMode) -> UdpPerfSummary {
        UdpPerfSummary {
            session_id: 1,
            mode,
            total_elapsed_micros: 1,
            interval_elapsed_micros: 1,
            total: UdpPerfCountersSummary::default(),
            interval: UdpPerfCountersSummary::default(),
            forward: UdpPerfDirectionSummary::default(),
            reverse: UdpPerfDirectionSummary::default(),
        }
    }

    async fn spawn_reverse_tail_after_final_report_server(
    ) -> Result<(String, tokio::task::JoinHandle<Result<()>>)> {
        spawn_reverse_tail_after_final_report_server_with_delay(Duration::from_millis(20)).await
    }

    async fn spawn_reverse_tail_after_final_report_server_with_delay(
        tail_delay: Duration,
    ) -> Result<(String, tokio::task::JoinHandle<Result<()>>)> {
        let socket = UdpSocket::bind("127.0.0.1:0").await?;
        let addr = socket.local_addr()?;
        let task = tokio::spawn(async move {
            let mut buf = vec![0u8; 64 * 1024];
            let mut client = None;
            let mut session_id = None;

            loop {
                let (n, from) = socket.recv_from(&mut buf).await?;
                match UdpPerfWirePacket::decode(&buf[..n])? {
                    UdpPerfWirePacket::Control(UdpPerfControlPacket::Hello(hello)) => {
                        client = Some(from);
                        session_id = Some(hello.session_id);
                    }
                    UdpPerfWirePacket::Control(UdpPerfControlPacket::Start(start)) => {
                        client = Some(from);
                        session_id = Some(start.session_id);
                    }
                    UdpPerfWirePacket::Control(UdpPerfControlPacket::Stop(stop)) => {
                        let peer = client.unwrap_or(from);
                        let sid = session_id.unwrap_or(stop.session_id);
                        socket
                            .send_to(
                                &wire_report(UdpPerfReport {
                                    report_seq: 1,
                                    is_final: true,
                                    summary: UdpPerfSummary {
                                        session_id: sid,
                                        mode: UdpPerfMode::Reverse,
                                        total_elapsed_micros: 20_000,
                                        interval_elapsed_micros: 20_000,
                                        total: UdpPerfCountersSummary {
                                            packets: 1,
                                            bytes: 32,
                                            ..UdpPerfCountersSummary::default()
                                        },
                                        interval: UdpPerfCountersSummary {
                                            packets: 1,
                                            bytes: 32,
                                            ..UdpPerfCountersSummary::default()
                                        },
                                        forward: UdpPerfDirectionSummary::default(),
                                        reverse: UdpPerfDirectionSummary {
                                            total: UdpPerfCountersSummary {
                                                packets: 1,
                                                bytes: 32,
                                                ..UdpPerfCountersSummary::default()
                                            },
                                            interval: UdpPerfCountersSummary {
                                                packets: 1,
                                                bytes: 32,
                                                ..UdpPerfCountersSummary::default()
                                            },
                                        },
                                    },
                                })?,
                                peer,
                            )
                            .await?;
                        tokio::time::sleep(tail_delay).await;
                        socket
                            .send_to(&wire_reverse_data(sid, 1, 32)?, peer)
                            .await?;
                        return Ok(());
                    }
                    _ => {}
                }
            }
        });
        Ok((addr.to_string(), task))
    }

    async fn spawn_drop_first_start_reverse_server(
    ) -> Result<(String, tokio::task::JoinHandle<Result<()>>)> {
        spawn_drop_first_start_reverse_server_with_min_stop_delay(Duration::ZERO).await
    }

    async fn spawn_drop_first_start_reverse_server_with_min_stop_delay(
        min_stop_delay: Duration,
    ) -> Result<(String, tokio::task::JoinHandle<Result<()>>)> {
        let socket = UdpSocket::bind("127.0.0.1:0").await?;
        let addr = socket.local_addr()?;
        let task = tokio::spawn(async move {
            let mut buf = vec![0u8; 64 * 1024];
            let mut client = None;
            let mut session_id = None;
            let mut start_seen = 0u64;
            let mut accepted_start_at = None;

            loop {
                let (n, from) = socket.recv_from(&mut buf).await?;
                match UdpPerfWirePacket::decode(&buf[..n])? {
                    UdpPerfWirePacket::Control(UdpPerfControlPacket::Hello(hello)) => {
                        client = Some(from);
                        session_id = Some(hello.session_id);
                    }
                    UdpPerfWirePacket::Control(UdpPerfControlPacket::Start(start)) => {
                        client = Some(from);
                        session_id = Some(start.session_id);
                        start_seen += 1;
                        if start_seen == 1 {
                            continue;
                        }

                        accepted_start_at = Some(Instant::now());
                        socket
                            .send_to(&wire_reverse_data(start.session_id, 1, 32)?, from)
                            .await?;
                    }
                    UdpPerfWirePacket::Control(UdpPerfControlPacket::Stop(stop)) => {
                        ensure!(start_seen >= 2, "client did not retry START before STOP");
                        if !min_stop_delay.is_zero() {
                            let accepted_start_at =
                                accepted_start_at.context("missing accepted start timestamp")?;
                            let elapsed = accepted_start_at.elapsed();
                            ensure!(
                                elapsed >= min_stop_delay,
                                "stop arrived too early after accepted START: {:?} < {:?}",
                                elapsed,
                                min_stop_delay
                            );
                        }
                        let sid = session_id.unwrap_or(stop.session_id);
                        let peer = client.unwrap_or(from);
                        socket
                            .send_to(
                                &wire_report(UdpPerfReport {
                                    report_seq: 1,
                                    is_final: true,
                                    summary: UdpPerfSummary {
                                        session_id: sid,
                                        mode: UdpPerfMode::Reverse,
                                        total_elapsed_micros: 1_000_000,
                                        interval_elapsed_micros: 1_000_000,
                                        total: UdpPerfCountersSummary {
                                            packets: 1,
                                            bytes: 32,
                                            ..UdpPerfCountersSummary::default()
                                        },
                                        interval: UdpPerfCountersSummary {
                                            packets: 1,
                                            bytes: 32,
                                            ..UdpPerfCountersSummary::default()
                                        },
                                        forward: UdpPerfDirectionSummary::default(),
                                        reverse: UdpPerfDirectionSummary {
                                            total: UdpPerfCountersSummary {
                                                packets: 1,
                                                bytes: 32,
                                                ..UdpPerfCountersSummary::default()
                                            },
                                            interval: UdpPerfCountersSummary {
                                                packets: 1,
                                                bytes: 32,
                                                ..UdpPerfCountersSummary::default()
                                            },
                                        },
                                    },
                                })?,
                                peer,
                            )
                            .await?;
                        return Ok(());
                    }
                    _ => {}
                }
            }
        });
        Ok((addr.to_string(), task))
    }

    async fn spawn_drop_initial_hello_and_start_forward_server(
    ) -> Result<(String, tokio::task::JoinHandle<Result<()>>)> {
        let socket = UdpSocket::bind("127.0.0.1:0").await?;
        let addr = socket.local_addr()?;
        let task = tokio::spawn(async move {
            let mut buf = vec![0u8; 64 * 1024];
            let mut client = None;
            let mut session_id = None;
            let mut first_hello_dropped = false;
            let mut first_start_dropped = false;
            let mut retry_hello_seen = false;
            let mut start_accepted = false;
            let mut forward_packets = 0u64;

            loop {
                let (n, from) = socket.recv_from(&mut buf).await?;
                match UdpPerfWirePacket::decode(&buf[..n])? {
                    UdpPerfWirePacket::Control(UdpPerfControlPacket::Hello(hello)) => {
                        client = Some(from);
                        session_id = Some(hello.session_id);
                        if !first_hello_dropped {
                            first_hello_dropped = true;
                            continue;
                        }
                        retry_hello_seen = true;
                    }
                    UdpPerfWirePacket::Control(UdpPerfControlPacket::Start(start)) => {
                        client = Some(from);
                        session_id = Some(start.session_id);
                        if !first_start_dropped {
                            first_start_dropped = true;
                            continue;
                        }
                        ensure!(
                            retry_hello_seen,
                            "client retried START without resending HELLO"
                        );
                        retry_hello_seen = false;
                        start_accepted = true;
                    }
                    UdpPerfWirePacket::Data(data) => {
                        if start_accepted
                            && matches!(data.header.direction, UdpPerfDirection::Forward)
                        {
                            forward_packets = forward_packets.saturating_add(1);
                        }
                    }
                    UdpPerfWirePacket::Control(UdpPerfControlPacket::Stop(stop)) => {
                        ensure!(first_hello_dropped, "client did not send initial HELLO");
                        ensure!(first_start_dropped, "client did not send initial START");
                        ensure!(start_accepted, "client never recovered after retry");
                        ensure!(
                            forward_packets > 0,
                            "client sent no forward data after retry"
                        );
                        let sid = session_id.unwrap_or(stop.session_id);
                        let peer = client.unwrap_or(from);
                        socket
                            .send_to(
                                &wire_report(UdpPerfReport {
                                    report_seq: 1,
                                    is_final: true,
                                    summary: UdpPerfSummary {
                                        session_id: sid,
                                        mode: UdpPerfMode::Forward,
                                        total_elapsed_micros: 1_000_000,
                                        interval_elapsed_micros: 1_000_000,
                                        total: UdpPerfCountersSummary {
                                            packets: forward_packets,
                                            bytes: forward_packets.saturating_mul(96),
                                            ..UdpPerfCountersSummary::default()
                                        },
                                        interval: UdpPerfCountersSummary {
                                            packets: forward_packets,
                                            bytes: forward_packets.saturating_mul(96),
                                            ..UdpPerfCountersSummary::default()
                                        },
                                        forward: UdpPerfDirectionSummary {
                                            total: UdpPerfCountersSummary {
                                                packets: forward_packets,
                                                bytes: forward_packets.saturating_mul(96),
                                                ..UdpPerfCountersSummary::default()
                                            },
                                            interval: UdpPerfCountersSummary {
                                                packets: forward_packets,
                                                bytes: forward_packets.saturating_mul(96),
                                                ..UdpPerfCountersSummary::default()
                                            },
                                        },
                                        reverse: UdpPerfDirectionSummary::default(),
                                    },
                                })?,
                                peer,
                            )
                            .await?;
                        return Ok(());
                    }
                    _ => {}
                }
            }
        });
        Ok((addr.to_string(), task))
    }

    async fn spawn_forward_packet_counting_server(
    ) -> Result<(String, tokio::task::JoinHandle<Result<()>>)> {
        let socket = UdpSocket::bind("127.0.0.1:0").await?;
        let addr = socket.local_addr()?;
        let task = tokio::spawn(async move {
            let mut buf = vec![0u8; 64 * 1024];
            let mut client = None;
            let mut session_id = None;
            let mut forward_packets = 0u64;

            loop {
                let (n, from) = socket.recv_from(&mut buf).await?;
                match UdpPerfWirePacket::decode(&buf[..n])? {
                    UdpPerfWirePacket::Control(UdpPerfControlPacket::Hello(hello)) => {
                        client = Some(from);
                        session_id = Some(hello.session_id);
                    }
                    UdpPerfWirePacket::Control(UdpPerfControlPacket::Start(start)) => {
                        client = Some(from);
                        session_id = Some(start.session_id);
                    }
                    UdpPerfWirePacket::Data(data) => {
                        if matches!(data.header.direction, UdpPerfDirection::Forward) {
                            forward_packets += 1;
                        }
                    }
                    UdpPerfWirePacket::Control(UdpPerfControlPacket::Stop(stop)) => {
                        ensure!(
                            forward_packets == 1,
                            "expected exactly one forward packet, got {forward_packets}"
                        );
                        let sid = session_id.unwrap_or(stop.session_id);
                        let peer = client.unwrap_or(from);
                        socket
                            .send_to(
                                &wire_report(UdpPerfReport {
                                    report_seq: 1,
                                    is_final: true,
                                    summary: UdpPerfSummary {
                                        session_id: sid,
                                        mode: UdpPerfMode::Forward,
                                        total_elapsed_micros: 1_000_000,
                                        interval_elapsed_micros: 1_000_000,
                                        total: UdpPerfCountersSummary {
                                            packets: forward_packets,
                                            bytes: forward_packets.saturating_mul(96),
                                            ..UdpPerfCountersSummary::default()
                                        },
                                        interval: UdpPerfCountersSummary {
                                            packets: forward_packets,
                                            bytes: forward_packets.saturating_mul(96),
                                            ..UdpPerfCountersSummary::default()
                                        },
                                        forward: UdpPerfDirectionSummary {
                                            total: UdpPerfCountersSummary {
                                                packets: forward_packets,
                                                bytes: forward_packets.saturating_mul(96),
                                                ..UdpPerfCountersSummary::default()
                                            },
                                            interval: UdpPerfCountersSummary {
                                                packets: forward_packets,
                                                bytes: forward_packets.saturating_mul(96),
                                                ..UdpPerfCountersSummary::default()
                                            },
                                        },
                                        reverse: UdpPerfDirectionSummary::default(),
                                    },
                                })?,
                                peer,
                            )
                            .await?;
                        return Ok(());
                    }
                    _ => {}
                }
            }
        });
        Ok((addr.to_string(), task))
    }

    fn wire_report(report: UdpPerfReport) -> Result<Vec<u8>> {
        UdpPerfWirePacket::Control(UdpPerfControlPacket::Report(report)).encode()
    }

    fn interval_report(
        report_seq: u64,
        total_elapsed_micros: u64,
        interval_elapsed_micros: u64,
    ) -> UdpPerfReport {
        UdpPerfReport {
            report_seq,
            is_final: false,
            summary: UdpPerfSummary {
                session_id: 1,
                mode: UdpPerfMode::Reverse,
                total_elapsed_micros,
                interval_elapsed_micros,
                total: UdpPerfCountersSummary::default(),
                interval: UdpPerfCountersSummary::default(),
                forward: UdpPerfDirectionSummary::default(),
                reverse: UdpPerfDirectionSummary {
                    total: UdpPerfCountersSummary {
                        packets: report_seq,
                        bytes: report_seq.saturating_mul(32),
                        ..UdpPerfCountersSummary::default()
                    },
                    interval: UdpPerfCountersSummary {
                        packets: 1,
                        bytes: 32,
                        ..UdpPerfCountersSummary::default()
                    },
                },
            },
        }
    }

    fn wire_reverse_data(session_id: u64, seq: u64, payload_len: usize) -> Result<Vec<u8>> {
        UdpPerfWirePacket::Data(UdpPerfDataPacket {
            header: UdpPerfDataHeader {
                session_id,
                stream_id: 1,
                direction: UdpPerfDirection::Reverse,
                seq,
                send_ts_micros: seq * 1_000,
                payload_len: payload_len as u16,
            },
            payload: vec![0x5a; payload_len],
        })
        .encode()
    }
}
