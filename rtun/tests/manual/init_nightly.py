#!/usr/bin/env python3
from __future__ import annotations

import argparse
import os
import pty
import select
import shlex
import sys
import time
import uuid
from pathlib import Path
import subprocess

from init_common import (
    NIGHTLY_SHELL_AGENT_DEFAULT,
    RTUN_CONFIG_PATH,
    RTUN_NIGHTLY_WORKDIR,
    build_build_command,
    build_nightly_cleanup_commands,
    build_nightly_git_commands,
    format_step,
    parse_remote_command_result,
    wrap_remote_command_for_marker,
)


class PtyShellRunner:
    def __init__(self, argv: list[str]) -> None:
        self.argv = argv
        self.master_fd: int | None = None
        self.proc: subprocess.Popen[bytes] | None = None
        self._line_buf = ""

    def start(self) -> None:
        master_fd, slave_fd = pty.openpty()
        self.proc = subprocess.Popen(
            self.argv,
            stdin=slave_fd,
            stdout=slave_fd,
            stderr=slave_fd,
            close_fds=True,
        )
        os.close(slave_fd)
        self.master_fd = master_fd

    def close(self) -> None:
        if self.proc is not None and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.proc.kill()
        if self.master_fd is not None:
            os.close(self.master_fd)
            self.master_fd = None

    def send_command(self, command: str) -> None:
        if self.master_fd is None:
            raise RuntimeError("pty session is not started")
        data = (command + "\n").encode("utf-8")
        os.write(self.master_fd, data)

    def run_checked(self, command: str, timeout_sec: float = 300.0) -> None:
        marker = uuid.uuid4().hex
        wrapped = wrap_remote_command_for_marker(command, marker)
        self.send_command(wrapped)
        rc = self.wait_marker(marker, timeout_sec)
        if rc != 0:
            raise RuntimeError(f"remote command failed rc={rc}: {command}")

    def wait_marker(self, marker: str, timeout_sec: float) -> int:
        if self.master_fd is None:
            raise RuntimeError("pty session is not started")
        deadline = time.monotonic() + timeout_sec
        while time.monotonic() < deadline:
            remaining = max(0.0, deadline - time.monotonic())
            wait = min(0.5, remaining)
            ready, _, _ = select.select([self.master_fd], [], [], wait)
            if not ready:
                continue
            chunk = os.read(self.master_fd, 4096)
            if not chunk:
                raise RuntimeError("pty closed before command completed")
            text = chunk.decode("utf-8", errors="replace")
            print(text, end="", flush=True)
            self._line_buf += text
            while "\n" in self._line_buf:
                line, self._line_buf = self._line_buf.split("\n", 1)
                rc = parse_remote_command_result(line.rstrip("\r"), marker)
                if rc is not None:
                    return rc
        raise TimeoutError(f"wait remote marker timeout: {marker}")


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Initialize nightly remote environment via rtun shell."
    )
    parser.add_argument(
        "--branch",
        required=True,
        help="git branch to checkout on nightly",
    )
    parser.add_argument(
        "--shell-agent",
        default=NIGHTLY_SHELL_AGENT_DEFAULT,
        help=f"agent for rtun shell (default: {NIGHTLY_SHELL_AGENT_DEFAULT})",
    )
    return parser


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    return build_parser().parse_args(argv)


def build_rtun_shell_cmd(shell_agent: str, config_path: str = RTUN_CONFIG_PATH) -> list[str]:
    expanded_config_path = os.path.expanduser(config_path)
    return [
        "rtun",
        "--config",
        expanded_config_path,
        "shell",
        "--agent",
        shell_agent,
    ]


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    rtun_shell_cmd = build_rtun_shell_cmd(args.shell_agent)
    print(
        format_step(
            "init-nightly",
            f"connect shell agent [{args.shell_agent}] via {' '.join(shlex.quote(v) for v in rtun_shell_cmd)}",
        ),
        flush=True,
    )
    runner = PtyShellRunner(rtun_shell_cmd)
    runner.start()
    try:
        cmds = []
        cmds.extend(build_nightly_cleanup_commands())
        cmds.extend(build_nightly_git_commands(args.branch))
        cmds.append(build_build_command(RTUN_NIGHTLY_WORKDIR))

        for idx, cmd in enumerate(cmds, start=1):
            print(format_step("init-nightly", f"step {idx}/{len(cmds)}: {cmd}"), flush=True)
            runner.run_checked(cmd)
    finally:
        runner.close()

    print(format_step("init-nightly", "done"), flush=True)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (subprocess.CalledProcessError, RuntimeError, TimeoutError) as exc:
        print(format_step("init-nightly", f"failed: {exc}"), file=sys.stderr, flush=True)
        sys.exit(1)
