#!/usr/bin/env python3
from __future__ import annotations

import argparse
import ast
import json
import os
import re
import select
import shlex
import subprocess
import sys
import time
import uuid
from pathlib import Path
from typing import Any

from init_common import (
    NIGHTLY_SHELL_AGENT_DEFAULT,
    RTUN_LOCAL_HOST,
    RTUN_LOCAL_WORKDIR,
    RTUN_NIGHTLY_WORKDIR,
    build_kill_processes_by_workdir_command,
    create_source_archive,
    format_step,
)
from init_nightly import (
    PtyShellRunner,
    build_rtun_shell_cmd,
    parse_remote_command_result,
    wrap_remote_command_for_marker,
)

NAT4_PREFERRED_IP = "101.40.161.229"

REMOTE_NAT3_LOG = f"{RTUN_NIGHTLY_WORKDIR}/nat3-manual.log"
LOCAL_NAT4_LOG = f"{RTUN_LOCAL_WORKDIR}/nat4-manual.log"


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Run manual NAT4 probing automatically across nat4 candidate IPs."
    )
    parser.add_argument("--rtun-local-host", default=RTUN_LOCAL_HOST)
    parser.add_argument("--shell-agent", default=NIGHTLY_SHELL_AGENT_DEFAULT)
    parser.add_argument("--branch", default="issue/9-hardnat-protocol")
    parser.add_argument("--nightly-source", choices=["git", "worktree"], default="git")
    parser.add_argument("--init-rtun-local", action="store_true")
    parser.add_argument("--init-nightly", action="store_true")
    parser.add_argument("--batches-per-ip", type=int, default=4)
    parser.add_argument("--summary-json", default="")
    return parser


def run_local_cmd(args: list[str], timeout: float = 60.0, check: bool = True) -> subprocess.CompletedProcess[str]:
    proc = subprocess.run(args, capture_output=True, text=True, timeout=timeout)
    if check and proc.returncode != 0:
        raise RuntimeError(
            f"command failed rc={proc.returncode}: {args}\nstdout={proc.stdout}\nstderr={proc.stderr}"
        )
    return proc


def ssh_cmd(host: str, command: str, timeout: float = 60.0, check: bool = True) -> subprocess.CompletedProcess[str]:
    return run_local_cmd(["ssh", host, command], timeout=timeout, check=check)


def parse_candidate_ips_from_dump(text: str) -> list[str]:
    m = re.search(r"candidate_ips \[(.*?)\]", text, re.S)
    if not m:
        return []
    list_text = "[" + m.group(1) + "]"
    try:
        values = ast.literal_eval(list_text)
    except (ValueError, SyntaxError):
        return []
    out: list[str] = []
    for value in values:
        if isinstance(value, str) and value and value not in out:
            out.append(value)
    return out


def prioritize_candidate_ips(ips: list[str], preferred_ip: str = NAT4_PREFERRED_IP) -> list[str]:
    out: list[str] = []
    seen: set[str] = set()
    if preferred_ip in ips:
        out.append(preferred_ip)
        seen.add(preferred_ip)
    for ip in ips:
        if ip in seen:
            continue
        seen.add(ip)
        out.append(ip)
    return out


def parse_nat3_public_addr(text: str) -> str | None:
    m = re.search(r"recommended peer command: rtun nat4 nat4 -t ([0-9.]+:\d+)", text)
    if m:
        return m.group(1)
    m = re.search(r"nat3 public address discovery local \[[^\]]+\] => mapped \[([0-9.]+:\d+)\]", text)
    return m.group(1) if m else None


def build_environment_reuse_warnings(
    *,
    init_rtun_local: bool,
    init_nightly: bool,
) -> list[str]:
    warnings: list[str] = []
    if not init_rtun_local:
        warnings.append(
            "当前复用现有 rtun-local 环境，二进制/代码版本可能不是最新，请注意环境状态可能已陈旧"
        )
    if not init_nightly:
        warnings.append(
            "当前复用现有 nightly 环境，二进制/代码版本可能不是最新，请注意环境状态可能已陈旧"
        )
    return warnings


def detect_manual_connect_success(nat3_log: str, nat4_log: str) -> bool:
    nat3_connected = re.search(r"(^|\n).*connected from target \[[^\]]+\]", nat3_log) is not None
    nat4_connected = (
        re.search(r"(^|\n).*manual converge final connected selected: owner \[\d+\]", nat4_log)
        is not None
    )
    return nat3_connected and nat4_connected


def build_result_summary(
    *,
    nat4_candidate_ip: str,
    nat3_public_addr: str | None,
    batches: int,
    nat3_log: str,
    nat4_log: str,
) -> dict[str, Any]:
    probe_hit_sockets = sorted(set(re.findall(r"manual converge probe hit: socket \[(\d+)\]", nat4_log)))
    ttl_promote_count = len(re.findall(r"promoted recv socket ttl", nat4_log))
    summary = {
        "nat4_candidate_ip": nat4_candidate_ip,
        "nat3_public_addr": nat3_public_addr or "",
        "nat3_batches": batches,
        "nat4_probe_hit_socket_count": len(probe_hit_sockets),
        "nat4_probe_hit_sockets": probe_hit_sockets,
        "nat4_ttl_promoted": ttl_promote_count > 0,
        "nat4_ttl_promote_count": ttl_promote_count,
        "nat3_connected_from_target": "connected from target" in nat3_log,
        "nat4_final_connected_selected": "final connected selected" in nat4_log,
    }
    summary["success"] = detect_manual_connect_success(nat3_log, nat4_log)
    return summary


def run_remote_capture(runner: PtyShellRunner, command: str, timeout_sec: float = 60.0) -> tuple[int, str]:
    marker = uuid.uuid4().hex
    wrapped = wrap_remote_command_for_marker(command, marker)
    runner.send_command(wrapped)
    deadline = time.monotonic() + timeout_sec
    buf = ""
    lines: list[str] = []
    while time.monotonic() < deadline:
        remaining = max(0.0, deadline - time.monotonic())
        wait = min(0.5, remaining)
        ready, _, _ = select.select([runner.master_fd], [], [], wait)
        if not ready:
            continue
        chunk = os.read(runner.master_fd, 4096)
        if not chunk:
            raise RuntimeError("pty closed before remote command completed")
        text = chunk.decode("utf-8", errors="replace")
        buf += text
        while "\n" in buf:
            line, buf = buf.split("\n", 1)
            line = line.rstrip("\r")
            rc = parse_remote_command_result(line, marker)
            if rc is not None:
                return rc, "\n".join(lines)
            lines.append(line)
    raise TimeoutError(f"remote command timeout: {command}")


def remote_checked(runner: PtyShellRunner, command: str, timeout_sec: float = 60.0) -> str:
    rc, out = run_remote_capture(runner, command, timeout_sec=timeout_sec)
    if rc != 0:
        raise RuntimeError(f"remote command failed rc={rc}: {command}\nout={out}")
    return out


def init_rtun_local_for_host(host: str, worktree: Path) -> None:
    print(format_step("manual-auto", f"init rtun-local host [{host}]"), flush=True)
    archive_path = create_source_archive(worktree)
    remote_archive = f"{RTUN_LOCAL_WORKDIR}.tar.gz"
    try:
        cleanup_cmd = " && ".join(
            [
                build_kill_processes_by_workdir_command(RTUN_LOCAL_WORKDIR),
                f"rm -rf {shlex.quote(RTUN_LOCAL_WORKDIR)} {shlex.quote(remote_archive)}",
                f"mkdir -p {shlex.quote(RTUN_LOCAL_WORKDIR)}",
            ]
        )
        ssh_cmd(host, cleanup_cmd, timeout=120)
        run_local_cmd(["scp", str(archive_path), f"{host}:{remote_archive}"], timeout=120)
        extract_build = " && ".join(
            [
                f"tar -xzf {shlex.quote(remote_archive)} -C {shlex.quote(RTUN_LOCAL_WORKDIR)}",
                f"rm -f {shlex.quote(remote_archive)}",
                f"cd {shlex.quote(RTUN_LOCAL_WORKDIR)} && cargo build -p rtun --bin rtun",
            ]
        )
        ssh_cmd(host, extract_build, timeout=1800)
    finally:
        if archive_path.exists():
            archive_path.unlink()


def init_nightly(shell_agent: str, branch: str, source: str, worktree: Path) -> None:
    print(
        format_step(
            "manual-auto",
            f"init nightly via shell-agent [{shell_agent}] branch [{branch}] source [{source}]",
        ),
        flush=True,
    )
    cmd = [
        "python3",
        "rtun/tests/manual/init_nightly.py",
        "--branch",
        branch,
        "--shell-agent",
        shell_agent,
        "--source",
        source,
    ]
    run_local_cmd(cmd, timeout=2400)


def sample_nat4_candidate_ips(rtun_local_host: str) -> list[str]:
    command = (
        f"cd {shlex.quote(RTUN_LOCAL_WORKDIR)} && timeout 18s env RUST_LOG=debug stdbuf -oL -eL "
        "target/debug/rtun nat4 nat4 -t 1.1.1.1:9 -c 128 --dump-public-addrs 2>&1"
    )
    proc = ssh_cmd(rtun_local_host, command, timeout=40, check=False)
    dump = (proc.stdout or "") + (proc.stderr or "")
    return parse_candidate_ips_from_dump(dump)


def cleanup_before_trial(runner: PtyShellRunner, rtun_local_host: str) -> None:
    for cmd in [
        "tmux kill-session -t rtun-debug-agent 2>/dev/null || true",
        "tmux kill-session -t rtun-tcpdump 2>/dev/null || true",
        build_kill_processes_by_workdir_command(RTUN_NIGHTLY_WORKDIR),
        f"rm -f {shlex.quote(REMOTE_NAT3_LOG)}",
    ]:
        remote_checked(runner, cmd, timeout_sec=60.0)

    local_cleanup = " && ".join(
        [
            "tmux kill-session -t rtun-manual-nat4 2>/dev/null || true",
            "tmux kill-session -t rtun-manual-tcpdump 2>/dev/null || true",
            build_kill_processes_by_workdir_command(RTUN_LOCAL_WORKDIR),
            f"rm -f {shlex.quote(LOCAL_NAT4_LOG)}",
        ]
    )
    ssh_cmd(rtun_local_host, local_cleanup, timeout=60)


def wait_nat3_discovery_ready(runner: PtyShellRunner, timeout_sec: float = 25.0) -> str:
    deadline = time.monotonic() + timeout_sec
    last_out = ""
    while time.monotonic() < deadline:
        rc, out = run_remote_capture(
            runner,
            "tmux capture-pane -pJ -t rtun-debug-agent -S -200 | tail -n 120",
            timeout_sec=10.0,
        )
        if rc == 0:
            last_out = out
            if "nat3 discovery finished, press Enter to start probing" in out:
                return out
        time.sleep(1.0)
    return last_out


def fetch_remote_file(runner: PtyShellRunner, path: str) -> str:
    rc, out = run_remote_capture(
        runner,
        f"test -f {shlex.quote(path)} && cat {shlex.quote(path)} || true",
        timeout_sec=30.0,
    )
    return out if rc == 0 else ""


def fetch_local_file(rtun_local_host: str, path: str) -> str:
    proc = ssh_cmd(
        rtun_local_host,
        f"test -f {shlex.quote(path)} && cat {shlex.quote(path)} || true",
        timeout=30,
        check=False,
    )
    return proc.stdout or ""


def run_manual_nat4_auto(args: argparse.Namespace) -> list[dict[str, Any]]:
    worktree = Path.cwd()

    for warning in build_environment_reuse_warnings(
        init_rtun_local=args.init_rtun_local,
        init_nightly=args.init_nightly,
    ):
        print(format_step("manual-auto", f"warning: {warning}"), flush=True)

    if args.init_rtun_local:
        init_rtun_local_for_host(args.rtun_local_host, worktree)
    if args.init_nightly:
        init_nightly(args.shell_agent, args.branch, args.nightly_source, worktree)

    candidate_ips = sample_nat4_candidate_ips(args.rtun_local_host)
    if not candidate_ips:
        raise RuntimeError("sample nat4 candidate ips failed: empty candidate list")
    ordered_ips = prioritize_candidate_ips(candidate_ips)
    print(format_step("manual-auto", f"candidate_ips={ordered_ips}"), flush=True)

    runner = PtyShellRunner(build_rtun_shell_cmd(args.shell_agent))
    runner.start()
    results: list[dict[str, Any]] = []
    try:
        for index, nat4_candidate_ip in enumerate(ordered_ips, start=1):
            print(
                format_step("manual-auto", f"trial {index}/{len(ordered_ips)} ip [{nat4_candidate_ip}]"),
                flush=True,
            )
            cleanup_before_trial(runner, args.rtun_local_host)

            start_nat3_cmd = (
                f"cd {shlex.quote(RTUN_NIGHTLY_WORKDIR)} && "
                "tmux new-session -d -s rtun-debug-agent "
                + shlex.quote(
                    "timeout 180s env RUST_LOG=debug stdbuf -oL -eL target/debug/rtun nat4 nat3 "
                    f"-t {nat4_candidate_ip} -c 512 --interval 500 --discover-public-addr "
                    "--pause-after-discovery --hold-batch-until-enter --debug-converge-lease"
                )
            )
            remote_checked(runner, start_nat3_cmd, timeout_sec=30.0)
            remote_checked(
                runner,
                f"tmux pipe-pane -o -t rtun-debug-agent 'cat >> {REMOTE_NAT3_LOG}'",
                timeout_sec=10.0,
            )

            discovery_text = wait_nat3_discovery_ready(runner)
            nat3_public_addr = parse_nat3_public_addr(discovery_text)
            if not nat3_public_addr:
                summary = build_result_summary(
                    nat4_candidate_ip=nat4_candidate_ip,
                    nat3_public_addr=None,
                    batches=0,
                    nat3_log=discovery_text,
                    nat4_log="",
                )
                summary["error"] = "missing nat3 public addr in discovery output"
                results.append(summary)
                continue

            start_nat4_cmd = (
                f"cd {shlex.quote(RTUN_LOCAL_WORKDIR)} && "
                "tmux new-session -d -s rtun-manual-nat4 "
                + shlex.quote(
                    "timeout 180s env RUST_LOG=debug stdbuf -oL -eL target/debug/rtun nat4 nat4 "
                    f"-t {nat3_public_addr} -c 512 --interval 500 --debug-keep-recv "
                    "--debug-promote-hit-ttl 64 --debug-converge-lease "
                    f"> {LOCAL_NAT4_LOG} 2>&1"
                )
            )
            ssh_cmd(args.rtun_local_host, start_nat4_cmd, timeout=20)

            for _ in range(args.batches_per_ip):
                remote_checked(runner, "tmux send-keys -t rtun-debug-agent C-m", timeout_sec=10.0)
                time.sleep(2.5)
            time.sleep(5.0)

            nat3_log = fetch_remote_file(runner, REMOTE_NAT3_LOG)
            nat4_log = fetch_local_file(args.rtun_local_host, LOCAL_NAT4_LOG)
            summary = build_result_summary(
                nat4_candidate_ip=nat4_candidate_ip,
                nat3_public_addr=nat3_public_addr,
                batches=args.batches_per_ip,
                nat3_log=nat3_log,
                nat4_log=nat4_log,
            )
            results.append(summary)
            if summary["success"]:
                break
    finally:
        runner.close()

    return results


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    results = run_manual_nat4_auto(args)
    payload = json.dumps(results, ensure_ascii=False, indent=2)
    print(payload, flush=True)

    if args.summary_json:
        path = Path(args.summary_json)
        path.write_text(payload, encoding="utf-8")
        print(format_step("manual-auto", f"summary written to {path}"), flush=True)

    return 0 if any(x.get("success") for x in results) else 1


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (subprocess.CalledProcessError, RuntimeError, TimeoutError) as exc:
        print(format_step("manual-auto", f"failed: {exc}"), file=sys.stderr, flush=True)
        sys.exit(1)
