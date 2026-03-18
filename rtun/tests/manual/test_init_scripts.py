#!/usr/bin/env python3
import fcntl
import os
import pty
import subprocess
import struct
import sys
import tempfile
import termios
import unittest
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))

from init_common import (
    RTUN_LOCAL_HOST,
    NIGHTLY_SHELL_AGENT_DEFAULT,
    RTUN_DEBUG_TMUX_SESSION,
    RTUN_LOCAL_WORKDIR,
    RTUN_NIGHTLY_WORKDIR,
    build_kill_processes_by_workdir_command,
    build_nightly_cleanup_commands,
    collect_archive_members,
    parse_remote_command_result,
    wrap_remote_command_for_marker,
)
from init_nightly import (
    build_remote_append_file_command,
    build_remote_write_file_command,
    chunk_text,
    build_worktree_extract_commands,
    build_worktree_metadata,
    build_rtun_shell_cmd,
    parse_args as parse_nightly_args,
    set_pty_window_size,
)
from init_rtun_local import parse_args as parse_local_args


class ArchiveSelectionTests(unittest.TestCase):
    def test_collect_archive_members_excludes_build_artifacts(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "rtun").mkdir()
            (root / "rtun" / "src.txt").write_text("ok", encoding="utf-8")
            (root / ".git").mkdir()
            (root / ".git" / "config").write_text("x", encoding="utf-8")
            (root / "target").mkdir()
            (root / "target" / "out").write_text("x", encoding="utf-8")
            (root / "__pycache__").mkdir()
            (root / "__pycache__" / "x.pyc").write_text("x", encoding="utf-8")
            (root / "Cargo.lock").write_text("lock", encoding="utf-8")
            (root / ".DS_Store").write_text("x", encoding="utf-8")

            members = {str(v) for v in collect_archive_members(root)}

        self.assertIn("rtun/src.txt", members)
        self.assertNotIn(".git/config", members)
        self.assertNotIn("target/out", members)
        self.assertNotIn("__pycache__/x.pyc", members)
        self.assertIn("Cargo.lock", members)
        self.assertNotIn(".DS_Store", members)


class RemoteCommandSafetyTests(unittest.TestCase):
    def test_kill_command_only_targets_rtun_with_workdir_cwd(self) -> None:
        cmd = build_kill_processes_by_workdir_command("/tmp/rtun-nightly-work")
        self.assertIn("pgrep -x rtun", cmd)
        self.assertIn("/proc/$pid/cwd", cmd)
        self.assertIn("/tmp/rtun-nightly-work", cmd)
        self.assertNotIn("pgrep -f", cmd)
        self.assertNotIn("pkill rtun", cmd)
        self.assertNotIn("killall rtun", cmd)

    def test_nightly_cleanup_commands_scope_to_fixed_resources(self) -> None:
        cmds = build_nightly_cleanup_commands()
        merged = "\n".join(cmds)
        self.assertIn(RTUN_NIGHTLY_WORKDIR, merged)
        self.assertIn(RTUN_DEBUG_TMUX_SESSION, merged)
        self.assertNotIn("pkill rtun", merged)
        self.assertNotIn("killall rtun", merged)


class MarkerProtocolTests(unittest.TestCase):
    def test_wrap_remote_command_with_marker_contains_trailer(self) -> None:
        wrapped = wrap_remote_command_for_marker("echo hi", "abc123")
        self.assertIn("echo hi", wrapped)
        self.assertIn("__RTUN_INIT_DONE__abc123", wrapped)

    def test_parse_remote_command_result_handles_valid_marker(self) -> None:
        marker = "k1"
        line = "__RTUN_INIT_DONE__k1:0"
        rc = parse_remote_command_result(line, marker)
        self.assertEqual(rc, 0)

    def test_parse_remote_command_result_ignores_other_lines(self) -> None:
        self.assertIsNone(parse_remote_command_result("normal output", "k1"))
        self.assertIsNone(parse_remote_command_result("__RTUN_INIT_DONE__k2:0", "k1"))

    def test_parse_remote_command_result_accepts_ansi_prefixed_marker(self) -> None:
        line = "\x1b[?2004l\r__RTUN_INIT_DONE__k1:0"
        self.assertEqual(parse_remote_command_result(line, "k1"), 0)


class ParserTests(unittest.TestCase):
    def test_init_nightly_default_shell_agent(self) -> None:
        args = parse_nightly_args(["--branch", "issue/9-hardnat-protocol"])
        self.assertEqual(args.shell_agent, NIGHTLY_SHELL_AGENT_DEFAULT)
        self.assertEqual(args.branch, "issue/9-hardnat-protocol")
        self.assertEqual(args.source, "git")

    def test_init_nightly_accepts_worktree_source(self) -> None:
        args = parse_nightly_args(
            ["--branch", "issue/9-hardnat-protocol", "--source", "worktree"]
        )
        self.assertEqual(args.source, "worktree")

    def test_init_nightly_branch_is_required(self) -> None:
        with self.assertRaises(SystemExit):
            parse_nightly_args([])

    def test_init_rtun_local_defaults(self) -> None:
        args = parse_local_args([])
        self.assertEqual(vars(args), {})
        self.assertEqual(RTUN_LOCAL_HOST, "rtun-local")
        self.assertEqual(RTUN_LOCAL_WORKDIR, "/tmp/rtun-local-work")

    def test_nightly_shell_cmd_expands_tilde_config_path(self) -> None:
        with mock.patch.dict("os.environ", {"HOME": "/tmp/fake-home"}, clear=False):
            cmd = build_rtun_shell_cmd("nightly-rtun1-manual", "~/simon/bin/config/rtun-full.toml")
        self.assertEqual(cmd[0], "rtun")
        self.assertEqual(cmd[1], "--config")
        self.assertEqual(cmd[2], "/tmp/fake-home/simon/bin/config/rtun-full.toml")
        self.assertNotIn("~/simon/bin/config/rtun-full.toml", cmd)


class PtySizingTests(unittest.TestCase):
    def test_set_pty_window_size_sets_non_zero_terminal_size(self) -> None:
        master_fd, slave_fd = pty.openpty()
        try:
            before = struct.unpack(
                "HHHH", fcntl.ioctl(slave_fd, termios.TIOCGWINSZ, b"\0" * 8)
            )
            self.assertEqual(before[:2], (0, 0))

            set_pty_window_size(slave_fd, cols=80, rows=24)

            after = struct.unpack(
                "HHHH", fcntl.ioctl(slave_fd, termios.TIOCGWINSZ, b"\0" * 8)
            )
            self.assertEqual(after[:2], (24, 80))
        finally:
            os.close(master_fd)
            os.close(slave_fd)


class WorktreeUploadCommandTests(unittest.TestCase):
    def test_chunk_text_splits_without_dropping_bytes(self) -> None:
        chunks = chunk_text("abcdefghij", 4)
        self.assertEqual(chunks, ["abcd", "efgh", "ij"])

    def test_build_remote_append_file_command_uses_single_line_printf(self) -> None:
        command = build_remote_append_file_command(
            "/tmp/rtun-nightly-work.tar.gz.b64",
            "YWJjMTIz",
        )
        self.assertIn("printf '%s'", command)
        self.assertIn("YWJjMTIz", command)
        self.assertIn(">> /tmp/rtun-nightly-work.tar.gz.b64", command)
        self.assertNotIn("<<", command)

    def test_build_remote_write_file_command_uses_single_line_printf_b(self) -> None:
        command = build_remote_write_file_command(
            "/tmp/rtun-nightly-work/.init-source.txt",
            "source=worktree\nbranch=issue/9",
            "__unused__",
        )
        self.assertIn("printf '%b'", command)
        self.assertIn("> /tmp/rtun-nightly-work/.init-source.txt", command)
        self.assertIn("\\n", command)
        self.assertNotIn("<<", command)

    def test_build_worktree_extract_commands_decode_and_cleanup_archive_files(self) -> None:
        cmds = build_worktree_extract_commands(
            "/tmp/rtun-nightly-work",
            "/tmp/rtun-nightly-work.tar.gz",
            "/tmp/rtun-nightly-work.tar.gz.b64",
        )
        merged = "\n".join(cmds)
        self.assertIn("base64 -d /tmp/rtun-nightly-work.tar.gz.b64 > /tmp/rtun-nightly-work.tar.gz", merged)
        self.assertIn("tar -xzf /tmp/rtun-nightly-work.tar.gz -C /tmp/rtun-nightly-work", merged)
        self.assertIn("rm -f /tmp/rtun-nightly-work.tar.gz.b64", merged)
        self.assertIn("rm -f /tmp/rtun-nightly-work.tar.gz", merged)

    def test_build_worktree_metadata_records_branch_head_dirty_and_sha(self) -> None:
        metadata = build_worktree_metadata(
            branch="issue/9-hardnat-protocol",
            local_head="abc123",
            dirty=True,
            archive_sha256="deadbeef",
        )
        self.assertIn("source=worktree", metadata)
        self.assertIn("branch=issue/9-hardnat-protocol", metadata)
        self.assertIn("local_head=abc123", metadata)
        self.assertIn("local_dirty=true", metadata)
        self.assertIn("archive_sha256=deadbeef", metadata)


class RemoteStartIssue9AgentScriptTests(unittest.TestCase):
    def _write_executable(self, path: Path, content: str) -> None:
        path.write_text(content, encoding="utf-8")
        path.chmod(0o755)

    def _run_remote_start_script(self, url: str, secret: str) -> tuple[subprocess.CompletedProcess[str], list[list[str]]]:
        script = HERE / "remote_start_issue9_agent.sh"
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            fakebin = root / "bin"
            fakebin.mkdir()
            tmux_log = root / "tmux.log"

            self._write_executable(
                fakebin / "git",
                """#!/bin/sh
set -eu
target_dir=
for arg in "$@"; do
  target_dir="$arg"
done
mkdir -p "$target_dir"
""",
            )
            self._write_executable(
                fakebin / "cargo",
                """#!/bin/sh
set -eu
mkdir -p target/debug
: > target/debug/rtun
chmod +x target/debug/rtun
""",
            )
            self._write_executable(
                fakebin / "sleep",
                """#!/bin/sh
exit 0
""",
            )
            self._write_executable(
                fakebin / "tmux",
                """#!/bin/sh
set -eu
log_path="$TMUX_LOG"
case "${1:-}" in
  ls)
    printf 'rtun-issue9-agent: 1 windows (created now)\\n'
    exit 0
    ;;
esac
{
  printf 'CALL\\n'
  for arg in "$@"; do
    printf '%s\\n' "$arg"
  done
  printf 'END\\n'
} >> "$log_path"
exit 0
""",
            )

            env = os.environ.copy()
            env["PATH"] = f"{fakebin}:{env.get('PATH', '')}"
            env["TMUX_LOG"] = str(tmux_log)
            env["MY_RTUN1_URL"] = url
            env["MY_RTUN1_SECRET"] = secret

            proc = subprocess.run(
                ["sh", str(script)],
                cwd=str(HERE),
                env=env,
                capture_output=True,
                text=True,
            )
            calls: list[list[str]] = []
            current: list[str] | None = None
            for line in tmux_log.read_text(encoding="utf-8").splitlines():
                if line == "CALL":
                    current = []
                elif line == "END":
                    if current is not None:
                        calls.append(current)
                    current = None
                elif current is not None:
                    current.append(line)
            return proc, calls

    def test_remote_start_issue9_agent_does_not_inline_url_or_secret_into_tmux_command(self) -> None:
        url = "wss://example.test/$HOME/$(printf-pwn)/`tick`"
        secret = "secret-$HOME-$(printf-pwn)-`tick`"

        proc, calls = self._run_remote_start_script(url, secret)

        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)

        agent_cmd_args = [
            arg
            for call in calls
            for arg in call
            if "./target/debug/rtun agent pub" in arg
        ]
        self.assertTrue(agent_cmd_args, calls)
        for arg in agent_cmd_args:
            self.assertIn("$MY_RTUN1_URL", arg)
            self.assertIn("$MY_RTUN1_SECRET", arg)
            self.assertNotIn(url, arg)
            self.assertNotIn(secret, arg)


if __name__ == "__main__":
    unittest.main()
