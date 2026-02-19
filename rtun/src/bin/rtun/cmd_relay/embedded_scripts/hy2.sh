#!/usr/bin/env sh
set -eu

SCRIPT_NAME="hy2"
DEFAULT_VERSION="v2.7.0"
DEFAULT_PASSWORD="rtun_hy2_password"
DEFAULT_LISTEN="127.0.0.1:4433"

VERSION="${DEFAULT_VERSION}"
PASSWORD="${DEFAULT_PASSWORD}"
LISTEN="${DEFAULT_LISTEN}"
LOCK_ENABLED="1"
FORCE_CLOSE="0"
FORCE_DL="0"
CERT_PATH=""
KEY_PATH=""

WORK_DIR="${RTUN_HY2_HOME:-${HOME:-/tmp}/.rtun/hy2}"
LOCK_DIR="${WORK_DIR}/.lock"
PID_FILE="${WORK_DIR}/hysteria.pid"
CFG_FILE="${WORK_DIR}/server.yaml"
LOG_FILE="${WORK_DIR}/hysteria.log"
AUTO_CERT_DIR="${WORK_DIR}/self_signed_certs"
AUTO_CERT_FILE="${AUTO_CERT_DIR}/cert.pem"
AUTO_KEY_FILE="${AUTO_CERT_DIR}/key.pem"

LOCK_HELD="0"

usage() {
  cat <<'EOF'
Usage:
  hy2.sh [options]

Options:
  --version <v2.x.y|2.x.y|app/v2.x.y>  Hysteria2 version (default: v2.7.0)
  --password <value>                    Hysteria2 password
  --listen <port|host:port>             Listen address (default: 127.0.0.1:4433)
  --lock[=true|false]                   Enable lock (default: true)
  --no-lock                             Disable lock
  --force_close[=true|false]            Stop existing instance before start (default: false)
  --force_dl[=true|false]               Re-download binary even when cached (default: false)
  --cert <path>                         TLS cert path (optional)
  --key <path>                          TLS key path (optional)
  -h, --help                            Show help

Notes:
  - If --cert/--key are omitted, the script auto-generates and reuses self-signed cert/key.
  - Binary/log/config path: ${HOME}/.rtun/hy2 (or RTUN_HY2_HOME).
EOF
}

timestamp() {
  date '+%Y-%m-%d %H:%M:%S'
}

log() {
  echo "$(timestamp) [rtun][${SCRIPT_NAME}] $*"
}

err() {
  echo "$(timestamp) [rtun][${SCRIPT_NAME}] ERROR: $*" >&2
}

to_bool() {
  case "$1" in
    1|true|TRUE|yes|YES|on|ON) echo "1" ;;
    0|false|FALSE|no|NO|off|OFF) echo "0" ;;
    *)
      err "invalid boolean value [$1]"
      exit 2
      ;;
  esac
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1
}

download_file() {
  url="$1"
  out="$2"

  if need_cmd curl; then
    curl -fsSL --retry 3 --connect-timeout 10 -o "$out" "$url"
    return 0
  fi

  if need_cmd wget; then
    wget -q -O "$out" "$url"
    return 0
  fi

  err "missing downloader: curl/wget not found"
  return 1
}

normalize_tag() {
  v="$1"
  case "$v" in
    app/v*) echo "$v" ;;
    v*) echo "app/$v" ;;
    *) echo "app/v$v" ;;
  esac
}

yaml_quote() {
  printf "%s" "$1" | sed "s/'/'\"'\"'/g"
}

acquire_lock() {
  if [ "$LOCK_ENABLED" != "1" ]; then
    return 0
  fi

  mkdir -p "$WORK_DIR"
  while ! mkdir "$LOCK_DIR" 2>/dev/null; do
    sleep 1
  done
  LOCK_HELD="1"
  echo "$$" > "${LOCK_DIR}/pid" 2>/dev/null || true
}

release_lock() {
  if [ "$LOCK_HELD" = "1" ]; then
    rm -rf "$LOCK_DIR" >/dev/null 2>&1 || true
    LOCK_HELD="0"
  fi
}

trap 'release_lock' EXIT INT TERM

pid_running() {
  pid="$1"
  kill -0 "$pid" 2>/dev/null
}

pid_matches_hy2() {
  pid="$1"
  cmd="$(ps -p "$pid" -o command= 2>/dev/null || true)"
  case "$cmd" in
    *"${WORK_DIR}/hysteria-current"*server* )
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

current_pid() {
  if [ ! -f "$PID_FILE" ]; then
    return 1
  fi
  pid="$(cat "$PID_FILE" 2>/dev/null || true)"
  case "$pid" in
    ''|*[!0-9]*)
      return 1
      ;;
  esac
  if pid_running "$pid" && pid_matches_hy2 "$pid"; then
    echo "$pid"
    return 0
  fi
  return 1
}

stop_pid() {
  pid="$1"
  log "stopping running hysteria2 pid [$pid]"
  kill "$pid" >/dev/null 2>&1 || true

  waited="0"
  while pid_running "$pid"; do
    if [ "$waited" -ge 15 ]; then
      break
    fi
    waited=$((waited + 1))
    sleep 1
  done

  if pid_running "$pid"; then
    log "force kill hysteria2 pid [$pid]"
    kill -9 "$pid" >/dev/null 2>&1 || true
  fi
  rm -f "$PID_FILE"
}

ensure_tls_material() {
  if [ -n "$CERT_PATH" ] || [ -n "$KEY_PATH" ]; then
    return 0
  fi

  CERT_PATH="$AUTO_CERT_FILE"
  KEY_PATH="$AUTO_KEY_FILE"

  if [ -s "$CERT_PATH" ] && [ -s "$KEY_PATH" ]; then
    log "reuse self-signed cert/key [$CERT_PATH] [$KEY_PATH]"
    return 0
  fi

  if ! need_cmd openssl; then
    err "openssl not found; set --cert/--key explicitly or install openssl"
    exit 1
  fi

  mkdir -p "$AUTO_CERT_DIR"
  tmp_cert="$(mktemp "${TMPDIR:-/tmp}/rtun-hy2-cert.XXXXXX")"
  tmp_key="$(mktemp "${TMPDIR:-/tmp}/rtun-hy2-key.XXXXXX")"
  cleanup_tls_tmp() {
    rm -f "$tmp_cert" "$tmp_key" >/dev/null 2>&1 || true
  }

  if ! openssl req -x509 -newkey rsa:2048 -sha256 -nodes \
      -keyout "$tmp_key" \
      -out "$tmp_cert" \
      -days 3650 \
      -subj "/CN=localhost" >/dev/null 2>&1; then
    cleanup_tls_tmp
    err "generate self-signed cert/key failed"
    exit 1
  fi

  mv "$tmp_cert" "$CERT_PATH"
  mv "$tmp_key" "$KEY_PATH"
  chmod 600 "$KEY_PATH" >/dev/null 2>&1 || true
  cleanup_tls_tmp
  log "generated self-signed cert/key [$CERT_PATH] [$KEY_PATH]"
}

LAST_DOWNLOAD_URL=""
download_from_release() {
  rel_path="$1"
  out="$2"
  if download_file "${base_url_encoded}/${rel_path}" "$out"; then
    LAST_DOWNLOAD_URL="${base_url_encoded}/${rel_path}"
    return 0
  fi
  if [ "$base_url_raw" != "$base_url_encoded" ] && download_file "${base_url_raw}/${rel_path}" "$out"; then
    LAST_DOWNLOAD_URL="${base_url_raw}/${rel_path}"
    return 0
  fi
  return 1
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --version)
      [ "$#" -ge 2 ] || { err "--version requires a value"; exit 2; }
      VERSION="$2"
      shift 2
      ;;
    --version=*)
      VERSION="${1#*=}"
      shift 1
      ;;
    --password)
      [ "$#" -ge 2 ] || { err "--password requires a value"; exit 2; }
      PASSWORD="$2"
      shift 2
      ;;
    --password=*)
      PASSWORD="${1#*=}"
      shift 1
      ;;
    --listen)
      [ "$#" -ge 2 ] || { err "--listen requires a value"; exit 2; }
      LISTEN="$2"
      shift 2
      ;;
    --listen=*)
      LISTEN="${1#*=}"
      shift 1
      ;;
    --lock)
      LOCK_ENABLED="1"
      shift 1
      ;;
    --lock=*)
      LOCK_ENABLED="$(to_bool "${1#*=}")"
      shift 1
      ;;
    --no-lock)
      LOCK_ENABLED="0"
      shift 1
      ;;
    --force_close)
      FORCE_CLOSE="1"
      shift 1
      ;;
    --force_close=*)
      FORCE_CLOSE="$(to_bool "${1#*=}")"
      shift 1
      ;;
    --force_dl)
      FORCE_DL="1"
      shift 1
      ;;
    --force_dl=*)
      FORCE_DL="$(to_bool "${1#*=}")"
      shift 1
      ;;
    --cert)
      [ "$#" -ge 2 ] || { err "--cert requires a value"; exit 2; }
      CERT_PATH="$2"
      shift 2
      ;;
    --cert=*)
      CERT_PATH="${1#*=}"
      shift 1
      ;;
    --key)
      [ "$#" -ge 2 ] || { err "--key requires a value"; exit 2; }
      KEY_PATH="$2"
      shift 2
      ;;
    --key=*)
      KEY_PATH="${1#*=}"
      shift 1
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      err "unknown argument [$1]"
      usage
      exit 2
      ;;
  esac
done

if [ -n "$CERT_PATH" ] || [ -n "$KEY_PATH" ]; then
  if [ -z "$CERT_PATH" ] || [ -z "$KEY_PATH" ]; then
    err "--cert and --key must be used together"
    exit 2
  fi
  [ -f "$CERT_PATH" ] || { err "cert not found [$CERT_PATH]"; exit 2; }
  [ -f "$KEY_PATH" ] || { err "key not found [$KEY_PATH]"; exit 2; }
fi

acquire_lock

if running_pid="$(current_pid 2>/dev/null || true)"; [ -n "${running_pid:-}" ]; then
  if [ "$FORCE_CLOSE" = "1" ]; then
    stop_pid "$running_pid"
  else
    log "hysteria2 already running pid [$running_pid], skip"
    exit 0
  fi
fi

case "$(uname -s 2>/dev/null || true)" in
  Darwin) os="darwin" ;;
  Linux) os="linux" ;;
  *)
    err "unsupported OS [$(uname -s 2>/dev/null || echo unknown)]"
    exit 1
    ;;
esac

case "$(uname -m 2>/dev/null || true)" in
  arm64|aarch64) arch="arm64" ;;
  x86_64|amd64) arch="amd64" ;;
  *)
    err "unsupported CPU arch [$(uname -m 2>/dev/null || echo unknown)]"
    exit 1
    ;;
esac

tag="$(normalize_tag "$VERSION")"
safe_tag="$(printf "%s" "$tag" | tr '/' '_')"
asset="hysteria-${os}-${arch}"
encoded_tag="$(printf "%s" "$tag" | sed 's|/|%2F|g')"
base_url_raw="https://github.com/apernet/hysteria/releases/download/${tag}"
base_url_encoded="https://github.com/apernet/hysteria/releases/download/${encoded_tag}"

mkdir -p "$WORK_DIR"
bin_path="${WORK_DIR}/${asset}-${safe_tag}"
bin_link="${WORK_DIR}/hysteria-current"

if [ "$FORCE_DL" = "1" ]; then
  rm -f "$bin_path"
fi

if [ ! -x "$bin_path" ]; then
  tmp_bin="$(mktemp "${TMPDIR:-/tmp}/rtun-hy2.bin.XXXXXX")"
  cleanup_tmp() {
    rm -f "$tmp_bin" >/dev/null 2>&1 || true
  }
  trap 'cleanup_tmp; release_lock' EXIT INT TERM

  log "downloading ${asset} for tag ${tag}"
  if download_from_release "${asset}" "$tmp_bin"; then
    log "download success: ${asset} (${LAST_DOWNLOAD_URL})"
  else
    err "download failed: ${asset} for tag ${tag}"
    cleanup_tmp
    exit 1
  fi

  mv "$tmp_bin" "$bin_path"
  chmod +x "$bin_path"
  cleanup_tmp
else
  log "reuse cached binary [$bin_path]"
fi

ensure_tls_material

ln -sf "$bin_path" "$bin_link"

if running_pid="$(current_pid 2>/dev/null || true)"; [ -n "${running_pid:-}" ]; then
  log "hysteria2 already running pid [$running_pid], skip"
  exit 0
fi

case "$LISTEN" in
  *:*) listen_addr="$LISTEN" ;;
  *) listen_addr=":$LISTEN" ;;
esac

password_q="$(yaml_quote "$PASSWORD")"
cert_q="$(yaml_quote "$CERT_PATH")"
key_q="$(yaml_quote "$KEY_PATH")"
{
  printf "listen: %s\n\n" "$listen_addr"
  printf "tls:\n"
  printf "  cert: '%s'\n" "$cert_q"
  printf "  key: '%s'\n" "$key_q"
  printf "  sniGuard: disable\n\n"
  printf "auth:\n"
  printf "  type: password\n"
  printf "  password: '%s'\n" "$password_q"
} > "$CFG_FILE"

log "starting hysteria2 (version=${tag}, listen=${listen_addr})"
nohup "$bin_link" -c "$CFG_FILE" server >> "$LOG_FILE" 2>&1 &
new_pid="$!"
echo "$new_pid" > "$PID_FILE"

sleep 1
if ! pid_running "$new_pid"; then
  err "hysteria2 exited unexpectedly, check log [$LOG_FILE]"
  exit 1
fi

log "hysteria2 started pid [$new_pid], log [$LOG_FILE]"
