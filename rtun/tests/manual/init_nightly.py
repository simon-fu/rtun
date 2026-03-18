#!/usr/bin/env python3
from __future__ import annotations

import argparse
import base64
import fcntl
import hashlib
import os
import pty
import select
import shlex
import shutil
import struct
import sys
import termios
import time
import uuid
from pathlib import Path
import subprocess

from init_common import (
    create_source_archive,
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

NIGHTLY_SOURCE_GIT = "git"
NIGHTLY_SOURCE_WORKTREE = "worktree"
NIGHTLY_WORKTREE_ARCHIVE_BASE64_PATH = f"{RTUN_NIGHTLY_WORKDIR}.tar.gz.b64"
NIGHTLY_WORKTREE_ARCHIVE_PATH = f"{RTUN_NIGHTLY_WORKDIR}.tar.gz"
NIGHTLY_WORKTREE_METADATA_PATH = f"{RTUN_NIGHTLY_WORKDIR}/.init-source.txt"
NIGHTLY_UPLOAD_TIMEOUT_SEC = 1800.0
# Interactive PTYs often impose ~4 KiB canonical line limits.
# Keep each append command comfortably below that ceiling.
NIGHTLY_UPLOAD_CHUNK_SIZE = 1024


class PtyShellRunner:
    def __init__(self, argv: list[str]) -> None:
        self.argv = argv
        self.master_fd: int | None = None
        self.proc: subprocess.Popen[bytes] | None = None
        self._line_buf = ""

    def start(self) -> None:
        master_fd, slave_fd = pty.openpty()
        cols, rows = get_local_terminal_size()
        set_pty_window_size(slave_fd, cols, rows)
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
        sent = 0
        while sent < len(data):
            sent += os.write(self.master_fd, data[sent:])

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


def get_local_terminal_size() -> tuple[int, int]:
    size = shutil.get_terminal_size(fallback=(80, 24))
    return size.columns, size.lines


def set_pty_window_size(fd: int, cols: int, rows: int) -> None:
    winsz = struct.pack("HHHH", rows, cols, 0, 0)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, winsz)


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
    parser.add_argument(
        "--source",
        choices=[NIGHTLY_SOURCE_GIT, NIGHTLY_SOURCE_WORKTREE],
        default=NIGHTLY_SOURCE_GIT,
        help=f"code source for nightly init (default: {NIGHTLY_SOURCE_GIT})",
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


def build_remote_write_file_command(remote_path: str, content: str, heredoc_tag: str) -> str:
    _ = heredoc_tag
    escaped = (
        content.replace("\\", "\\\\")
        .replace("\n", "\\n")
        .replace("\r", "\\r")
        .replace("\t", "\\t")
    )
    return f"printf '%b' {shlex.quote(escaped)} > {shlex.quote(remote_path)}"


def build_remote_append_file_command(remote_path: str, content: str) -> str:
    return f"printf '%s' {shlex.quote(content)} >> {shlex.quote(remote_path)}"


def build_worktree_extract_commands(
    workdir: str,
    archive_path: str,
    archive_base64_path: str,
) -> list[str]:
    q_workdir = shlex.quote(workdir)
    q_archive = shlex.quote(archive_path)
    q_archive_b64 = shlex.quote(archive_base64_path)
    return [
        f"base64 -d {q_archive_b64} > {q_archive}",
        f"rm -f {q_archive_b64}",
        f"mkdir -p {q_workdir}",
        f"tar -xzf {q_archive} -C {q_workdir}",
        f"rm -f {q_archive}",
    ]


def build_worktree_metadata(
    branch: str,
    local_head: str,
    dirty: bool,
    archive_sha256: str,
) -> str:
    return "\n".join(
        [
            "source=worktree",
            f"branch={branch}",
            f"local_head={local_head}",
            f"local_dirty={'true' if dirty else 'false'}",
            f"archive_sha256={archive_sha256}",
        ]
    )


def compute_sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as fh:
        for chunk in iter(lambda: fh.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def encode_base64_text(path: Path) -> str:
    return base64.b64encode(path.read_bytes()).decode("ascii")


def chunk_text(text: str, chunk_size: int) -> list[str]:
    if chunk_size <= 0:
        raise ValueError("chunk_size must be positive")
    return [text[i : i + chunk_size] for i in range(0, len(text), chunk_size)]


def git_stdout(worktree: Path, args: list[str]) -> str:
    proc = subprocess.run(
        ["git", *args],
        cwd=str(worktree),
        check=True,
        capture_output=True,
        text=True,
    )
    return proc.stdout.strip()


def read_local_head(worktree: Path) -> str:
    return git_stdout(worktree, ["rev-parse", "HEAD"])


def is_local_worktree_dirty(worktree: Path) -> bool:
    return bool(git_stdout(worktree, ["status", "--short"]))


def run_worktree_init(
    runner: PtyShellRunner,
    branch: str,
    worktree: Path,
) -> None:
    print(format_step("init-nightly", f"worktree: {worktree}"), flush=True)
    archive_path = create_source_archive(worktree)
    try:
        archive_sha256 = compute_sha256(archive_path)
        archive_base64 = encode_base64_text(archive_path)
        metadata = build_worktree_metadata(
            branch=branch,
            local_head=read_local_head(worktree),
            dirty=is_local_worktree_dirty(worktree),
            archive_sha256=archive_sha256,
        )
        archive_chunks = chunk_text(archive_base64, NIGHTLY_UPLOAD_CHUNK_SIZE)
        extract_cmds = build_worktree_extract_commands(
            RTUN_NIGHTLY_WORKDIR,
            NIGHTLY_WORKTREE_ARCHIVE_PATH,
            NIGHTLY_WORKTREE_ARCHIVE_BASE64_PATH,
        )
        step_total = len(build_nightly_cleanup_commands()) + 1 + len(extract_cmds) + 2
        step_idx = 0

        for cmd in build_nightly_cleanup_commands():
            step_idx += 1
            print(
                format_step("init-nightly", f"step {step_idx}/{step_total}: {cmd}"),
                flush=True,
            )
            runner.run_checked(cmd, timeout_sec=300.0)

        step_idx += 1
        print(
            format_step(
                "init-nightly",
                f"step {step_idx}/{step_total}: upload worktree archive to {NIGHTLY_WORKTREE_ARCHIVE_BASE64_PATH}",
            ),
            flush=True,
        )
        restore_exc: Exception | None = None
        runner.run_checked("stty -echo", timeout_sec=10.0)
        try:
            runner.run_checked(
                f": > {shlex.quote(NIGHTLY_WORKTREE_ARCHIVE_BASE64_PATH)}",
                timeout_sec=10.0,
            )
            for idx, chunk in enumerate(archive_chunks, start=1):
                if idx == 1 or idx == len(archive_chunks) or idx % 50 == 0:
                    print(
                        format_step(
                            "init-nightly",
                            f"upload chunk {idx}/{len(archive_chunks)}",
                        ),
                        flush=True,
                    )
                runner.run_checked(
                    build_remote_append_file_command(
                        NIGHTLY_WORKTREE_ARCHIVE_BASE64_PATH,
                        chunk,
                    ),
                    timeout_sec=30.0,
                )
        finally:
            try:
                runner.run_checked("stty echo", timeout_sec=10.0)
            except Exception as exc:
                restore_exc = exc
        if restore_exc is not None:
            raise restore_exc

        for cmd in extract_cmds:
            step_idx += 1
            print(
                format_step("init-nightly", f"step {step_idx}/{step_total}: {cmd}"),
                flush=True,
            )
            runner.run_checked(cmd, timeout_sec=300.0)

        step_idx += 1
        print(
            format_step(
                "init-nightly",
                f"step {step_idx}/{step_total}: write worktree metadata to {NIGHTLY_WORKTREE_METADATA_PATH}",
            ),
            flush=True,
        )
        runner.run_checked(
            build_remote_write_file_command(
                NIGHTLY_WORKTREE_METADATA_PATH,
                metadata,
                "__unused__",
            ),
            timeout_sec=60.0,
        )

        step_idx += 1
        build_cmd = build_build_command(RTUN_NIGHTLY_WORKDIR)
        print(
            format_step("init-nightly", f"step {step_idx}/{step_total}: {build_cmd}"),
            flush=True,
        )
        runner.run_checked(build_cmd, timeout_sec=NIGHTLY_UPLOAD_TIMEOUT_SEC)
    finally:
        if archive_path.exists():
            archive_path.unlink()


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    worktree = Path.cwd()
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
        if args.source == NIGHTLY_SOURCE_WORKTREE:
            run_worktree_init(runner, args.branch, worktree)
        else:
            cmds = []
            cmds.extend(build_nightly_cleanup_commands())
            cmds.extend(build_nightly_git_commands(args.branch))
            cmds.append(build_build_command(RTUN_NIGHTLY_WORKDIR))

            for idx, cmd in enumerate(cmds, start=1):
                print(
                    format_step("init-nightly", f"step {idx}/{len(cmds)}: {cmd}"),
                    flush=True,
                )
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
