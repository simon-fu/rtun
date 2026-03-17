#!/usr/bin/env python3
from __future__ import annotations

import argparse
import shlex
import subprocess
import sys
from pathlib import Path

from init_common import (
    RTUN_LOCAL_HOST, RTUN_LOCAL_WORKDIR,
    build_build_command,
    build_kill_processes_by_workdir_command,
    create_source_archive,
    format_step,
)


def build_parser() -> argparse.ArgumentParser:
    return argparse.ArgumentParser(
        description="Initialize rtun-local environment from current local worktree."
    )


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    return build_parser().parse_args(argv)


def run_cmd(args: list[str], prefix: str) -> None:
    print(format_step(prefix, f"run: {' '.join(shlex.quote(v) for v in args)}"), flush=True)
    subprocess.run(args, check=True)


def main(argv: list[str] | None = None) -> int:
    _args = parse_args(argv)
    worktree = Path.cwd()

    print(format_step("init-rtun-local", f"worktree: {worktree}"), flush=True)
    archive_path = create_source_archive(worktree)
    remote_archive = f"{RTUN_LOCAL_WORKDIR}.tar.gz"
    try:
        print(format_step("init-rtun-local", f"archive: {archive_path}"), flush=True)

        remote_cleanup = " && ".join(
            [
                build_kill_processes_by_workdir_command(RTUN_LOCAL_WORKDIR),
                f"rm -rf {shlex.quote(RTUN_LOCAL_WORKDIR)} {shlex.quote(remote_archive)}",
                f"mkdir -p {shlex.quote(RTUN_LOCAL_WORKDIR)}",
            ]
        )
        run_cmd(["ssh", RTUN_LOCAL_HOST, remote_cleanup], "init-rtun-local")

        run_cmd(
            ["scp", str(archive_path), f"{RTUN_LOCAL_HOST}:{remote_archive}"],
            "init-rtun-local",
        )

        remote_extract_and_build = " && ".join(
            [
                f"tar -xzf {shlex.quote(remote_archive)} -C {shlex.quote(RTUN_LOCAL_WORKDIR)}",
                f"rm -f {shlex.quote(remote_archive)}",
                build_build_command(RTUN_LOCAL_WORKDIR),
            ]
        )
        run_cmd(["ssh", RTUN_LOCAL_HOST, remote_extract_and_build], "init-rtun-local")
    finally:
        if archive_path.exists():
            archive_path.unlink()

    print(format_step("init-rtun-local", "done"), flush=True)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except subprocess.CalledProcessError as exc:
        print(
            format_step("init-rtun-local", f"failed: returncode={exc.returncode}"),
            file=sys.stderr,
            flush=True,
        )
        sys.exit(exc.returncode or 1)
