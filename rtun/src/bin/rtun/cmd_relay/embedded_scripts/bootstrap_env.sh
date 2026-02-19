#!/usr/bin/env sh
set -eu

echo "[rtun] bootstrap_env start"
if command -v uname >/dev/null 2>&1; then
  uname -a || true
fi
echo "[rtun] bootstrap_env done"
