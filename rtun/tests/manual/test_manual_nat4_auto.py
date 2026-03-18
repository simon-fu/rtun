#!/usr/bin/env python3
import sys
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))

from run_manual_nat4_auto import (
    NAT4_PREFERRED_IP,
    build_environment_reuse_warnings,
    build_parser,
    build_result_summary,
    detect_manual_connect_success,
    parse_candidate_ips_from_dump,
    parse_nat3_public_addr,
    prioritize_candidate_ips,
)


class ParserTests(unittest.TestCase):
    def test_defaults(self) -> None:
        args = build_parser().parse_args([])
        self.assertEqual(args.rtun_local_host, "rtun-local")
        self.assertEqual(args.shell_agent, "nightly-rtun1-manual")
        self.assertFalse(args.init_rtun_local)
        self.assertFalse(args.init_nightly)
        self.assertEqual(args.nightly_source, "git")
        self.assertEqual(args.batches_per_ip, 4)
        self.assertFalse(hasattr(args, "rtun_local_workdir"))
        self.assertFalse(hasattr(args, "nightly_workdir"))

    def test_override_rtun_local_and_shell_agent(self) -> None:
        args = build_parser().parse_args(
            [
                "--rtun-local-host",
                "rtun-local-alt",
                "--shell-agent",
                "nightly-foo",
                "--init-rtun-local",
                "--init-nightly",
                "--nightly-source",
                "worktree",
                "--batches-per-ip",
                "6",
            ]
        )
        self.assertEqual(args.rtun_local_host, "rtun-local-alt")
        self.assertEqual(args.shell_agent, "nightly-foo")
        self.assertTrue(args.init_rtun_local)
        self.assertTrue(args.init_nightly)
        self.assertEqual(args.nightly_source, "worktree")
        self.assertEqual(args.batches_per_ip, 6)

    def test_removed_workdir_args_are_rejected(self) -> None:
        with self.assertRaises(SystemExit):
            build_parser().parse_args(["--rtun-local-workdir", "/tmp/x"])
        with self.assertRaises(SystemExit):
            build_parser().parse_args(["--nightly-workdir", "/tmp/y"])


class CandidateOrderingTests(unittest.TestCase):
    def test_preferred_ip_moves_to_front_when_present(self) -> None:
        ips = ["1.1.1.1", NAT4_PREFERRED_IP, "2.2.2.2", NAT4_PREFERRED_IP]
        self.assertEqual(
            prioritize_candidate_ips(ips),
            [NAT4_PREFERRED_IP, "1.1.1.1", "2.2.2.2"],
        )

    def test_order_preserved_when_preferred_absent(self) -> None:
        ips = ["3.3.3.3", "2.2.2.2", "3.3.3.3", "1.1.1.1"]
        self.assertEqual(
            prioritize_candidate_ips(ips),
            ["3.3.3.3", "2.2.2.2", "1.1.1.1"],
        )


class ParsingTests(unittest.TestCase):
    def test_parse_candidate_ips_from_dump(self) -> None:
        text = (
            "xx candidate_ips [\"60.194.12.167\", \"101.39.212.231\", \"101.40.161.229\"] yy"
        )
        self.assertEqual(
            parse_candidate_ips_from_dump(text),
            ["60.194.12.167", "101.39.212.231", "101.40.161.229"],
        )

    def test_parse_nat3_public_addr_prefers_recommended(self) -> None:
        text = """
        nat3 public address discovery local [0.0.0.0:1] => mapped [20.168.109.87:34875]
        recommended peer command: rtun nat4 nat4 -t 20.168.109.87:34875
        """
        self.assertEqual(parse_nat3_public_addr(text), "20.168.109.87:34875")

    def test_parse_nat3_public_addr_fallback_to_mapped(self) -> None:
        text = "nat3 public address discovery local [0.0.0.0:1] => mapped [20.168.109.87:34876]"
        self.assertEqual(parse_nat3_public_addr(text), "20.168.109.87:34876")


class SuccessAndSummaryTests(unittest.TestCase):
    def test_detect_manual_connect_success(self) -> None:
        nat3_log = "2026 line: connected from target [1.2.3.4:5]"
        nat4_log = "2026 line: manual converge final connected selected: owner [1]"
        self.assertTrue(detect_manual_connect_success(nat3_log, nat4_log))
        self.assertFalse(detect_manual_connect_success("", nat4_log))
        self.assertFalse(detect_manual_connect_success(nat3_log, ""))
        self.assertFalse(
            detect_manual_connect_success(
                "connected from target should be printed here later",
                nat4_log,
            )
        )
        self.assertFalse(
            detect_manual_connect_success(
                nat3_log,
                "manual converge final connected selected should appear later",
            )
        )

    def test_build_result_summary_contains_required_fields(self) -> None:
        summary = build_result_summary(
            nat4_candidate_ip="60.194.12.167",
            nat3_public_addr="20.168.109.87:34875",
            batches=4,
            nat3_log="connected from target [x]",
            nat4_log=(
                "manual converge probe hit: socket [1]\n"
                "promoted recv socket ttl [0.0.0.0:1] => [64]\n"
                "manual converge final connected selected: owner [1]"
            ),
        )
        self.assertEqual(summary["nat4_candidate_ip"], "60.194.12.167")
        self.assertEqual(summary["nat3_public_addr"], "20.168.109.87:34875")
        self.assertEqual(summary["nat3_batches"], 4)
        self.assertEqual(summary["nat4_probe_hit_socket_count"], 1)
        self.assertTrue(summary["nat4_ttl_promoted"])
        self.assertTrue(summary["nat3_connected_from_target"])
        self.assertTrue(summary["nat4_final_connected_selected"])
        self.assertIn("success", summary)


class WarningTests(unittest.TestCase):
    def test_build_environment_reuse_warnings_for_both_uninitialized(self) -> None:
        warnings = build_environment_reuse_warnings(
            init_rtun_local=False,
            init_nightly=False,
        )
        self.assertEqual(len(warnings), 2)
        self.assertIn("rtun-local", warnings[0])
        self.assertIn("nightly", warnings[1])
        self.assertTrue(all("可能不是最新" in item for item in warnings))

    def test_build_environment_reuse_warnings_for_single_side(self) -> None:
        self.assertEqual(
            build_environment_reuse_warnings(
                init_rtun_local=True,
                init_nightly=True,
            ),
            [],
        )
        warnings = build_environment_reuse_warnings(
            init_rtun_local=False,
            init_nightly=True,
        )
        self.assertEqual(len(warnings), 1)
        self.assertIn("rtun-local", warnings[0])


if __name__ == "__main__":
    unittest.main()
