#!/usr/bin/env python3
from __future__ import annotations

import os
import re
import shlex
import tarfile
import tempfile
from pathlib import Path
from typing import Optional

RTUN_LOCAL_HOST = "rtun-local"
RTUN_LOCAL_WORKDIR = "/tmp/rtun-local-work"
RTUN_NIGHTLY_WORKDIR = "/tmp/rtun-nightly-work"
RTUN_DEBUG_TMUX_SESSION = "rtun-debug-agent"
NIGHTLY_REPO_URL = "https://github.com/simon-fu/rtun.git"
NIGHTLY_SHELL_AGENT_DEFAULT = "nightly-rtun1-manual"
RTUN_CONFIG_PATH = "~/simon/bin/config/rtun-full.toml"
MARKER_PREFIX = "__RTUN_INIT_DONE__"

EXCLUDED_DIR_NAMES = {
    ".git",
    "target",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".venv",
    "venv",
    ".idea",
    ".vscode",
}

EXCLUDED_FILE_NAMES = {
    ".DS_Store",
}

EXCLUDED_FILE_SUFFIXES = {
    ".pyc",
    ".pyo",
    ".swp",
    ".swo",
    ".tmp",
}

MARKER_RE = re.compile(rf"{MARKER_PREFIX}([A-Za-z0-9_-]+):(-?\d+)")


def should_exclude_relative_path(rel_path: Path) -> bool:
    parts = rel_path.parts
    if any(part in EXCLUDED_DIR_NAMES for part in parts):
        return True
    if rel_path.name in EXCLUDED_FILE_NAMES:
        return True
    if rel_path.suffix in EXCLUDED_FILE_SUFFIXES:
        return True
    return False


def collect_archive_members(root: Path) -> list[Path]:
    out: list[Path] = []
    for dirpath, dirnames, filenames in os.walk(root):
        rel_dir = Path(dirpath).relative_to(root)
        dirnames[:] = [
            name
            for name in dirnames
            if not should_exclude_relative_path(rel_dir / name)
        ]
        for filename in filenames:
            rel_file = rel_dir / filename
            if should_exclude_relative_path(rel_file):
                continue
            out.append(rel_file)
    out.sort()
    return out


def create_source_archive(root: Path) -> Path:
    members = collect_archive_members(root)
    fd, path = tempfile.mkstemp(prefix="rtun-local-src-", suffix=".tar.gz")
    os.close(fd)
    tar_path = Path(path)
    with tarfile.open(tar_path, "w:gz") as tar:
        for rel_file in members:
            tar.add(root / rel_file, arcname=str(rel_file))
    return tar_path


def build_kill_processes_by_workdir_command(workdir: str) -> str:
    # Linux-specific: cleanup runs on Ubuntu remote host and relies on /proc/<pid>/cwd.
    q = shlex.quote(workdir)
    return (
        "for pid in $(pgrep -x rtun 2>/dev/null || true); do "
        'cwd=$(readlink -f "/proc/$pid/cwd" 2>/dev/null || true); '
        f'case "$cwd" in {q}|{q}/*) kill "$pid" ;; esac; '
        "done"
    )


def build_nightly_cleanup_commands() -> list[str]:
    cmds = [
        f"tmux kill-session -t {shlex.quote(RTUN_DEBUG_TMUX_SESSION)} 2>/dev/null || true",
        build_kill_processes_by_workdir_command(RTUN_NIGHTLY_WORKDIR),
        f"rm -rf {shlex.quote(RTUN_NIGHTLY_WORKDIR)}",
        f"mkdir -p {shlex.quote(RTUN_NIGHTLY_WORKDIR)}",
    ]
    return cmds


def build_nightly_git_commands(branch: str) -> list[str]:
    q_branch = shlex.quote(branch)
    q_workdir = shlex.quote(RTUN_NIGHTLY_WORKDIR)
    q_repo = shlex.quote(NIGHTLY_REPO_URL)
    return [
        (
            f"if [ ! -d {q_workdir}/.git ]; then "
            f"git clone {q_repo} {q_workdir}; fi"
        ),
        f"cd {q_workdir} && git fetch origin --prune",
        f"cd {q_workdir} && git checkout {q_branch}",
        f"cd {q_workdir} && git reset --hard origin/{q_branch}",
    ]


def build_build_command(workdir: str) -> str:
    return f"cd {shlex.quote(workdir)} && cargo build -p rtun --bin rtun"


def wrap_remote_command_for_marker(command: str, marker: str) -> str:
    return (
        "{ "
        + command
        + "; }; __rtun_init_rc=$?; "
        + f"printf '{MARKER_PREFIX}{marker}:%s\\n' \"$__rtun_init_rc\""
    )


def parse_remote_command_result(line: str, marker: str) -> Optional[int]:
    m = MARKER_RE.search(line)
    if not m:
        return None
    got_marker, rc_text = m.groups()
    if got_marker != marker:
        return None
    return int(rc_text)


def format_step(prefix: str, text: str) -> str:
    return f"[{prefix}] {text}"
