#!/usr/bin/env python3
import sys
import tempfile
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
from init_nightly import build_rtun_shell_cmd, parse_args as parse_nightly_args
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


class ParserTests(unittest.TestCase):
    def test_init_nightly_default_shell_agent(self) -> None:
        args = parse_nightly_args(["--branch", "issue/9-hardnat-protocol"])
        self.assertEqual(args.shell_agent, NIGHTLY_SHELL_AGENT_DEFAULT)
        self.assertEqual(args.branch, "issue/9-hardnat-protocol")

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


if __name__ == "__main__":
    unittest.main()
