use std::{
    ffi::OsStr,
    fs,
    net::{SocketAddr, TcpListener},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context, Result};
use rcgen::{CertificateParams, DistinguishedName, DnType, SanType};
use tokio::{
    io::AsyncReadExt,
    net::UdpSocket,
    process::{Child, Command},
    sync::oneshot,
    task::JoinHandle,
    time::Instant,
};

const TEST_SECRET: &str = "rtun-e2e-secret";
const READY_TIMEOUT: Duration = Duration::from_secs(25);
const ROUND_TRIP_TIMEOUT: Duration = Duration::from_millis(800);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_quic_smoke_e2e() -> Result<()> {
    let temp = TempDir::new()?;
    let (cert_path, key_path) = write_test_cert_pair(temp.path())?;

    let signal_port = reserve_tcp_port()?;
    let relay_port = reserve_udp_port()?;

    let (echo_addr, echo_stop, echo_task) = spawn_udp_echo_server().await?;
    let signal_addr = format!("127.0.0.1:{signal_port}");
    let relay_addr: SocketAddr = format!("127.0.0.1:{relay_port}").parse()?;
    let quic_url = format!("quic://{signal_addr}");
    let rule = format!("udp://{relay_addr}?to={echo_addr}");

    let mut listen = Proc::spawn(
        "listen",
        [
            "agent",
            "listen",
            "--addr",
            signal_addr.as_str(),
            "--https-key",
            key_path.to_string_lossy().as_ref(),
            "--https-cert",
            cert_path.to_string_lossy().as_ref(),
            "--secret",
            TEST_SECRET,
        ],
    )
    .await?;

    let mut publisher = Proc::spawn(
        "pub",
        [
            "agent",
            "pub",
            quic_url.as_str(),
            "--agent",
            "relay-e2e",
            "--expire_in",
            "10",
            "--secret",
            TEST_SECRET,
            "--quic-insecure",
        ],
    )
    .await?;

    let mut relay = Proc::spawn(
        "relay",
        [
            "relay",
            "-L",
            rule.as_str(),
            quic_url.as_str(),
            "--agent",
            "^relay-e2e$",
            "--secret",
            TEST_SECRET,
            "--quic-insecure",
            "--udp-idle-timeout",
            "30",
        ],
    )
    .await?;

    let run_result = async {
        wait_for_ready_roundtrip(&mut listen, &mut publisher, &mut relay, relay_addr).await?;

        for idx in 0..5 {
            let payload = format!("relay-smoke-{idx}").into_bytes();
            udp_roundtrip(relay_addr, &payload).await?;
        }

        Ok::<(), anyhow::Error>(())
    }
    .await;

    relay.terminate().await;
    publisher.terminate().await;
    listen.terminate().await;

    let _ = echo_stop.send(());
    let _ = tokio::time::timeout(Duration::from_secs(2), echo_task).await;

    run_result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_quic_smoke_multi_tunnel_e2e() -> Result<()> {
    let temp = TempDir::new()?;
    let (cert_path, key_path) = write_test_cert_pair(temp.path())?;

    let signal_port = reserve_tcp_port()?;
    let relay_port = reserve_udp_port()?;

    let (echo_addr, echo_stop, echo_task) = spawn_udp_echo_server().await?;
    let signal_addr = format!("127.0.0.1:{signal_port}");
    let relay_addr: SocketAddr = format!("127.0.0.1:{relay_port}").parse()?;
    let quic_url = format!("quic://{signal_addr}");
    let rule = format!("udp://{relay_addr}?to={echo_addr}");

    let mut listen = Proc::spawn(
        "listen",
        [
            "agent",
            "listen",
            "--addr",
            signal_addr.as_str(),
            "--https-key",
            key_path.to_string_lossy().as_ref(),
            "--https-cert",
            cert_path.to_string_lossy().as_ref(),
            "--secret",
            TEST_SECRET,
        ],
    )
    .await?;

    let mut publisher = Proc::spawn(
        "pub",
        [
            "agent",
            "pub",
            quic_url.as_str(),
            "--agent",
            "relay-e2e-multi",
            "--expire_in",
            "10",
            "--secret",
            TEST_SECRET,
            "--quic-insecure",
        ],
    )
    .await?;

    let mut relay = Proc::spawn(
        "relay",
        [
            "relay",
            "-L",
            rule.as_str(),
            quic_url.as_str(),
            "--agent",
            "^relay-e2e-multi$",
            "--secret",
            TEST_SECRET,
            "--quic-insecure",
            "--udp-idle-timeout",
            "30",
            "--p2p-min-channels",
            "1",
            "--p2p-max-channels",
            "2",
        ],
    )
    .await?;

    let run_result = async {
        wait_for_ready_roundtrip(&mut listen, &mut publisher, &mut relay, relay_addr).await?;

        for idx in 0..5 {
            let payload = format!("relay-multi-{idx}").into_bytes();
            udp_roundtrip(relay_addr, &payload).await?;
        }

        Ok::<(), anyhow::Error>(())
    }
    .await;

    relay.terminate().await;
    publisher.terminate().await;
    listen.terminate().await;

    let _ = echo_stop.send(());
    let _ = tokio::time::timeout(Duration::from_secs(2), echo_task).await;

    run_result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_quic_min_zero_opens_tunnel_on_demand_e2e() -> Result<()> {
    let temp = TempDir::new()?;
    let (cert_path, key_path) = write_test_cert_pair(temp.path())?;

    let signal_port = reserve_tcp_port()?;
    let relay_port = reserve_udp_port()?;

    let (echo_addr, echo_stop, echo_task) = spawn_udp_echo_server().await?;
    let signal_addr = format!("127.0.0.1:{signal_port}");
    let relay_addr: SocketAddr = format!("127.0.0.1:{relay_port}").parse()?;
    let quic_url = format!("quic://{signal_addr}");
    let rule = format!("udp://{relay_addr}?to={echo_addr}");

    let mut listen = Proc::spawn(
        "listen",
        [
            "agent",
            "listen",
            "--addr",
            signal_addr.as_str(),
            "--https-key",
            key_path.to_string_lossy().as_ref(),
            "--https-cert",
            cert_path.to_string_lossy().as_ref(),
            "--secret",
            TEST_SECRET,
        ],
    )
    .await?;

    let mut publisher = Proc::spawn(
        "pub",
        [
            "agent",
            "pub",
            quic_url.as_str(),
            "--agent",
            "relay-e2e-min-zero",
            "--expire_in",
            "10",
            "--secret",
            TEST_SECRET,
            "--quic-insecure",
        ],
    )
    .await?;

    let mut relay = Proc::spawn(
        "relay",
        [
            "relay",
            "-L",
            rule.as_str(),
            quic_url.as_str(),
            "--agent",
            "^relay-e2e-min-zero$",
            "--secret",
            TEST_SECRET,
            "--quic-insecure",
            "--udp-idle-timeout",
            "30",
            "--p2p-min-channels",
            "0",
            "--p2p-max-channels",
            "1",
        ],
    )
    .await?;

    let run_result = async {
        wait_for_process_log_contains(&mut relay, "relay(udp) listen on", Duration::from_secs(10))
            .await?;
        listen.ensure_running()?;
        publisher.ensure_running()?;
        relay.ensure_running()?;

        // No bootstrap tunnel at min=0. First packet should trigger demand-open and still roundtrip.
        let first_payload = b"relay-demand-open-first".to_vec();
        udp_roundtrip_with_timeout(
            relay_addr,
            first_payload.as_slice(),
            Duration::from_secs(20),
        )
        .await?;

        wait_for_process_log_contains(
            &mut relay,
            "relay tunnel connected(demand-open):",
            Duration::from_secs(10),
        )
        .await?;

        for idx in 0..3 {
            let payload = format!("relay-demand-open-{idx}").into_bytes();
            udp_roundtrip(relay_addr, payload.as_slice()).await?;
        }

        Ok::<(), anyhow::Error>(())
    }
    .await;

    relay.terminate().await;
    publisher.terminate().await;
    listen.terminate().await;

    let _ = echo_stop.send(());
    let _ = tokio::time::timeout(Duration::from_secs(2), echo_task).await;

    run_result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "manual reproducer: fixed source burst + sticky target peer"]
async fn relay_quic_fixed_source_burst_multi_tunnel_e2e() -> Result<()> {
    let temp = TempDir::new()?;
    let (cert_path, key_path) = write_test_cert_pair(temp.path())?;

    let signal_port = reserve_tcp_port()?;
    let relay_port = reserve_udp_port()?;

    let (echo_addr, sticky_stats, echo_stop, echo_task) = spawn_udp_sticky_echo_server().await?;
    let signal_addr = format!("127.0.0.1:{signal_port}");
    let relay_addr: SocketAddr = format!("127.0.0.1:{relay_port}").parse()?;
    let quic_url = format!("quic://{signal_addr}");
    let rule = format!("udp://{relay_addr}?to={echo_addr}");

    let mut listen = Proc::spawn(
        "listen",
        [
            "agent",
            "listen",
            "--addr",
            signal_addr.as_str(),
            "--https-key",
            key_path.to_string_lossy().as_ref(),
            "--https-cert",
            cert_path.to_string_lossy().as_ref(),
            "--secret",
            TEST_SECRET,
        ],
    )
    .await?;

    let mut publisher = Proc::spawn(
        "pub",
        [
            "agent",
            "pub",
            quic_url.as_str(),
            "--agent",
            "relay-e2e-fixed-src",
            "--expire_in",
            "10",
            "--secret",
            TEST_SECRET,
            "--quic-insecure",
        ],
    )
    .await?;

    let mut relay = Proc::spawn(
        "relay",
        [
            "relay",
            "-L",
            rule.as_str(),
            quic_url.as_str(),
            "--agent",
            "^relay-e2e-fixed-src$",
            "--secret",
            TEST_SECRET,
            "--quic-insecure",
            "--udp-idle-timeout",
            "30",
            "--p2p-min-channels",
            "1",
            "--p2p-max-channels",
            "3",
        ],
    )
    .await?;

    let run_result = async {
        wait_for_process_log_contains(
            &mut relay,
            "relay tunnel connected: idx [0]",
            Duration::from_secs(30),
        )
        .await?;
        tokio::time::sleep(Duration::from_secs(1)).await;

        let source = UdpSocket::bind("127.0.0.1:0")
            .await
            .with_context(|| "bind fixed source udp client failed")?;

        let lock_probe = b"fixed-source-lock-probe";
        source
            .send_to(lock_probe.as_slice(), relay_addr)
            .await
            .with_context(|| format!("send lock probe to relay failed [{relay_addr}]"))?;
        let lock_replies = recv_udp_replies_until(&source, 1, Duration::from_secs(1))
            .await
            .with_context(|| "recv lock probe reply failed")?;
        if lock_replies.len() != 1 || lock_replies[0] != lock_probe {
            let stats = sticky_stats_snapshot(&sticky_stats);
            bail!(
                "sticky lock probe failed, got {:?}, want {:?}, sticky_stats={stats:?}\n{}",
                lock_replies,
                lock_probe,
                dump_process_logs([&listen, &publisher, &relay])
            );
        }

        set_sticky_mode(&sticky_stats, true);
        let baseline = sticky_stats_snapshot(&sticky_stats).accepted_payloads.len();
        let mut expected = Vec::with_capacity(5);
        for idx in 0..5 {
            let payload = format!("fixed-source-burst-{idx}").into_bytes();
            source
                .send_to(payload.as_slice(), relay_addr)
                .await
                .with_context(|| format!("send burst udp to relay failed [{relay_addr}]"))?;
            expected.push(payload);
        }

        let replies = recv_udp_replies_until(&source, expected.len(), Duration::from_secs(3))
            .await
            .with_context(|| "recv burst replies failed")?;

        if replies.len() != expected.len() {
            let stats = sticky_stats_snapshot(&sticky_stats);
            bail!(
                "fixed source burst reply count mismatch: got [{}], want [{}], sticky_stats={stats:?}\n{}",
                replies.len(),
                expected.len(),
                dump_process_logs([&listen, &publisher, &relay])
            );
        }

        let mut got_sorted = replies;
        got_sorted.sort();
        let mut expected_sorted = expected;
        expected_sorted.sort();
        if got_sorted != expected_sorted {
            let stats = sticky_stats_snapshot(&sticky_stats);
            bail!(
                "fixed source burst payload mismatch, sticky_stats={stats:?}\n{}",
                dump_process_logs([&listen, &publisher, &relay])
            );
        }

        let stats = wait_for_sticky_accepted_count(
            &sticky_stats,
            baseline.saturating_add(5),
            Duration::from_secs(2),
        )
        .await;
        if stats.accepted_payloads.len() < baseline.saturating_add(5) {
            bail!(
                "sticky target observed too few packets after burst: got [{}], want_at_least [{}], sticky_stats={stats:?}\n{}",
                stats.accepted_payloads.len(),
                baseline.saturating_add(5),
                dump_process_logs([&listen, &publisher, &relay])
            );
        }
        if stats.dropped_non_primary > 0 {
            bail!(
                "sticky target dropped packets from non-primary peer, sticky_stats={stats:?}\n{}",
                dump_process_logs([&listen, &publisher, &relay])
            );
        }

        Ok::<(), anyhow::Error>(())
    }
    .await;

    relay.terminate().await;
    publisher.terminate().await;
    listen.terminate().await;

    let _ = echo_stop.send(());
    let _ = tokio::time::timeout(Duration::from_secs(2), echo_task).await;

    run_result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_quic_same_flow_target_src_port_should_stay_stable_e2e() -> Result<()> {
    let temp = TempDir::new()?;
    let (cert_path, key_path) = write_test_cert_pair(temp.path())?;

    let signal_port = reserve_tcp_port()?;
    let relay_port = reserve_udp_port()?;

    let (echo_addr, observer, echo_stop, echo_task) = spawn_udp_observer_echo_server().await?;
    let signal_addr = format!("127.0.0.1:{signal_port}");
    let relay_addr: SocketAddr = format!("127.0.0.1:{relay_port}").parse()?;
    let quic_url = format!("quic://{signal_addr}");
    let rule = format!("udp://{relay_addr}?to={echo_addr}");

    let mut listen = Proc::spawn(
        "listen",
        [
            "agent",
            "listen",
            "--addr",
            signal_addr.as_str(),
            "--https-key",
            key_path.to_string_lossy().as_ref(),
            "--https-cert",
            cert_path.to_string_lossy().as_ref(),
            "--secret",
            TEST_SECRET,
        ],
    )
    .await?;

    let mut publisher = Proc::spawn(
        "pub",
        [
            "agent",
            "pub",
            quic_url.as_str(),
            "--agent",
            "relay-e2e-stable-flow",
            "--expire_in",
            "10",
            "--secret",
            TEST_SECRET,
            "--quic-insecure",
        ],
    )
    .await?;

    let mut relay = Proc::spawn(
        "relay",
        [
            "relay",
            "-L",
            rule.as_str(),
            quic_url.as_str(),
            "--agent",
            "^relay-e2e-stable-flow$",
            "--secret",
            TEST_SECRET,
            "--quic-insecure",
            "--udp-idle-timeout",
            "120",
            "--p2p-min-channels",
            "1",
            "--p2p-max-channels",
            "1",
            "--p2p-channel-lifetime",
            "12",
        ],
    )
    .await?;

    let run_result = async {
        wait_for_process_log_contains(
            &mut relay,
            "relay tunnel connected: idx [0]",
            Duration::from_secs(30),
        )
        .await?;
        tokio::time::sleep(Duration::from_secs(1)).await;

        let source = UdpSocket::bind("127.0.0.1:0")
            .await
            .with_context(|| "bind fixed source udp client failed")?;

        let payloads: Vec<Vec<u8>> = (0..5)
            .map(|i| format!("stable-flow-{i}").into_bytes())
            .collect();

        // First packets go through initial tunnel and must all roundtrip.
        for payload in payloads.iter().take(2) {
            source
                .send_to(payload.as_slice(), relay_addr)
                .await
                .with_context(|| format!("send udp to relay failed [{relay_addr}]"))?;
            let replies = recv_udp_replies_until(&source, 1, Duration::from_secs(3))
                .await
                .with_context(|| "recv pre-rotate reply failed")?;
            if replies.len() != 1 || replies[0] != *payload {
                bail!(
                    "pre-rotate roundtrip mismatch, got={replies:?}, want={payload:?}\n{}",
                    dump_process_logs([&listen, &publisher, &relay])
                );
            }
        }

        wait_for_process_log_contains(
            &mut relay,
            "relay tunnel rotated:",
            Duration::from_secs(45),
        )
        .await?;

        // Remaining packets are sent after tunnel rotation.
        tokio::time::sleep(Duration::from_secs(6)).await;
        for payload in payloads.iter().skip(2) {
            source
                .send_to(payload.as_slice(), relay_addr)
                .await
                .with_context(|| format!("send udp to relay failed [{relay_addr}]"))?;
            let replies = recv_udp_replies_until(&source, 1, Duration::from_secs(3))
                .await
                .with_context(|| "recv post-rotate reply failed")?;
            if replies.len() != 1 || replies[0] != *payload {
                bail!(
                    "post-rotate roundtrip mismatch, got={replies:?}, want={payload:?}\n{}",
                    dump_process_logs([&listen, &publisher, &relay])
                );
            }
        }

        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let snapshot = observer_snapshot(&observer);
            let observed: Vec<(Vec<u8>, SocketAddr)> = payloads
                .iter()
                .filter_map(|payload| {
                    snapshot
                        .records
                        .iter()
                        .find(|(p, _)| p == payload)
                        .map(|(_, addr)| (payload.clone(), *addr))
                })
                .collect();
            let src_ports: std::collections::BTreeSet<u16> =
                observed.iter().map(|(_, addr)| addr.port()).collect();

            if observed.len() >= 3 && src_ports.len() > 1 {
                bail!(
                    "same flow target-side source port changed across migration, ports={src_ports:?}, observed={observed:?}\n{}",
                    dump_process_logs([&listen, &publisher, &relay])
                );
            }

            if observed.len() == payloads.len() {
                if src_ports.len() != 1 {
                    bail!(
                        "same flow target-side source port changed across migration, ports={src_ports:?}, observed={observed:?}\n{}",
                        dump_process_logs([&listen, &publisher, &relay])
                    );
                }
                break;
            }

            if Instant::now() >= deadline {
                bail!(
                    "wait observed payload timeout, payloads={payloads:?}, observed_records={:?}\n{}",
                    snapshot.records,
                    dump_process_logs([&listen, &publisher, &relay])
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        Ok::<(), anyhow::Error>(())
    }
    .await;

    relay.terminate().await;
    publisher.terminate().await;
    listen.terminate().await;

    let _ = echo_stop.send(());
    let _ = tokio::time::timeout(Duration::from_secs(2), echo_task).await;

    run_result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "slow/manual: relay long-run stability; run on demand"]
async fn relay_quic_longrun_e2e() -> Result<()> {
    let temp = TempDir::new()?;
    let (cert_path, key_path) = write_test_cert_pair(temp.path())?;

    let signal_port = reserve_tcp_port()?;
    let relay_port = reserve_udp_port()?;

    let (echo_addr, echo_stop, echo_task) = spawn_udp_echo_server().await?;
    let signal_addr = format!("127.0.0.1:{signal_port}");
    let relay_addr: SocketAddr = format!("127.0.0.1:{relay_port}").parse()?;
    let quic_url = format!("quic://{signal_addr}");
    let rule = format!("udp://{relay_addr}?to={echo_addr}");

    let mut listen = Proc::spawn(
        "listen",
        [
            "agent",
            "listen",
            "--addr",
            signal_addr.as_str(),
            "--https-key",
            key_path.to_string_lossy().as_ref(),
            "--https-cert",
            cert_path.to_string_lossy().as_ref(),
            "--secret",
            TEST_SECRET,
        ],
    )
    .await?;

    let mut publisher = Proc::spawn(
        "pub",
        [
            "agent",
            "pub",
            quic_url.as_str(),
            "--agent",
            "relay-e2e-rotate",
            "--expire_in",
            "10",
            "--secret",
            TEST_SECRET,
            "--quic-insecure",
        ],
    )
    .await?;

    let mut relay = Proc::spawn(
        "relay",
        [
            "relay",
            "-L",
            rule.as_str(),
            quic_url.as_str(),
            "--agent",
            "^relay-e2e-rotate$",
            "--secret",
            TEST_SECRET,
            "--quic-insecure",
            "--udp-idle-timeout",
            "30",
            "--p2p-min-channels",
            "1",
            "--p2p-max-channels",
            "1",
            "--p2p-channel-lifetime",
            "12",
        ],
    )
    .await?;

    let run_result = async {
        wait_for_ready_roundtrip(&mut listen, &mut publisher, &mut relay, relay_addr).await?;

        let deadline = Instant::now() + Duration::from_secs(40);
        let mut idx = 0_u64;
        let mut success = 0_u32;
        let mut consecutive_failures = 0_u32;
        let mut saw_rotation = false;
        while Instant::now() < deadline {
            listen.ensure_running()?;
            publisher.ensure_running()?;
            relay.ensure_running()?;

            let payload = format!("longrun-{idx}").into_bytes();
            match udp_roundtrip(relay_addr, &payload).await {
                Ok(()) => {
                    success += 1;
                    consecutive_failures = 0;
                }
                Err(e) => {
                    consecutive_failures += 1;
                    if consecutive_failures > 10 {
                        bail!("too many consecutive relay failures in longrun test: {e:#}");
                    }
                }
            }
            idx += 1;

            let relay_log = format!("{}\n{}", relay.stdout(), relay.stderr());
            if relay_log.contains("relay tunnel rotated:") {
                saw_rotation = true;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }

        if success == 0 {
            bail!("no successful roundtrip observed in longrun test");
        }

        if !saw_rotation {
            tracing::info!("longrun test finished without tunnel rotation");
        }

        Ok::<(), anyhow::Error>(())
    }
    .await;

    relay.terminate().await;
    publisher.terminate().await;
    listen.terminate().await;

    let _ = echo_stop.send(());
    let _ = tokio::time::timeout(Duration::from_secs(2), echo_task).await;

    run_result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "slow/manual: expire_in is minute-based; run on demand"]
async fn relay_quic_agent_switch_e2e() -> Result<()> {
    let temp = TempDir::new()?;
    let (cert_path, key_path) = write_test_cert_pair(temp.path())?;

    let signal_port = reserve_tcp_port()?;
    let relay_port = reserve_udp_port()?;

    let (echo_addr, echo_stop, echo_task) = spawn_udp_echo_server().await?;
    let signal_addr = format!("127.0.0.1:{signal_port}");
    let relay_addr: SocketAddr = format!("127.0.0.1:{relay_port}").parse()?;
    let quic_url = format!("quic://{signal_addr}");
    let rule = format!("udp://{relay_addr}?to={echo_addr}");

    let mut listen = Proc::spawn(
        "listen",
        [
            "agent",
            "listen",
            "--addr",
            signal_addr.as_str(),
            "--https-key",
            key_path.to_string_lossy().as_ref(),
            "--https-cert",
            cert_path.to_string_lossy().as_ref(),
            "--secret",
            TEST_SECRET,
        ],
    )
    .await?;

    let mut pub_short = Proc::spawn(
        "pub-short",
        [
            "agent",
            "pub",
            quic_url.as_str(),
            "--agent",
            "relay-e2e-short",
            "--expire_in",
            "1",
            "--secret",
            TEST_SECRET,
            "--quic-insecure",
        ],
    )
    .await?;

    let mut relay = Proc::spawn(
        "relay",
        [
            "relay",
            "-L",
            rule.as_str(),
            quic_url.as_str(),
            "--agent",
            "^relay-e2e-(short|long)$",
            "--secret",
            TEST_SECRET,
            "--quic-insecure",
            "--udp-idle-timeout",
            "45",
        ],
    )
    .await?;

    wait_for_ready_roundtrip(&mut listen, &mut pub_short, &mut relay, relay_addr).await?;

    let mut pub_long = Proc::spawn(
        "pub-long",
        [
            "agent",
            "pub",
            quic_url.as_str(),
            "--agent",
            "relay-e2e-long",
            "--expire_in",
            "3",
            "--secret",
            TEST_SECRET,
            "--quic-insecure",
        ],
    )
    .await?;

    let run_result = async {
        let deadline = Instant::now() + Duration::from_secs(75);
        let post_expire_probe_at = Instant::now() + Duration::from_secs(65);
        let mut idx = 0_u64;
        let mut post_expire_success = 0_u32;
        let mut consecutive_failures = 0_u32;
        while Instant::now() < deadline {
            listen.ensure_running()?;
            pub_long.ensure_running()?;
            relay.ensure_running()?;

            let payload = format!("switch-{idx}").into_bytes();
            match udp_roundtrip(relay_addr, &payload).await {
                Ok(()) => {
                    consecutive_failures = 0;
                    if Instant::now() >= post_expire_probe_at {
                        post_expire_success += 1;
                    }
                }
                Err(e) => {
                    consecutive_failures += 1;
                    if consecutive_failures > 10 {
                        bail!("too many consecutive relay failures during switch: {e:#}");
                    }
                }
            }
            idx += 1;
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        if post_expire_success == 0 {
            bail!("no successful roundtrip observed after short agent expiry window");
        }

        Ok::<(), anyhow::Error>(())
    }
    .await;

    relay.terminate().await;
    pub_long.terminate().await;
    pub_short.terminate().await;
    listen.terminate().await;

    let _ = echo_stop.send(());
    let _ = tokio::time::timeout(Duration::from_secs(2), echo_task).await;

    run_result
}

async fn wait_for_ready_roundtrip(
    listen: &mut Proc,
    publisher: &mut Proc,
    relay: &mut Proc,
    relay_addr: SocketAddr,
) -> Result<()> {
    let deadline = Instant::now() + READY_TIMEOUT;
    let mut attempt = 0_u64;

    loop {
        attempt += 1;
        listen.ensure_running()?;
        publisher.ensure_running()?;
        relay.ensure_running()?;

        let payload = format!("ready-{attempt}").into_bytes();
        match udp_roundtrip(relay_addr, payload.as_slice()).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                if Instant::now() >= deadline {
                    bail!(
                        "relay ready timeout after {} attempts, last_err: {}\\n{}",
                        attempt,
                        format!("{e:#}"),
                        dump_process_logs([listen, publisher, relay])
                    );
                }
            }
        }

        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn udp_roundtrip(relay_addr: SocketAddr, payload: &[u8]) -> Result<()> {
    udp_roundtrip_with_timeout(relay_addr, payload, ROUND_TRIP_TIMEOUT).await
}

async fn udp_roundtrip_with_timeout(
    relay_addr: SocketAddr,
    payload: &[u8],
    timeout: Duration,
) -> Result<()> {
    let socket = UdpSocket::bind("127.0.0.1:0")
        .await
        .with_context(|| "bind udp client failed")?;

    socket
        .send_to(payload, relay_addr)
        .await
        .with_context(|| format!("send udp to relay failed [{relay_addr}]"))?;

    let mut recv_buf = [0_u8; 4096];
    let (n, _from) = tokio::time::timeout(timeout, socket.recv_from(&mut recv_buf))
        .await
        .with_context(|| "udp recv timeout")?
        .with_context(|| "recv udp from relay failed")?;

    if &recv_buf[..n] != payload {
        bail!(
            "roundtrip payload mismatch, got [{}], want [{}]",
            String::from_utf8_lossy(&recv_buf[..n]),
            String::from_utf8_lossy(payload)
        );
    }

    Ok(())
}

async fn recv_udp_replies_until(
    socket: &UdpSocket,
    expected: usize,
    timeout: Duration,
) -> Result<Vec<Vec<u8>>> {
    let deadline = Instant::now() + timeout;
    let mut out = Vec::with_capacity(expected);
    let mut recv_buf = [0_u8; 4096];
    while out.len() < expected {
        if Instant::now() >= deadline {
            break;
        }
        let remain = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remain, socket.recv_from(&mut recv_buf)).await {
            Ok(Ok((n, _))) => out.push(recv_buf[..n].to_vec()),
            Ok(Err(e)) => return Err(e).with_context(|| "recv burst reply failed"),
            Err(_) => break,
        }
    }
    Ok(out)
}

async fn wait_for_process_log_contains(
    proc: &mut Proc,
    pattern: &str,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        proc.ensure_running()?;
        let combined = format!("{}\n{}", proc.stdout(), proc.stderr());
        if combined.contains(pattern) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "wait process log timeout, pattern [{}] not found in [{}]",
                pattern,
                proc.name
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn spawn_udp_echo_server() -> Result<(SocketAddr, oneshot::Sender<()>, JoinHandle<Result<()>>)>
{
    let socket = UdpSocket::bind("127.0.0.1:0")
        .await
        .with_context(|| "bind udp echo server failed")?;
    let addr = socket.local_addr()?;

    let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let mut buf = [0_u8; 4096];
        loop {
            tokio::select! {
                _ = &mut stop_rx => {
                    return Ok(());
                }
                r = socket.recv_from(&mut buf) => {
                    let (n, from) = r.with_context(|| "echo recv_from failed")?;
                    socket.send_to(&buf[..n], from)
                        .await
                        .with_context(|| format!("echo send_to failed [{from}]"))?;
                }
            }
        }
    });

    Ok((addr, stop_tx, task))
}

#[derive(Debug, Default, Clone)]
struct UdpObserverStats {
    records: Vec<(Vec<u8>, SocketAddr)>,
}

async fn spawn_udp_observer_echo_server() -> Result<(
    SocketAddr,
    Arc<Mutex<UdpObserverStats>>,
    oneshot::Sender<()>,
    JoinHandle<Result<()>>,
)> {
    let socket = UdpSocket::bind("127.0.0.1:0")
        .await
        .with_context(|| "bind udp observer echo server failed")?;
    let addr = socket.local_addr()?;
    let stats = Arc::new(Mutex::new(UdpObserverStats::default()));

    let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
    let task_stats = stats.clone();
    let task = tokio::spawn(async move {
        let mut buf = [0_u8; 4096];
        loop {
            tokio::select! {
                _ = &mut stop_rx => {
                    return Ok(());
                }
                r = socket.recv_from(&mut buf) => {
                    let (n, from) = r.with_context(|| "observer recv_from failed")?;
                    {
                        let mut guard = task_stats.lock().unwrap();
                        guard.records.push((buf[..n].to_vec(), from));
                    }
                    socket.send_to(&buf[..n], from)
                        .await
                        .with_context(|| format!("observer send_to failed [{from}]"))?;
                }
            }
        }
    });

    Ok((addr, stats, stop_tx, task))
}

fn observer_snapshot(stats: &Arc<Mutex<UdpObserverStats>>) -> UdpObserverStats {
    let guard = stats.lock().unwrap();
    guard.clone()
}
#[derive(Debug, Default, Clone)]
struct UdpStickyEchoStats {
    sticky_peer: Option<SocketAddr>,
    sticky_mode: bool,
    observed_peers: Vec<SocketAddr>,
    dropped_non_primary: usize,
    accepted_payloads: Vec<Vec<u8>>,
}

async fn spawn_udp_sticky_echo_server() -> Result<(
    SocketAddr,
    Arc<Mutex<UdpStickyEchoStats>>,
    oneshot::Sender<()>,
    JoinHandle<Result<()>>,
)> {
    let socket = UdpSocket::bind("127.0.0.1:0")
        .await
        .with_context(|| "bind udp sticky echo server failed")?;
    let addr = socket.local_addr()?;
    let stats = Arc::new(Mutex::new(UdpStickyEchoStats::default()));

    let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
    let task_stats = stats.clone();
    let task = tokio::spawn(async move {
        let mut buf = [0_u8; 4096];
        loop {
            tokio::select! {
                _ = &mut stop_rx => {
                    return Ok(());
                }
                r = socket.recv_from(&mut buf) => {
                    let (n, from) = r.with_context(|| "sticky echo recv_from failed")?;
                    let accepted = {
                        let mut stats = task_stats.lock().unwrap();
                        if !stats.observed_peers.contains(&from) {
                            stats.observed_peers.push(from);
                        }
                        if !stats.sticky_mode {
                            stats.sticky_peer = Some(from);
                            stats.accepted_payloads.push(buf[..n].to_vec());
                            true
                        } else if stats.sticky_peer == Some(from) {
                            stats.accepted_payloads.push(buf[..n].to_vec());
                            true
                        } else {
                            stats.dropped_non_primary = stats.dropped_non_primary.saturating_add(1);
                            false
                        }
                    };
                    if accepted {
                        socket.send_to(&buf[..n], from)
                            .await
                            .with_context(|| format!("sticky echo send_to failed [{from}]"))?;
                    }
                }
            }
        }
    });

    Ok((addr, stats, stop_tx, task))
}

fn sticky_stats_snapshot(stats: &Arc<Mutex<UdpStickyEchoStats>>) -> UdpStickyEchoStats {
    let guard = stats.lock().unwrap();
    guard.clone()
}

fn set_sticky_mode(stats: &Arc<Mutex<UdpStickyEchoStats>>, sticky: bool) {
    let mut guard = stats.lock().unwrap();
    guard.sticky_mode = sticky;
}

async fn wait_for_sticky_accepted_count(
    stats: &Arc<Mutex<UdpStickyEchoStats>>,
    expected: usize,
    timeout: Duration,
) -> UdpStickyEchoStats {
    let deadline = Instant::now() + timeout;
    loop {
        let snapshot = sticky_stats_snapshot(stats);
        if snapshot.accepted_payloads.len() >= expected || Instant::now() >= deadline {
            return snapshot;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn reserve_tcp_port() -> Result<u16> {
    let listener =
        TcpListener::bind(("127.0.0.1", 0)).with_context(|| "reserve tcp port failed")?;
    Ok(listener.local_addr()?.port())
}

fn reserve_udp_port() -> Result<u16> {
    let socket =
        std::net::UdpSocket::bind(("127.0.0.1", 0)).with_context(|| "reserve udp port failed")?;
    Ok(socket.local_addr()?.port())
}

fn rtun_bin() -> Result<PathBuf> {
    if let Some(v) = std::env::var_os("CARGO_BIN_EXE_rtun") {
        return Ok(PathBuf::from(v));
    }

    let exe = if cfg!(windows) { "rtun.exe" } else { "rtun" };
    let fallback = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("target")
        .join("debug")
        .join(exe);

    if fallback.exists() {
        return Ok(fallback);
    }

    bail!("CARGO_BIN_EXE_rtun is not set and fallback binary not found");
}

fn dump_process_logs<const N: usize>(procs: [&Proc; N]) -> String {
    let mut out = String::new();
    for proc in procs {
        let stdout = proc.stdout();
        let stderr = proc.stderr();
        out.push_str("\n===== ");
        out.push_str(proc.name.as_str());
        out.push_str(" stdout =====\n");
        out.push_str(stdout.as_str());
        out.push_str("\n===== ");
        out.push_str(proc.name.as_str());
        out.push_str(" stderr =====\n");
        out.push_str(stderr.as_str());
        out.push('\n');
    }
    out
}

fn write_test_cert_pair(dir: &Path) -> Result<(PathBuf, PathBuf)> {
    let mut params = CertificateParams::new(vec!["localhost".to_string()]);
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "localhost");
    params.distinguished_name = dn;
    params
        .subject_alt_names
        .push(SanType::IpAddress("127.0.0.1".parse()?));

    let cert = rcgen::Certificate::from_params(params)?;

    let cert_path = dir.join("test-cert.pem");
    let key_path = dir.join("test-key.pem");

    fs::write(&cert_path, cert.serialize_pem()?).with_context(|| "write cert failed")?;
    fs::write(&key_path, cert.serialize_private_key_pem()).with_context(|| "write key failed")?;

    Ok((cert_path, key_path))
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Result<Self> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .with_context(|| "system clock before unix epoch")?
            .as_nanos();
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rtun-relay-e2e-{pid}-{now}"));
        fs::create_dir_all(&path)
            .with_context(|| format!("create temp dir failed [{:?}]", path))?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        self.path.as_path()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct Proc {
    name: String,
    child: Child,
    stdout_buf: Arc<Mutex<Vec<u8>>>,
    stderr_buf: Arc<Mutex<Vec<u8>>>,
    pumps: Vec<JoinHandle<()>>,
}

impl Proc {
    async fn spawn<I, S>(name: &str, args: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = Command::new(rtun_bin()?);
        cmd.args(args)
            .env("RUST_LOG", "info")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawn rtun process failed [{name}]"))?;

        let stdout = child
            .stdout
            .take()
            .with_context(|| format!("take stdout failed [{name}]"))?;
        let stderr = child
            .stderr
            .take()
            .with_context(|| format!("take stderr failed [{name}]"))?;

        let stdout_buf = Arc::new(Mutex::new(Vec::new()));
        let stderr_buf = Arc::new(Mutex::new(Vec::new()));
        let pumps = vec![
            tokio::spawn(pump_output(stdout, stdout_buf.clone())),
            tokio::spawn(pump_output(stderr, stderr_buf.clone())),
        ];

        Ok(Self {
            name: name.to_string(),
            child,
            stdout_buf,
            stderr_buf,
            pumps,
        })
    }

    fn ensure_running(&mut self) -> Result<()> {
        if let Some(status) = self
            .child
            .try_wait()
            .with_context(|| format!("try_wait failed [{}]", self.name))?
        {
            bail!(
                "process exited [{}], status [{}]\n{}",
                self.name,
                status,
                dump_process_logs([self])
            );
        }
        Ok(())
    }

    fn stdout(&self) -> String {
        let out = self.stdout_buf.lock().unwrap();
        String::from_utf8_lossy(out.as_slice()).into_owned()
    }

    fn stderr(&self) -> String {
        let out = self.stderr_buf.lock().unwrap();
        String::from_utf8_lossy(out.as_slice()).into_owned()
    }

    async fn terminate(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.start_kill();
        }
        let _ = tokio::time::timeout(Duration::from_secs(2), self.child.wait()).await;

        for pump in self.pumps.drain(..) {
            let _ = tokio::time::timeout(Duration::from_secs(1), pump).await;
        }
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.start_kill();
        }

        for pump in self.pumps.drain(..) {
            pump.abort();
        }
    }
}

async fn pump_output<R>(mut reader: R, buf: Arc<Mutex<Vec<u8>>>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let mut chunk = [0_u8; 1024];
    loop {
        let n = match reader.read(&mut chunk).await {
            Ok(0) => return,
            Ok(n) => n,
            Err(_) => return,
        };

        if let Ok(mut out) = buf.lock() {
            out.extend_from_slice(&chunk[..n]);
        }
    }
}
