#!/bin/sh

set -eu

REPO_DIR=/tmp/rtun-issue9-agent
BRANCH=issue/9-hardnat-protocol
AGENT_NAME=nightly-rtun1-hardnat-debug
SESSION_NAME=rtun-issue9-agent

cleanup_tmux_agent_env() {
  tmux set-environment -gu MY_RTUN1_URL 2>/dev/null || true
  tmux set-environment -gu MY_RTUN1_SECRET 2>/dev/null || true
}

echo "prepare repo [$REPO_DIR] branch [$BRANCH]"
rm -rf "$REPO_DIR"
git clone --depth 1 --branch "$BRANCH" https://github.com/simon-fu/rtun.git "$REPO_DIR"

cd "$REPO_DIR"
echo "build rtun from [$BRANCH]"
cargo build -p rtun --bin rtun

tmux kill-session -t "$SESSION_NAME" 2>/dev/null || true
cleanup_tmux_agent_env
trap cleanup_tmux_agent_env EXIT INT TERM
tmux set-environment -g MY_RTUN1_URL "$MY_RTUN1_URL"
tmux set-environment -g MY_RTUN1_SECRET "$MY_RTUN1_SECRET"
tmux new-session -d -s "$SESSION_NAME" \
  "cd '$REPO_DIR' && ./target/debug/rtun agent pub \"\$MY_RTUN1_URL\" --agent \"$AGENT_NAME\" --secret \"\$MY_RTUN1_SECRET\" --expire_in 60"
cleanup_tmux_agent_env

sleep 2
tmux ls | grep -F "${SESSION_NAME}:"
echo "started agent [$AGENT_NAME]"
