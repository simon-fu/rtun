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
use serde::Serialize;
use tokio::{net::UdpSocket, time::Instant};
use tracing::info;

use super::udp_perf::{
    UdpPerfControlPacket, UdpPerfDataHeader, UdpPerfDataPacket, UdpPerfDirection, UdpPerfHello,
    UdpPerfMode, UdpPerfReport, UdpPerfStart, UdpPerfStats, UdpPerfStop, UdpPerfSummary,
    UdpPerfWirePacket, MAX_UDP_DATAGRAM_PAYLOAD_LEN, MAX_UDP_SAFE_PAYLOAD_LEN,
};

const DEFAULT_PPS: u64 = 1_000;
const DEFAULT_CONTROL_PLANE_TIMEOUT: Duration = Duration::from_secs(5);
const MIN_RTT_SCALED_CONTROL_TIMEOUT: Duration = Duration::from_secs(2);
const CONTROL_PLANE_TIMEOUT_RTT_MULTIPLIER: u32 = 4;
const START_RETRY_INTERVAL: Duration = Duration::from_millis(100);
const MAX_INITIAL_FORWARD_SEND_DELAY: Duration = Duration::from_millis(10);
const MAX_FORWARD_SENDS_PER_LOOP: usize = 128;

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
    let start_sent_at = Instant::now();
    send_control(&socket, UdpPerfControlPacket::Start(start.clone())).await?;

    let mut last_start_tx_at = start_sent_at;
    let wait_for_start_confirmation = mode_waits_for_start_confirmation(args.mode, pps, duration);
    let retry_start_until_server_response = wait_for_start_confirmation;
    let mut observed_control_rtt: Option<Duration> = None;
    let mut stop_at = if wait_for_start_confirmation {
        None
    } else {
        Some(stop_deadline(start_sent_at, duration)?)
    };
    let mut stop_sent = false;
    let mut final_deadline: Option<Instant> = None;
    let mut next_start_retry_at = next_start_retry_deadline(start_sent_at)?;
    let mut next_stop_retry_at: Option<Instant> = None;
    let mut start_confirmation_deadline = if wait_for_start_confirmation {
        Some(start_confirmation_timeout(
            start_sent_at,
            observed_control_rtt,
        )?)
    } else {
        None
    };
    let mut start_confirmation_deadline_cap: Option<Instant> = None;
    let mut next_send_at = if wait_for_start_confirmation {
        None
    } else {
        first_send_deadline(start_sent_at, args.mode, pps)
    };
    let mut send_seq: u64 = 0;
    let mut client_tx = ClientTxStats::default();
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
        }

        if !stop_sent {
            if let Some(due) = next_start_retry_at {
                if due <= now && !stop_at.is_some_and(|deadline| now >= deadline) {
                    let retry_sent_at =
                        send_start_retry_with_hello(&socket, session_id, args.mode, &start).await?;
                    last_start_tx_at = retry_sent_at;
                    next_start_retry_at = if retry_start_until_server_response {
                        next_start_retry_deadline(retry_sent_at)?
                    } else {
                        None
                    };
                    if start_confirmation_deadline.is_some() {
                        let retry_deadline =
                            start_confirmation_timeout(retry_sent_at, observed_control_rtt)?;
                        // Let the first retry extend the confirmation window once, but keep
                        // later retries from turning a silent peer into an unbounded wait.
                        let deadline_cap =
                            start_confirmation_deadline_cap.get_or_insert(retry_deadline);
                        start_confirmation_deadline = Some(min(retry_deadline, *deadline_cap));
                    }
                }
            }

            send_due_forward_packets(
                &socket,
                session_id,
                payload_len,
                pps,
                &mut send_seq,
                &mut next_send_at,
                now,
                stop_at,
                &mut client_tx,
            )
            .await?;

            if stop_at.is_some_and(|deadline| now >= deadline)
                && !has_due_forward_packets(next_send_at, now, stop_at)
            {
                send_control(
                    &socket,
                    UdpPerfControlPacket::Stop(UdpPerfStop { session_id }),
                )
                .await?;
                stop_sent = true;
                next_start_retry_at = None;
                next_send_at = None;
                next_stop_retry_at = next_stop_retry_deadline(now)?;
                start_confirmation_deadline = None;
                start_confirmation_deadline_cap = None;
                final_deadline = Some(
                    now.checked_add(control_plane_timeout(observed_control_rtt))
                        .context("final report deadline overflow")?,
                );
            }
        } else if final_report.is_none() {
            if let Some(due) = next_stop_retry_at {
                if due <= now {
                    send_control(
                        &socket,
                        UdpPerfControlPacket::Stop(UdpPerfStop { session_id }),
                    )
                    .await?;
                    next_stop_retry_at = next_stop_retry_deadline(now)?;
                }
            }
        }

        if let Some(report) = consume_final_report_or_timeout(
            &mut final_report,
            now,
            final_deadline,
            final_report_drain_deadline,
        )? {
            let final_summary =
                build_final_client_display_summary(&report, &mut client_rx, &mut client_tx);
            output.push_str(&render_final_summary(
                &final_summary,
                args.json,
                FinalSummaryContext {
                    target: &args.target,
                    mode: args.mode,
                    session_id,
                    duration_secs: args.time,
                    payload_len: args.len as u64,
                    pps,
                    interval_ms: args.interval.get(),
                },
            )?);
            return Ok(ClientRunOutput {
                final_report: report,
                client_rx,
                output,
            });
        }

        if let Some(deadline) = next_loop_deadline(
            next_send_at,
            next_start_retry_at,
            if stop_sent && final_report.is_none() {
                next_stop_retry_at
            } else {
                None
            },
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
                    let incoming = handle_incoming_packet(
                        session_id,
                        &recv_buf[..n],
                        &mut output,
                        !args.json,
                        output_policy,
                        &mut client_rx,
                        &mut client_tx,
                    )?;
                    if let Some(report) = incoming.report {
                        if report.is_final {
                            next_stop_retry_at = None;
                            final_report_drain_deadline = schedule_final_report_drain_deadline(
                                &report,
                                &client_rx,
                                Instant::now(),
                                observed_control_rtt,
                            )?;
                            final_report = Some(report);
                        }
                    }
                    if incoming.matched_session {
                        next_start_retry_at = None;
                        if start_confirmation_deadline.take().is_some() {
                            start_confirmation_deadline_cap = None;
                            let anchor = Instant::now();
                            observed_control_rtt = Some(measure_control_rtt(last_start_tx_at, anchor));
                            stop_at = Some(stop_deadline(anchor, duration)?);
                            next_send_at = first_send_deadline(anchor, args.mode, pps);
                        }
                    }
                    if had_final_report && incoming.matched_session {
                        refresh_final_report_drain_deadline(
                            final_report.as_ref(),
                            &client_rx,
                            &mut final_report_drain_deadline,
                            Instant::now(),
                            observed_control_rtt,
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
            let incoming = handle_incoming_packet(
                session_id,
                &recv_buf[..n],
                &mut output,
                !args.json,
                output_policy,
                &mut client_rx,
                &mut client_tx,
            )?;
            if let Some(report) = incoming.report {
                if report.is_final {
                    next_stop_retry_at = None;
                    final_report_drain_deadline = schedule_final_report_drain_deadline(
                        &report,
                        &client_rx,
                        Instant::now(),
                        observed_control_rtt,
                    )?;
                    final_report = Some(report);
                }
            }
            if incoming.matched_session {
                next_start_retry_at = None;
                if start_confirmation_deadline.take().is_some() {
                    start_confirmation_deadline_cap = None;
                    let anchor = Instant::now();
                    observed_control_rtt = Some(measure_control_rtt(last_start_tx_at, anchor));
                    stop_at = Some(stop_deadline(anchor, duration)?);
                    next_send_at = first_send_deadline(anchor, args.mode, pps);
                }
            }
            if had_final_report && incoming.matched_session {
                refresh_final_report_drain_deadline(
                    final_report.as_ref(),
                    &client_rx,
                    &mut final_report_drain_deadline,
                    Instant::now(),
                    observed_control_rtt,
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
    observed_control_rtt: Option<Duration>,
) -> Result<Option<Instant>> {
    if matches!(
        report.summary.mode,
        UdpPerfMode::Reverse | UdpPerfMode::Bidir
    ) && client_rx.has_pending_reverse_packets(report)
    {
        return Ok(Some(
            now.checked_add(reverse_tail_idle_timeout(observed_control_rtt))
                .context("final reverse drain deadline overflow")?,
        ));
    }
    Ok(None)
}

fn next_loop_deadline(
    next_send_at: Option<Instant>,
    next_start_retry_at: Option<Instant>,
    next_stop_retry_at: Option<Instant>,
    stop_at: Option<Instant>,
    start_confirmation_deadline: Option<Instant>,
    final_deadline: Option<Instant>,
) -> Option<Instant> {
    let mut next = next_send_at;
    if let Some(t) = next_start_retry_at {
        next = Some(next.map_or(t, |cur| min(cur, t)));
    }
    if let Some(t) = next_stop_retry_at {
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

fn next_stop_retry_deadline(now: Instant) -> Result<Option<Instant>> {
    Ok(Some(
        now.checked_add(START_RETRY_INTERVAL)
            .context("stop retry deadline overflow")?,
    ))
}

fn start_confirmation_timeout(
    started_at: Instant,
    observed_control_rtt: Option<Duration>,
) -> Result<Instant> {
    started_at
        .checked_add(control_plane_timeout(observed_control_rtt))
        .context("start confirmation deadline overflow")
}

fn refresh_final_report_drain_deadline(
    final_report: Option<&UdpPerfReport>,
    client_rx: &ClientRxStats,
    final_report_drain_deadline: &mut Option<Instant>,
    now: Instant,
    observed_control_rtt: Option<Duration>,
) -> Result<()> {
    if let Some(report) = final_report {
        *final_report_drain_deadline =
            schedule_final_report_drain_deadline(report, client_rx, now, observed_control_rtt)?;
    }
    Ok(())
}

fn reverse_tail_idle_timeout(observed_control_rtt: Option<Duration>) -> Duration {
    control_plane_timeout(observed_control_rtt)
}

fn control_plane_timeout(observed_control_rtt: Option<Duration>) -> Duration {
    observed_control_rtt
        .and_then(|rtt| scale_duration(rtt, CONTROL_PLANE_TIMEOUT_RTT_MULTIPLIER))
        .map(|scaled| max_duration(scaled, MIN_RTT_SCALED_CONTROL_TIMEOUT))
        .unwrap_or(DEFAULT_CONTROL_PLANE_TIMEOUT)
}

fn scale_duration(duration: Duration, factor: u32) -> Option<Duration> {
    duration.checked_mul(factor)
}

fn max_duration(a: Duration, b: Duration) -> Duration {
    std::cmp::max(a, b)
}

fn measure_control_rtt(sent_at: Instant, received_at: Instant) -> Duration {
    received_at.saturating_duration_since(sent_at)
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

#[derive(Debug, Default)]
struct ClientTxStats {
    forward_total_packets: u64,
    forward_total_bytes: u64,
    forward_interval_packets: u64,
    forward_interval_bytes: u64,
}

impl ClientTxStats {
    fn on_forward_packet_sent(&mut self, payload_len: u16) {
        let bytes = payload_len as u64;
        self.forward_total_packets = self.forward_total_packets.saturating_add(1);
        self.forward_total_bytes = self.forward_total_bytes.saturating_add(bytes);
        self.forward_interval_packets = self.forward_interval_packets.saturating_add(1);
        self.forward_interval_bytes = self.forward_interval_bytes.saturating_add(bytes);
    }

    fn forward_total_summary(
        &self,
        elapsed_micros: u64,
    ) -> super::udp_perf::UdpPerfCountersSummary {
        counters_from_bytes_packets(
            self.forward_total_bytes,
            self.forward_total_packets,
            elapsed_micros,
        )
    }

    fn forward_interval_summary(
        &self,
        interval_elapsed_micros: u64,
    ) -> super::udp_perf::UdpPerfCountersSummary {
        counters_from_bytes_packets(
            self.forward_interval_bytes,
            self.forward_interval_packets,
            interval_elapsed_micros,
        )
    }

    fn take_forward_interval_summary(
        &mut self,
        interval_elapsed_micros: u64,
    ) -> super::udp_perf::UdpPerfCountersSummary {
        let summary = self.forward_interval_summary(interval_elapsed_micros);
        self.forward_interval_packets = 0;
        self.forward_interval_bytes = 0;
        summary
    }
}

#[derive(Debug, Clone)]
struct ClientDisplaySummary {
    mode: UdpPerfMode,
    total_elapsed_micros: u64,
    interval_elapsed_micros: u64,
    forward_sender: DisplayDirectionSummary,
    forward_receiver: DisplayDirectionSummary,
    reverse_sender: DisplayDirectionSummary,
    reverse_receiver: DisplayDirectionSummary,
}

#[derive(Debug, Clone, Default, Serialize)]
struct DisplayDirectionSummary {
    total: super::udp_perf::UdpPerfCountersSummary,
    interval: super::udp_perf::UdpPerfCountersSummary,
}

#[derive(Debug, Clone, Copy)]
struct FinalSummaryContext<'a> {
    target: &'a str,
    mode: UdpPerfMode,
    session_id: u64,
    duration_secs: u64,
    payload_len: u64,
    pps: u64,
    interval_ms: u64,
}

#[derive(Debug, Serialize)]
struct ClientFinalJson<'a> {
    start: ClientFinalJsonStart<'a>,
    end: ClientFinalJsonEnd,
}

#[derive(Debug, Serialize)]
struct ClientFinalJsonStart<'a> {
    session_id: u64,
    mode: UdpPerfMode,
    target: &'a str,
    duration_secs: u64,
    payload_len: u64,
    pps: u64,
    interval_ms: u64,
}

#[derive(Debug, Serialize)]
struct ClientFinalJsonEnd {
    sum_forward_sender: super::udp_perf::UdpPerfCountersSummary,
    sum_forward_receiver: super::udp_perf::UdpPerfCountersSummary,
    sum_reverse_sender: super::udp_perf::UdpPerfCountersSummary,
    sum_reverse_receiver: super::udp_perf::UdpPerfCountersSummary,
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
    !duration.is_zero() && pps > 0 && matches!(mode, UdpPerfMode::Reverse)
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

async fn send_due_forward_packets(
    socket: &UdpSocket,
    session_id: u64,
    payload_len: u16,
    pps: u64,
    send_seq: &mut u64,
    next_send_at: &mut Option<Instant>,
    now: Instant,
    stop_at: Option<Instant>,
    client_tx: &mut ClientTxStats,
) -> Result<()> {
    let send_until = stop_at.map_or(now, |deadline| min(deadline, now));
    let mut sent = 0usize;
    while let Some(due) = *next_send_at {
        if sent >= MAX_FORWARD_SENDS_PER_LOOP {
            break;
        }
        if due > send_until {
            break;
        }
        *send_seq = send_seq.saturating_add(1);
        send_forward_data(socket, session_id, *send_seq, payload_len).await?;
        client_tx.on_forward_packet_sent(payload_len);
        *next_send_at = next_send_deadline(due, pps);
        sent += 1;
    }
    Ok(())
}

fn has_due_forward_packets(
    next_send_at: Option<Instant>,
    now: Instant,
    stop_at: Option<Instant>,
) -> bool {
    let send_until = stop_at.map_or(now, |deadline| min(deadline, now));
    next_send_at.is_some_and(|due| due <= send_until)
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

#[derive(Debug)]
struct IncomingPacketOutcome {
    matched_session: bool,
    report: Option<UdpPerfReport>,
}

fn handle_incoming_packet(
    expected_session_id: u64,
    bytes: &[u8],
    output: &mut String,
    emit_interval: bool,
    output_policy: OutputPolicy,
    client_rx: &mut ClientRxStats,
    client_tx: &mut ClientTxStats,
) -> Result<IncomingPacketOutcome> {
    let wire = UdpPerfWirePacket::decode(bytes)?;
    match wire {
        UdpPerfWirePacket::Control(UdpPerfControlPacket::Report(report)) => {
            if report.summary.session_id != expected_session_id {
                return Ok(IncomingPacketOutcome {
                    matched_session: false,
                    report: None,
                });
            }
            if !report.is_final {
                match client_rx.observe_interval_report(report.report_seq) {
                    IntervalReportSync::Stale => {}
                    sync_state => {
                        let forward_interval = client_tx
                            .take_forward_interval_summary(report.summary.interval_elapsed_micros);
                        let reverse_interval = client_rx.take_interval_summary(
                            report.summary.total_elapsed_micros,
                            report.summary.interval_elapsed_micros,
                        );
                        if emit_interval && matches!(sync_state, IntervalReportSync::Synced) {
                            let display = build_client_display_summary(
                                &report.summary,
                                client_rx,
                                client_tx,
                                Some(reverse_interval),
                                Some(forward_interval),
                            );
                            let line = render_interval_summary(&display);
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
            Ok(IncomingPacketOutcome {
                matched_session: true,
                report: Some(report),
            })
        }
        UdpPerfWirePacket::Data(data) => {
            if data.header.session_id != expected_session_id {
                return Ok(IncomingPacketOutcome {
                    matched_session: false,
                    report: None,
                });
            }
            client_rx.on_data_packet(&data);
            Ok(IncomingPacketOutcome {
                matched_session: true,
                report: None,
            })
        }
        UdpPerfWirePacket::Control(_) => Ok(IncomingPacketOutcome {
            matched_session: false,
            report: None,
        }),
    }
}

fn render_interval_summary(summary: &ClientDisplaySummary) -> String {
    let mut out = String::new();
    let interval_start = elapsed_secs(
        summary
            .total_elapsed_micros
            .saturating_sub(summary.interval_elapsed_micros),
    );
    let interval_end = elapsed_secs(summary.total_elapsed_micros);
    match summary.mode {
        UdpPerfMode::Forward => {
            out.push_str(&render_direction_line(
                interval_start,
                interval_end,
                &summary.forward_sender.interval,
                &summary.forward_receiver.interval,
                "FWD",
                None,
            ));
        }
        UdpPerfMode::Reverse => {
            out.push_str(&render_direction_line(
                interval_start,
                interval_end,
                &summary.reverse_sender.interval,
                &summary.reverse_receiver.interval,
                "REV",
                None,
            ));
        }
        UdpPerfMode::Bidir => {
            out.push_str(&render_direction_line(
                interval_start,
                interval_end,
                &summary.forward_sender.interval,
                &summary.forward_receiver.interval,
                "FWD",
                None,
            ));
            out.push_str(&render_direction_line(
                interval_start,
                interval_end,
                &summary.reverse_sender.interval,
                &summary.reverse_receiver.interval,
                "REV",
                None,
            ));
        }
    }
    out
}

fn render_final_summary(
    summary: &ClientDisplaySummary,
    as_json: bool,
    ctx: FinalSummaryContext<'_>,
) -> Result<String> {
    if as_json {
        let json = ClientFinalJson {
            start: ClientFinalJsonStart {
                session_id: ctx.session_id,
                mode: ctx.mode,
                target: ctx.target,
                duration_secs: ctx.duration_secs,
                payload_len: ctx.payload_len,
                pps: ctx.pps,
                interval_ms: ctx.interval_ms,
            },
            end: ClientFinalJsonEnd {
                sum_forward_sender: summary.forward_sender.total.clone(),
                sum_forward_receiver: summary.forward_receiver.total.clone(),
                sum_reverse_sender: summary.reverse_sender.total.clone(),
                sum_reverse_receiver: summary.reverse_receiver.total.clone(),
            },
        };
        return Ok(format!("{}\n", serde_json::to_string_pretty(&json)?));
    }
    let mut out = String::new();
    let total_start = 0.0;
    let total_end = elapsed_secs(summary.total_elapsed_micros);
    match summary.mode {
        UdpPerfMode::Forward => {
            out.push_str(&render_direction_line(
                total_start,
                total_end,
                &summary.forward_sender.total,
                &summary.forward_receiver.total,
                "FWD",
                Some("sender"),
            ));
            out.push_str(&render_direction_line(
                total_start,
                total_end,
                &summary.forward_receiver.total,
                &summary.forward_sender.total,
                "FWD",
                Some("receiver"),
            ));
        }
        UdpPerfMode::Reverse => {
            out.push_str(&render_direction_line(
                total_start,
                total_end,
                &summary.reverse_sender.total,
                &summary.reverse_receiver.total,
                "REV",
                Some("sender"),
            ));
            out.push_str(&render_direction_line(
                total_start,
                total_end,
                &summary.reverse_receiver.total,
                &summary.reverse_sender.total,
                "REV",
                Some("receiver"),
            ));
        }
        UdpPerfMode::Bidir => {
            out.push_str(&render_direction_line(
                total_start,
                total_end,
                &summary.forward_sender.total,
                &summary.forward_receiver.total,
                "FWD",
                Some("sender"),
            ));
            out.push_str(&render_direction_line(
                total_start,
                total_end,
                &summary.forward_receiver.total,
                &summary.forward_sender.total,
                "FWD",
                Some("receiver"),
            ));
            out.push_str(&render_direction_line(
                total_start,
                total_end,
                &summary.reverse_sender.total,
                &summary.reverse_receiver.total,
                "REV",
                Some("sender"),
            ));
            out.push_str(&render_direction_line(
                total_start,
                total_end,
                &summary.reverse_receiver.total,
                &summary.reverse_sender.total,
                "REV",
                Some("receiver"),
            ));
        }
    }
    Ok(out)
}

fn build_client_display_summary(
    server: &UdpPerfSummary,
    client_rx: &ClientRxStats,
    client_tx: &ClientTxStats,
    reverse_interval_override: Option<super::udp_perf::UdpPerfCountersSummary>,
    forward_interval_override: Option<super::udp_perf::UdpPerfCountersSummary>,
) -> ClientDisplaySummary {
    let reverse_local =
        client_rx.reverse_summary(server.total_elapsed_micros, server.interval_elapsed_micros);
    ClientDisplaySummary {
        mode: server.mode,
        total_elapsed_micros: server.total_elapsed_micros,
        interval_elapsed_micros: server.interval_elapsed_micros,
        forward_sender: DisplayDirectionSummary {
            total: client_tx.forward_total_summary(server.total_elapsed_micros),
            interval: forward_interval_override.unwrap_or_else(|| {
                client_tx.forward_interval_summary(server.interval_elapsed_micros)
            }),
        },
        forward_receiver: DisplayDirectionSummary {
            total: server.forward.total.clone(),
            interval: server.forward.interval.clone(),
        },
        reverse_sender: DisplayDirectionSummary {
            total: server.reverse.total.clone(),
            interval: server.reverse.interval.clone(),
        },
        reverse_receiver: DisplayDirectionSummary {
            total: reverse_local.total,
            interval: reverse_interval_override.unwrap_or(reverse_local.interval),
        },
    }
}

fn build_final_client_display_summary(
    report: &UdpPerfReport,
    client_rx: &mut ClientRxStats,
    client_tx: &mut ClientTxStats,
) -> ClientDisplaySummary {
    if !matches!(
        report.summary.mode,
        UdpPerfMode::Reverse | UdpPerfMode::Bidir
    ) {
        return build_client_display_summary(&report.summary, client_rx, client_tx, None, None);
    }

    let (reverse_interval, forward_interval) = match client_rx
        .observe_interval_report(report.report_seq)
    {
        IntervalReportSync::Synced => (
            Some(client_rx.take_interval_summary(
                report.summary.total_elapsed_micros,
                report.summary.interval_elapsed_micros,
            )),
            Some(client_tx.take_forward_interval_summary(report.summary.interval_elapsed_micros)),
        ),
        IntervalReportSync::UnsyncedGap => {
            let _ = client_rx.take_interval_summary(
                report.summary.total_elapsed_micros,
                report.summary.interval_elapsed_micros,
            );
            let _ = client_tx.take_forward_interval_summary(report.summary.interval_elapsed_micros);
            (
                Some(report.summary.reverse.interval.clone()),
                Some(report.summary.forward.interval.clone()),
            )
        }
        IntervalReportSync::Stale => (
            Some(report.summary.reverse.interval.clone()),
            Some(report.summary.forward.interval.clone()),
        ),
    };

    build_client_display_summary(
        &report.summary,
        client_rx,
        client_tx,
        reverse_interval,
        forward_interval,
    )
}

fn render_direction_line(
    start_secs: f64,
    end_secs: f64,
    tx: &super::udp_perf::UdpPerfCountersSummary,
    rx: &super::udp_perf::UdpPerfCountersSummary,
    direction: &str,
    role: Option<&str>,
) -> String {
    match role {
        Some(role) => format!(
            "[{:>3}] {:>6.2}-{:<6.2} sec  TX {:>10} {:>12}  RX {:>10} {:>12}  {} {}\n",
            1,
            start_secs,
            end_secs,
            format_transfer(tx.bytes),
            format_bitrate(tx.mbps),
            format_transfer(rx.bytes),
            format_bitrate(rx.mbps),
            direction,
            role
        ),
        None => format!(
            "[{:>3}] {:>6.2}-{:<6.2} sec  TX {:>10} {:>12}  RX {:>10} {:>12}  {}\n",
            1,
            start_secs,
            end_secs,
            format_transfer(tx.bytes),
            format_bitrate(tx.mbps),
            format_transfer(rx.bytes),
            format_bitrate(rx.mbps),
            direction
        ),
    }
}

fn format_transfer(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    let bytes_f = bytes as f64;
    if bytes_f >= GIB {
        format!("{:.2} GBytes", bytes_f / GIB)
    } else if bytes_f >= MIB {
        format!("{:.2} MBytes", bytes_f / MIB)
    } else if bytes_f >= KIB {
        format!("{:.2} KBytes", bytes_f / KIB)
    } else {
        format!("{bytes} Bytes")
    }
}

fn format_bitrate(mbps: f64) -> String {
    let bps = mbps * 1_000_000.0;
    if bps >= 1_000_000_000.0 {
        format!("{:.2} Gbits/sec", bps / 1_000_000_000.0)
    } else if bps >= 1_000_000.0 {
        format!("{:.2} Mbits/sec", bps / 1_000_000.0)
    } else {
        format!("{:.2} Kbits/sec", bps / 1_000.0)
    }
}

fn elapsed_secs(elapsed_micros: u64) -> f64 {
    elapsed_micros as f64 / 1_000_000.0
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

fn build_final_client_view_summary(
    report: &UdpPerfReport,
    client_rx: &mut ClientRxStats,
) -> UdpPerfSummary {
    if !matches!(
        report.summary.mode,
        UdpPerfMode::Reverse | UdpPerfMode::Bidir
    ) {
        return build_client_view_summary(&report.summary, client_rx, None);
    }

    let reverse_interval = match client_rx.observe_interval_report(report.report_seq) {
        IntervalReportSync::Synced => Some(client_rx.take_interval_summary(
            report.summary.total_elapsed_micros,
            report.summary.interval_elapsed_micros,
        )),
        IntervalReportSync::UnsyncedGap => {
            let _ = client_rx.take_interval_summary(
                report.summary.total_elapsed_micros,
                report.summary.interval_elapsed_micros,
            );
            Some(report.summary.reverse.interval.clone())
        }
        IntervalReportSync::Stale => Some(report.summary.reverse.interval.clone()),
    };

    build_client_view_summary(&report.summary, client_rx, reverse_interval)
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

fn counters_from_bytes_packets(
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
    let counter = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
    let now_nanos = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as u64,
        Err(_) => counter,
    };
    let pid = std::process::id() as u64;
    let mixed = mix_session_id_seed(now_nanos ^ (pid << 32) ^ counter.rotate_left(17));
    if mixed == 0 {
        1
    } else {
        mixed
    }
}

fn mix_session_id_seed(mut x: u64) -> u64 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51afd7ed558ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ceb9fe1a85ec53);
    x ^= x >> 33;
    x
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
    use std::{
        net::SocketAddr,
        num::NonZeroU64,
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc,
        },
        time::Duration,
    };

    use anyhow::{bail, ensure, Context, Result};
    use serde_json::Value;
    use tokio::net::UdpSocket;
    use tokio::sync::oneshot;
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
    async fn udp_client_bidir_starts_forward_before_first_reverse_packet() -> Result<()> {
        let (target, server_task) =
            spawn_delayed_bidir_confirmation_server(Duration::from_millis(200)).await?;
        let mut args = test_args(target, UdpPerfMode::Bidir);
        args.time = 1;
        args.pps = Some(20);
        args.json = true;

        let out = tokio::time::timeout(Duration::from_secs(4), super::run_for_test(args))
            .await
            .context(
                "bidir client should complete against delayed reverse confirmation server",
            )??;

        assert!(out.final_report.is_final);
        server_task.await??;
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
        assert!(json.get("start").is_some(), "missing start field");
        assert!(json.get("end").is_some(), "missing end field");
        assert!(
            json.pointer("/end/sum_forward_sender").is_some(),
            "missing end.sum_forward_sender field"
        );
        assert!(
            json.pointer("/end/sum_forward_receiver").is_some(),
            "missing end.sum_forward_receiver field"
        );
        assert!(
            json.pointer("/end/sum_reverse_sender").is_some(),
            "missing end.sum_reverse_sender field"
        );
        assert!(
            json.pointer("/end/sum_reverse_receiver").is_some(),
            "missing end.sum_reverse_receiver field"
        );
        assert!(
            !out.output.contains("interval mode="),
            "json output should not contain interval text"
        );

        server_task.abort();
        Ok(())
    }

    #[test]
    fn udp_client_render_final_summary_uses_sender_receiver_lines() -> Result<()> {
        let summary = super::ClientDisplaySummary {
            mode: UdpPerfMode::Forward,
            total_elapsed_micros: 1_000_000,
            interval_elapsed_micros: 1_000_000,
            forward_sender: super::DisplayDirectionSummary {
                total: UdpPerfCountersSummary {
                    bytes: 10_000,
                    packets: 20,
                    mbps: 0.08,
                    pps: 20.0,
                    ..UdpPerfCountersSummary::default()
                },
                interval: UdpPerfCountersSummary::default(),
            },
            forward_receiver: super::DisplayDirectionSummary {
                total: UdpPerfCountersSummary {
                    bytes: 9_000,
                    packets: 18,
                    mbps: 0.072,
                    pps: 18.0,
                    ..UdpPerfCountersSummary::default()
                },
                interval: UdpPerfCountersSummary::default(),
            },
            reverse_sender: super::DisplayDirectionSummary::default(),
            reverse_receiver: super::DisplayDirectionSummary::default(),
        };

        let out = super::render_final_summary(
            &summary,
            false,
            super::FinalSummaryContext {
                target: "127.0.0.1:1",
                mode: UdpPerfMode::Forward,
                session_id: 1,
                duration_secs: 1,
                payload_len: 100,
                pps: 20,
                interval_ms: 1_000,
            },
        )?;
        assert!(
            out.contains("sender"),
            "final text should contain sender line, got: {out}"
        );
        assert!(
            out.contains("receiver"),
            "final text should contain receiver line, got: {out}"
        );
        Ok(())
    }

    #[test]
    fn udp_client_render_interval_summary_bidir_prints_two_direction_lines() {
        let summary = super::ClientDisplaySummary {
            mode: UdpPerfMode::Bidir,
            total_elapsed_micros: 1_000_000,
            interval_elapsed_micros: 1_000_000,
            forward_sender: super::DisplayDirectionSummary {
                total: UdpPerfCountersSummary::default(),
                interval: UdpPerfCountersSummary {
                    bytes: 1_000,
                    packets: 10,
                    mbps: 0.008,
                    pps: 10.0,
                    ..UdpPerfCountersSummary::default()
                },
            },
            forward_receiver: super::DisplayDirectionSummary {
                total: UdpPerfCountersSummary::default(),
                interval: UdpPerfCountersSummary {
                    bytes: 900,
                    packets: 9,
                    mbps: 0.0072,
                    pps: 9.0,
                    ..UdpPerfCountersSummary::default()
                },
            },
            reverse_sender: super::DisplayDirectionSummary {
                total: UdpPerfCountersSummary::default(),
                interval: UdpPerfCountersSummary {
                    bytes: 2_000,
                    packets: 20,
                    mbps: 0.016,
                    pps: 20.0,
                    ..UdpPerfCountersSummary::default()
                },
            },
            reverse_receiver: super::DisplayDirectionSummary {
                total: UdpPerfCountersSummary::default(),
                interval: UdpPerfCountersSummary {
                    bytes: 1_800,
                    packets: 18,
                    mbps: 0.0144,
                    pps: 18.0,
                    ..UdpPerfCountersSummary::default()
                },
            },
        };

        let out = super::render_interval_summary(&summary);
        assert!(
            out.contains("FWD"),
            "bidir interval should contain FWD line, got: {out}"
        );
        assert!(
            out.contains("REV"),
            "bidir interval should contain REV line, got: {out}"
        );
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
        let reverse_sender = json
            .pointer("/end/sum_reverse_sender/packets")
            .and_then(Value::as_u64)
            .context("missing end.sum_reverse_sender.packets")?;
        let reverse_receiver = json
            .pointer("/end/sum_reverse_receiver/packets")
            .and_then(Value::as_u64)
            .context("missing end.sum_reverse_receiver.packets")?;

        assert!(reverse_sender > 0, "reverse sender should be > 0");
        assert!(reverse_receiver > 0, "reverse receiver should be > 0");
        assert!(
            reverse_receiver <= reverse_sender,
            "reverse receiver should not exceed reverse sender, sender={reverse_sender}, receiver={reverse_receiver}"
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
    fn udp_client_session_id_carries_cross_process_entropy() {
        let session_id = super::next_session_id();
        assert_ne!(
            session_id >> 32,
            0,
            "session_id should not be a low 32-bit process-local counter"
        );
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
            .pointer("/end/sum_forward_sender/packets")
            .and_then(Value::as_u64)
            .context("missing end.sum_forward_sender.packets")?;

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
            .pointer("/end/sum_forward_sender/packets")
            .and_then(Value::as_u64)
            .context("missing end.sum_forward_sender.packets")?;

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
            json.pointer("/end/sum_reverse_receiver/packets")
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
            .pointer("/end/sum_reverse_receiver/packets")
            .and_then(Value::as_u64)
            .context("missing end.sum_reverse_receiver.packets")?;

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
    async fn udp_client_reverse_retry_refreshes_confirmation_deadline_after_retry() -> Result<()> {
        let (target, server_task) =
            spawn_drop_first_start_reverse_server_with_response_delay(Duration::from_millis(4_950))
                .await?;
        let mut args = test_args(target, UdpPerfMode::Reverse);
        args.time = 1;
        args.pps = Some(20);
        args.json = true;

        let out = tokio::time::timeout(Duration::from_secs(8), super::run_for_test(args))
            .await
            .context("udp-client should keep waiting for confirmation after START retry")??;

        assert!(out.final_report.is_final);
        assert!(
            out.client_rx.reverse_packets > 0,
            "client should accept delayed reverse confirmation after retry"
        );

        server_task.await??;
        Ok(())
    }

    #[tokio::test]
    async fn udp_client_reverse_unreachable_target_times_out_with_control_plane_deadline(
    ) -> Result<()> {
        let (target, start_seen, server_task) = spawn_blackhole_server().await?;
        let mut args = test_args(target, UdpPerfMode::Reverse);
        args.time = 60;
        args.pps = Some(20);
        args.json = true;

        let client_task = tokio::spawn(async move { super::run_for_test(args).await });
        start_seen.await.context("client never sent START")?;

        let err = tokio::time::timeout(Duration::from_secs(7), client_task)
            .await
            .context(
                "reverse run should fail after control-plane timeout instead of retrying forever",
            )?
            .context("udp-client join failed")?
            .expect_err("unreachable reverse target should time out");
        assert!(
            err.to_string()
                .contains("timeout waiting start confirmation"),
            "unexpected error: {err:#}"
        );

        server_task.abort();
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
    async fn udp_client_forward_late_wakeup_catches_up_send_cadence() -> Result<()> {
        let sender = UdpSocket::bind("127.0.0.1:0").await?;
        let receiver = UdpSocket::bind("127.0.0.1:0").await?;
        sender.connect(receiver.local_addr()?).await?;

        let now = Instant::now();
        let mut send_seq = 0u64;
        let mut client_tx = super::ClientTxStats::default();
        let mut next_send_at = Some(
            now.checked_sub(Duration::from_millis(25))
                .context("late wakeup deadline underflow")?,
        );
        super::send_due_forward_packets(
            &sender,
            1,
            16,
            100,
            &mut send_seq,
            &mut next_send_at,
            now,
            Some(now),
            &mut client_tx,
        )
        .await?;

        assert!(
            send_seq >= 3,
            "client should catch up to at least the scheduled 10/20/30ms sends after a 35ms late wakeup"
        );
        assert!(
            next_send_at.is_some_and(|due| due > now),
            "after catching up, the next forward deadline should move back into the future"
        );
        Ok(())
    }

    #[tokio::test]
    async fn udp_client_forward_catch_up_batch_is_capped() -> Result<()> {
        let sender = UdpSocket::bind("127.0.0.1:0").await?;
        let receiver = UdpSocket::bind("127.0.0.1:0").await?;
        sender.connect(receiver.local_addr()?).await?;

        let now = Instant::now();
        let mut send_seq = 0u64;
        let mut client_tx = super::ClientTxStats::default();
        let mut next_send_at = Some(now - Duration::from_millis(200));
        super::send_due_forward_packets(
            &sender,
            1,
            16,
            1_000,
            &mut send_seq,
            &mut next_send_at,
            now,
            Some(now),
            &mut client_tx,
        )
        .await?;

        assert!(
            send_seq <= 128,
            "single catch-up batch should not drain an unbounded forward backlog, got {send_seq}"
        );
        assert!(
            next_send_at.is_some_and(|due| due <= now),
            "capped catch-up should leave overdue packets queued for a later loop"
        );
        Ok(())
    }

    #[tokio::test]
    async fn udp_client_forward_sends_tail_packet_due_at_stop_deadline() -> Result<()> {
        let (target, server_task) = spawn_forward_exact_count_server(100).await?;
        let mut args = test_args(target, UdpPerfMode::Forward);
        args.time = 1;
        args.pps = Some(100);
        args.interval = NonZeroU64::new(2_000).expect("nonzero");
        args.json = true;

        let out = tokio::time::timeout(Duration::from_secs(4), super::run_for_test(args))
            .await
            .context("udp-client should complete exact-count forward run")??;

        assert!(out.final_report.is_final);
        server_task.await??;
        Ok(())
    }

    #[tokio::test]
    async fn udp_client_forward_silent_server_stops_retrying_start_after_startup() -> Result<()> {
        let (target, server_task) = spawn_silent_forward_final_report_server().await?;
        let mut args = test_args(target, UdpPerfMode::Forward);
        args.time = 1;
        args.pps = Some(1);
        args.interval = NonZeroU64::new(2_000).expect("nonzero");
        args.json = true;

        let _ = tokio::time::timeout(Duration::from_secs(4), super::run_for_test(args))
            .await
            .context("udp-client should finish against silent forward server")??;

        server_task.await??;
        Ok(())
    }

    #[tokio::test]
    async fn udp_client_ignores_final_report_from_other_session() -> Result<()> {
        let (target, server_task) = spawn_mismatched_final_report_server().await?;
        let mut args = test_args(target, UdpPerfMode::Forward);
        args.time = 1;
        args.pps = Some(20);
        args.json = true;

        let out = tokio::time::timeout(Duration::from_secs(4), super::run_for_test(args))
            .await
            .context("udp-client should finish after ignoring mismatched final report")??;

        assert_ne!(
            out.final_report.summary.session_id,
            u64::MAX,
            "client accepted final report from a different session"
        );

        server_task.await??;
        Ok(())
    }

    #[tokio::test]
    async fn udp_client_forward_tolerates_high_rtt_final_report() -> Result<()> {
        let (target, server_task) =
            spawn_delayed_forward_final_report_server(Duration::from_millis(2_500)).await?;
        let mut args = test_args(target, UdpPerfMode::Forward);
        args.time = 0;
        args.pps = Some(1);
        args.json = true;

        let out = tokio::time::timeout(Duration::from_secs(6), super::run_for_test(args))
            .await
            .context("udp-client should wait long enough for delayed final report")??;

        assert!(out.final_report.is_final);
        assert_eq!(out.final_report.summary.mode, UdpPerfMode::Forward);

        server_task.await??;
        Ok(())
    }

    #[tokio::test]
    async fn udp_client_retries_stop_while_waiting_for_final_report() -> Result<()> {
        let (target, server_task) = spawn_drop_first_stop_forward_server().await?;
        let mut args = test_args(target, UdpPerfMode::Forward);
        args.time = 0;
        args.pps = Some(1);
        args.json = true;

        let out = tokio::time::timeout(Duration::from_secs(4), super::run_for_test(args))
            .await
            .context("udp-client should resend STOP until final report arrives")??;

        assert!(out.final_report.is_final);
        server_task.await??;
        Ok(())
    }

    #[tokio::test]
    async fn udp_client_reverse_tolerates_high_rtt_start_confirmation() -> Result<()> {
        let (target, server_task) =
            spawn_delayed_reverse_start_confirmation_server(Duration::from_millis(3_200)).await?;
        let mut args = test_args(target, UdpPerfMode::Reverse);
        args.time = 1;
        args.pps = Some(20);
        args.json = true;

        let out = tokio::time::timeout(Duration::from_secs(8), super::run_for_test(args))
            .await
            .context(
                "udp-client should wait long enough for delayed reverse start confirmation",
            )??;

        assert!(out.final_report.is_final);
        assert_eq!(out.final_report.summary.mode, UdpPerfMode::Reverse);
        assert!(out.client_rx.reverse_packets > 0);

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
            json.pointer("/end/sum_reverse_receiver/packets")
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
        let mut client_tx = super::ClientTxStats::default();
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
            1,
            &wire_report(interval_report(2, 100_000, 100_000))?,
            &mut output,
            true,
            output_policy,
            &mut client_rx,
            &mut client_tx,
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
            1,
            &wire_report(interval_report(3, 200_000, 100_000))?,
            &mut output,
            true,
            output_policy,
            &mut client_rx,
            &mut client_tx,
        )?;
        assert!(
            output.contains("REV"),
            "next in-order interval should output reverse direction line, got: {output}"
        );
        Ok(())
    }

    #[test]
    fn udp_client_final_summary_uses_synced_reverse_interval_after_lost_interval_report(
    ) -> Result<()> {
        let output_policy = super::OutputPolicy {
            stream_interval: false,
            collect_interval: true,
        };
        let mut client_rx = super::ClientRxStats::default();
        let mut client_tx = super::ClientTxStats::default();
        let mut output = String::new();

        client_rx.on_data_packet(&UdpPerfDataPacket {
            header: UdpPerfDataHeader {
                session_id: 1,
                stream_id: 1,
                direction: UdpPerfDirection::Reverse,
                seq: 1,
                send_ts_micros: 1_000,
                payload_len: 32,
            },
            payload: vec![0x5a; 32],
        });
        let _ = super::handle_incoming_packet(
            1,
            &wire_report(interval_report(1, 100_000, 100_000))?,
            &mut output,
            true,
            output_policy,
            &mut client_rx,
            &mut client_tx,
        )?;

        for seq in [2, 3] {
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

        let mut final_report = interval_report(3, 300_000, 100_000);
        final_report.is_final = true;
        let final_summary = super::build_final_client_view_summary(&final_report, &mut client_rx);

        assert_eq!(
            final_summary.reverse.interval.packets, 1,
            "final reverse interval should stay aligned to final report window after a lost interval report"
        );
        assert_eq!(
            final_summary.interval.packets, 1,
            "merged final interval should not accumulate multiple windows after a lost interval report"
        );
        Ok(())
    }

    #[test]
    fn udp_client_ignores_report_from_other_session() -> Result<()> {
        let output_policy = super::OutputPolicy {
            stream_interval: false,
            collect_interval: true,
        };
        let mut client_rx = super::ClientRxStats::default();
        let mut client_tx = super::ClientTxStats::default();
        let mut output = String::new();
        let mut report = interval_report(1, 100_000, 100_000);
        report.summary.session_id = 2;

        let incoming = super::handle_incoming_packet(
            1,
            &wire_report(report)?,
            &mut output,
            true,
            output_policy,
            &mut client_rx,
            &mut client_tx,
        )?;

        assert!(
            !incoming.matched_session,
            "report from another session should be ignored"
        );
        assert!(incoming.report.is_none());
        assert!(output.is_empty());
        Ok(())
    }

    #[test]
    fn udp_client_ignores_data_from_other_session() -> Result<()> {
        let output_policy = super::OutputPolicy {
            stream_interval: false,
            collect_interval: true,
        };
        let mut client_rx = super::ClientRxStats::default();
        let mut client_tx = super::ClientTxStats::default();
        let mut output = String::new();

        let incoming = super::handle_incoming_packet(
            1,
            &wire_reverse_data(2, 1, 32)?,
            &mut output,
            true,
            output_policy,
            &mut client_rx,
            &mut client_tx,
        )?;

        assert!(
            !incoming.matched_session,
            "data from another session should be ignored"
        );
        assert!(incoming.report.is_none());
        assert_eq!(client_rx.reverse_packets, 0);
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
            .pointer("/end/sum_forward_sender/packets")
            .and_then(Value::as_u64)
            .context("missing end.sum_forward_sender.packets")?;
        let forward_packets = json
            .pointer("/end/sum_forward_receiver/packets")
            .and_then(Value::as_u64)
            .context("missing end.sum_forward_receiver.packets")?;

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
            .pointer("/end/sum_reverse_sender/packets")
            .and_then(Value::as_u64)
            .context("missing end.sum_reverse_sender.packets")?;
        let reverse_packets = json
            .pointer("/end/sum_reverse_receiver/packets")
            .and_then(Value::as_u64)
            .context("missing end.sum_reverse_receiver.packets")?;

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
            .pointer("/end/sum_forward_sender/packets")
            .and_then(Value::as_u64)
            .context("missing end.sum_forward_sender.packets")?;
        let forward_packets = json
            .pointer("/end/sum_forward_receiver/packets")
            .and_then(Value::as_u64)
            .context("missing end.sum_forward_receiver.packets")?;
        let reverse_packets = json
            .pointer("/end/sum_reverse_receiver/packets")
            .and_then(Value::as_u64)
            .context("missing end.sum_reverse_receiver.packets")?;

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

    async fn spawn_drop_first_start_reverse_server_with_response_delay(
        response_delay: Duration,
    ) -> Result<(String, tokio::task::JoinHandle<Result<()>>)> {
        let socket = UdpSocket::bind("127.0.0.1:0").await?;
        let addr = socket.local_addr()?;
        let task = tokio::spawn(async move {
            let mut buf = vec![0u8; 64 * 1024];
            let mut client = None;
            let mut session_id = None;
            let mut start_seen = 0u64;
            let mut first_packet_sent = false;

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
                        if start_seen == 1 || first_packet_sent {
                            continue;
                        }

                        tokio::time::sleep(response_delay).await;
                        socket
                            .send_to(&wire_reverse_data(start.session_id, 1, 32)?, from)
                            .await?;
                        first_packet_sent = true;
                    }
                    UdpPerfWirePacket::Control(UdpPerfControlPacket::Stop(stop)) => {
                        ensure!(start_seen >= 2, "client did not retry START before STOP");
                        ensure!(
                            first_packet_sent,
                            "client timed out before delayed reverse confirmation arrived"
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

    async fn spawn_delayed_bidir_confirmation_server(
        first_reverse_delay: Duration,
    ) -> Result<(String, tokio::task::JoinHandle<Result<()>>)> {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let addr = socket.local_addr()?;
        let task = tokio::spawn({
            let socket = Arc::clone(&socket);
            async move {
                let mut buf = vec![0u8; 64 * 1024];
                let first_reverse_sent = Arc::new(AtomicU64::new(0));
                let mut client = None;
                let mut session_id = None;
                let mut reverse_sender = None;
                let mut forward_before_reverse = 0u64;
                let mut total_forward_packets = 0u64;
                let mut total_forward_bytes = 0u64;

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
                            if reverse_sender.is_none() {
                                let send_socket = Arc::clone(&socket);
                                let sent = Arc::clone(&first_reverse_sent);
                                reverse_sender = Some(tokio::spawn(async move {
                                    tokio::time::sleep(first_reverse_delay).await;
                                    send_socket
                                        .send_to(&wire_reverse_data(start.session_id, 1, 32)?, from)
                                        .await?;
                                    sent.store(1, Ordering::SeqCst);
                                    Result::<()>::Ok(())
                                }));
                            }
                        }
                        UdpPerfWirePacket::Data(data)
                            if matches!(data.header.direction, UdpPerfDirection::Forward) =>
                        {
                            total_forward_packets = total_forward_packets.saturating_add(1);
                            total_forward_bytes =
                                total_forward_bytes.saturating_add(data.payload.len() as u64);
                            if first_reverse_sent.load(Ordering::SeqCst) == 0 {
                                forward_before_reverse = forward_before_reverse.saturating_add(1);
                            }
                        }
                        UdpPerfWirePacket::Control(UdpPerfControlPacket::Stop(stop)) => {
                            if let Some(sender) = reverse_sender.take() {
                                sender.await.context("reverse sender join failed")??;
                            }
                            ensure!(
                                forward_before_reverse > 0,
                                "bidir client sent no forward data before first reverse packet"
                            );
                            let sid = session_id.unwrap_or(stop.session_id);
                            let peer = client.unwrap_or(from);
                            let reverse = UdpPerfCountersSummary {
                                packets: 1,
                                bytes: 32,
                                ..UdpPerfCountersSummary::default()
                            };
                            let forward = UdpPerfCountersSummary {
                                packets: total_forward_packets,
                                bytes: total_forward_bytes,
                                ..UdpPerfCountersSummary::default()
                            };
                            socket
                                .send_to(
                                    &wire_report(UdpPerfReport {
                                        report_seq: 1,
                                        is_final: true,
                                        summary: UdpPerfSummary {
                                            session_id: sid,
                                            mode: UdpPerfMode::Bidir,
                                            total_elapsed_micros: 1_000_000,
                                            interval_elapsed_micros: 1_000_000,
                                            total: UdpPerfCountersSummary {
                                                packets: forward
                                                    .packets
                                                    .saturating_add(reverse.packets),
                                                bytes: forward.bytes.saturating_add(reverse.bytes),
                                                ..UdpPerfCountersSummary::default()
                                            },
                                            interval: UdpPerfCountersSummary {
                                                packets: forward
                                                    .packets
                                                    .saturating_add(reverse.packets),
                                                bytes: forward.bytes.saturating_add(reverse.bytes),
                                                ..UdpPerfCountersSummary::default()
                                            },
                                            forward: UdpPerfDirectionSummary {
                                                total: forward.clone(),
                                                interval: forward,
                                            },
                                            reverse: UdpPerfDirectionSummary {
                                                total: reverse.clone(),
                                                interval: reverse,
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

    async fn spawn_blackhole_server() -> Result<(
        String,
        oneshot::Receiver<()>,
        tokio::task::JoinHandle<Result<()>>,
    )> {
        let socket = UdpSocket::bind("127.0.0.1:0").await?;
        let addr = socket.local_addr()?;
        let (tx, rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut buf = vec![0u8; 64 * 1024];
            let mut tx = Some(tx);

            loop {
                let (n, _) = socket.recv_from(&mut buf).await?;
                if matches!(
                    UdpPerfWirePacket::decode(&buf[..n])?,
                    UdpPerfWirePacket::Control(UdpPerfControlPacket::Start(_))
                ) {
                    if let Some(tx) = tx.take() {
                        let _ = tx.send(());
                    }
                }
            }
        });
        Ok((addr.to_string(), rx, task))
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

    async fn spawn_forward_exact_count_server(
        expected_packets: u64,
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
                            forward_packets = forward_packets.saturating_add(1);
                        }
                    }
                    UdpPerfWirePacket::Control(UdpPerfControlPacket::Stop(stop)) => {
                        ensure!(
                            forward_packets == expected_packets,
                            "expected {expected_packets} forward packets before STOP, got {forward_packets}"
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

    async fn spawn_silent_forward_final_report_server(
    ) -> Result<(String, tokio::task::JoinHandle<Result<()>>)> {
        let socket = UdpSocket::bind("127.0.0.1:0").await?;
        let addr = socket.local_addr()?;
        let task = tokio::spawn(async move {
            let mut buf = vec![0u8; 64 * 1024];
            let mut client = None;
            let mut session_id = None;
            let mut hello_count = 0u64;
            let mut start_count = 0u64;
            let mut forward_packets = 0u64;

            loop {
                let (n, from) = socket.recv_from(&mut buf).await?;
                match UdpPerfWirePacket::decode(&buf[..n])? {
                    UdpPerfWirePacket::Control(UdpPerfControlPacket::Hello(hello)) => {
                        client = Some(from);
                        session_id = Some(hello.session_id);
                        hello_count = hello_count.saturating_add(1);
                    }
                    UdpPerfWirePacket::Control(UdpPerfControlPacket::Start(start)) => {
                        client = Some(from);
                        session_id = Some(start.session_id);
                        start_count = start_count.saturating_add(1);
                    }
                    UdpPerfWirePacket::Data(data) => {
                        if matches!(data.header.direction, UdpPerfDirection::Forward) {
                            forward_packets = forward_packets.saturating_add(1);
                        }
                    }
                    UdpPerfWirePacket::Control(UdpPerfControlPacket::Stop(stop)) => {
                        ensure!(hello_count >= 1, "client did not send initial HELLO");
                        ensure!(start_count >= 1, "client did not send initial START");
                        ensure!(
                            hello_count <= 2,
                            "forward run retried HELLO too many times: {hello_count}"
                        );
                        ensure!(
                            start_count <= 2,
                            "forward run retried START too many times: {start_count}"
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

    async fn spawn_mismatched_final_report_server(
    ) -> Result<(String, tokio::task::JoinHandle<Result<()>>)> {
        let socket = UdpSocket::bind("127.0.0.1:0").await?;
        let addr = socket.local_addr()?;
        let task = tokio::spawn(async move {
            let mut buf = vec![0u8; 64 * 1024];
            let mut client = None;
            let mut session_id = None;
            let mut fake_report_sent = false;
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
                        if !fake_report_sent {
                            fake_report_sent = true;
                            socket
                                .send_to(
                                    &wire_report(UdpPerfReport {
                                        report_seq: 1,
                                        is_final: true,
                                        summary: UdpPerfSummary {
                                            session_id: u64::MAX,
                                            mode: start.mode,
                                            total_elapsed_micros: 1_000_000,
                                            interval_elapsed_micros: 1_000_000,
                                            total: UdpPerfCountersSummary::default(),
                                            interval: UdpPerfCountersSummary::default(),
                                            forward: UdpPerfDirectionSummary::default(),
                                            reverse: UdpPerfDirectionSummary::default(),
                                        },
                                    })?,
                                    from,
                                )
                                .await?;
                        }
                    }
                    UdpPerfWirePacket::Data(data) => {
                        if matches!(data.header.direction, UdpPerfDirection::Forward) {
                            forward_packets = forward_packets.saturating_add(1);
                        }
                    }
                    UdpPerfWirePacket::Control(UdpPerfControlPacket::Stop(stop)) => {
                        ensure!(
                            fake_report_sent,
                            "server never sent mismatched final report before STOP"
                        );
                        let sid = session_id.unwrap_or(stop.session_id);
                        let peer = client.unwrap_or(from);
                        socket
                            .send_to(
                                &wire_report(UdpPerfReport {
                                    report_seq: 2,
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

    async fn spawn_delayed_forward_final_report_server(
        final_report_delay: Duration,
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
                        let sid = session_id.unwrap_or(stop.session_id);
                        let peer = client.unwrap_or(from);
                        tokio::time::sleep(final_report_delay).await;
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
                                        total: UdpPerfCountersSummary::default(),
                                        interval: UdpPerfCountersSummary::default(),
                                        forward: UdpPerfDirectionSummary::default(),
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

    async fn spawn_drop_first_stop_forward_server(
    ) -> Result<(String, tokio::task::JoinHandle<Result<()>>)> {
        let socket = UdpSocket::bind("127.0.0.1:0").await?;
        let addr = socket.local_addr()?;
        let task = tokio::spawn(async move {
            let mut buf = vec![0u8; 64 * 1024];
            let mut client = None;
            let mut session_id = None;
            let mut stop_count = 0u64;

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
                        stop_count = stop_count.saturating_add(1);
                        if stop_count == 1 {
                            continue;
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
                                        mode: UdpPerfMode::Forward,
                                        total_elapsed_micros: 1_000,
                                        interval_elapsed_micros: 1_000,
                                        total: UdpPerfCountersSummary::default(),
                                        interval: UdpPerfCountersSummary::default(),
                                        forward: UdpPerfDirectionSummary::default(),
                                        reverse: UdpPerfDirectionSummary::default(),
                                    },
                                })?,
                                peer,
                            )
                            .await?;
                        ensure!(stop_count >= 2, "client did not retry STOP");
                        return Ok(());
                    }
                    _ => {}
                }
            }
        });
        Ok((addr.to_string(), task))
    }

    async fn spawn_delayed_reverse_start_confirmation_server(
        first_packet_delay: Duration,
    ) -> Result<(String, tokio::task::JoinHandle<Result<()>>)> {
        let socket = UdpSocket::bind("127.0.0.1:0").await?;
        let addr = socket.local_addr()?;
        let task = tokio::spawn(async move {
            let mut buf = vec![0u8; 64 * 1024];
            let mut client = None;
            let mut session_id = None;
            let mut first_packet_sent = false;

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
                        if !first_packet_sent {
                            first_packet_sent = true;
                            tokio::time::sleep(first_packet_delay).await;
                            socket
                                .send_to(&wire_reverse_data(start.session_id, 1, 32)?, from)
                                .await?;
                        }
                    }
                    UdpPerfWirePacket::Control(UdpPerfControlPacket::Stop(stop)) => {
                        ensure!(
                            first_packet_sent,
                            "server never sent delayed reverse start confirmation"
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
