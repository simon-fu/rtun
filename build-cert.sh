#!/usr/bin/env sh
set -eu

SCRIPT_NAME="build-cert.sh"
SERVER_NAME="localhost"
DRY_RUN="0"
DEFAULT_REPO="simon-fu/rtun"

usage() {
  cat <<'EOF'
Usage:
  build-cert.sh gen <out_dir> [--dry-run]
  build-cert.sh gh <cert_dir> [--dry-run]
  build-cert.sh fp <cert_path> [--dry-run]
  build-cert.sh env <cert_dir> [--dry-run]
  build-cert.sh -h | --help

Commands:
  gen  Generate cert.pem + key.pem in <out_dir> (SERVER_NAME is fixed to localhost)
  gh   Set GitHub secrets RTUN_HY2_TLS_CERT_PEM / RTUN_HY2_TLS_KEY_PEM from <cert_dir>
  fp   Print SHA256 fingerprint of <cert_path>
  env  Print shell exports for RTUN_HY2_TLS_CERT_PEM / RTUN_HY2_TLS_KEY_PEM

Env:
  RTUN_GH_REPO        Override GitHub repo (default: simon-fu/rtun)
  RTUN_GH_TOKEN_FILE  Optional token file path; if unset, use existing gh login session
EOF
}

err() {
  echo "[${SCRIPT_NAME}] ERROR: $*" >&2
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || {
    err "missing command: $1"
    exit 1
  }
}

print_cmd() {
  printf "+ %s\n" "$*"
}

shell_quote() {
  printf "'%s'" "$(printf "%s" "$1" | sed "s/'/'\"'\"'/g")"
}

run_cmd() {
  print_cmd "$*"
  if [ "$DRY_RUN" = "0" ]; then
    "$@"
  fi
}

resolve_repo() {
  if [ -n "${RTUN_GH_REPO:-}" ]; then
    echo "$RTUN_GH_REPO"
    return 0
  fi

  script_dir="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
  remote_url=""

  if remote_url="$(git -C "$script_dir" remote get-url simon 2>/dev/null || true)"; [ -n "$remote_url" ]; then
    :
  elif remote_url="$(git -C "$script_dir" remote get-url origin 2>/dev/null || true)"; [ -n "$remote_url" ]; then
    :
  else
    echo "$DEFAULT_REPO"
    return 0
  fi

  repo="$(printf "%s" "$remote_url" | sed -E 's#^git@github.com:##; s#^https://github.com/##; s#\.git$##')"
  case "$repo" in
    */*) echo "$repo" ;;
    *) echo "$DEFAULT_REPO" ;;
  esac
}

ensure_gh_token() {
  if [ -n "${GH_TOKEN:-}" ]; then
    return 0
  fi

  if [ -z "${RTUN_GH_TOKEN_FILE:-}" ]; then
    return 0
  fi

  token_file="$RTUN_GH_TOKEN_FILE"
  print_cmd "export GH_TOKEN=\"\$(tr -d '\\r\\n' < ${token_file})\""
  if [ "$DRY_RUN" = "1" ]; then
    return 0
  fi

  [ -f "$token_file" ] || {
    err "token file not found: $token_file"
    exit 1
  }
  GH_TOKEN="$(tr -d '\r\n' < "$token_file")"
  [ -n "$GH_TOKEN" ] || {
    err "token file is empty: $token_file"
    exit 1
  }
  export GH_TOKEN
}

cmd_gen() {
  out_dir="$1"
  cert_file="${out_dir}/cert.pem"
  key_file="${out_dir}/key.pem"

  need_cmd openssl
  run_cmd mkdir -p "$out_dir"

  print_cmd "openssl req -x509 -newkey rsa:2048 -sha256 -nodes -keyout ${key_file} -out ${cert_file} -days 3650 -subj /CN=${SERVER_NAME} -addext subjectAltName=DNS:${SERVER_NAME}"
  if [ "$DRY_RUN" = "1" ]; then
    return 0
  fi

  rm -f "$cert_file" "$key_file"
  if openssl req -x509 -newkey rsa:2048 -sha256 -nodes \
      -keyout "$key_file" \
      -out "$cert_file" \
      -days 3650 \
      -subj "/CN=${SERVER_NAME}" \
      -addext "subjectAltName=DNS:${SERVER_NAME}"; then
    return 0
  fi

  rm -f "$cert_file" "$key_file"
  tmp_cfg="$(mktemp "${TMPDIR:-/tmp}/rtun-cert-openssl.XXXXXX")"
  cat > "$tmp_cfg" <<'EOF'
[req]
distinguished_name = req_distinguished_name
x509_extensions = v3_req
prompt = no

[req_distinguished_name]
CN = localhost

[v3_req]
subjectAltName = @alt_names

[alt_names]
DNS.1 = localhost
EOF

  if ! openssl req -x509 -newkey rsa:2048 -sha256 -nodes \
      -keyout "$key_file" \
      -out "$cert_file" \
      -days 3650 \
      -config "$tmp_cfg" \
      -extensions v3_req; then
    rm -f "$tmp_cfg"
    err "failed to generate cert/key"
    exit 1
  fi
  rm -f "$tmp_cfg"
}

cmd_gh() {
  cert_dir="$1"
  cert_file="${cert_dir}/cert.pem"
  key_file="${cert_dir}/key.pem"

  repo="$(resolve_repo)"
  if [ "$DRY_RUN" = "0" ]; then
    [ -f "$cert_file" ] || {
      err "cert not found: $cert_file"
      exit 1
    }
    [ -f "$key_file" ] || {
      err "key not found: $key_file"
      exit 1
    }
    need_cmd gh
  fi

  ensure_gh_token
  print_cmd "gh secret set RTUN_HY2_TLS_CERT_PEM -R ${repo} < ${cert_file}"
  if [ "$DRY_RUN" = "0" ]; then
    gh secret set RTUN_HY2_TLS_CERT_PEM -R "$repo" < "$cert_file"
  fi

  print_cmd "gh secret set RTUN_HY2_TLS_KEY_PEM -R ${repo} < ${key_file}"
  if [ "$DRY_RUN" = "0" ]; then
    gh secret set RTUN_HY2_TLS_KEY_PEM -R "$repo" < "$key_file"
  fi
}

cmd_fp() {
  cert_file="$1"

  print_cmd "openssl x509 -noout -fingerprint -sha256 -inform pem -in ${cert_file} | sed 's/^[Ss][Hh][Aa]256 Fingerprint=//'"
  if [ "$DRY_RUN" = "1" ]; then
    return 0
  fi

  [ -f "$cert_file" ] || {
    err "cert not found: $cert_file"
    exit 1
  }
  need_cmd openssl

  fp_raw="$(openssl x509 -noout -fingerprint -sha256 -inform pem -in "$cert_file")"
  printf "%s\n" "$fp_raw" | sed 's/^[Ss][Hh][Aa]256 Fingerprint=//'
}

cmd_env() {
  cert_dir="$1"
  cert_file="${cert_dir}/cert.pem"
  key_file="${cert_dir}/key.pem"

  if [ "$DRY_RUN" = "0" ]; then
    [ -f "$cert_file" ] || {
      err "cert not found: $cert_file"
      exit 1
    }
    [ -f "$key_file" ] || {
      err "key not found: $key_file"
      exit 1
    }
  fi

  q_cert_file="$(shell_quote "$cert_file")"
  q_key_file="$(shell_quote "$key_file")"

  if [ "$DRY_RUN" = "1" ]; then
    print_cmd "export RTUN_HY2_TLS_CERT_PEM=\"\$(cat ${q_cert_file})\""
    print_cmd "export RTUN_HY2_TLS_KEY_PEM=\"\$(cat ${q_key_file})\""
    return 0
  fi

  printf "export RTUN_HY2_TLS_CERT_PEM=\"\$(cat %s)\"\n" "$q_cert_file"
  printf "export RTUN_HY2_TLS_KEY_PEM=\"\$(cat %s)\"\n" "$q_key_file"
}

if [ "$#" -eq 0 ]; then
  usage
  exit 2
fi

case "$1" in
  -h|--help)
    usage
    exit 0
    ;;
esac

subcmd="$1"
shift

target_path=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --dry-run)
      DRY_RUN="1"
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      if [ -z "$target_path" ]; then
        target_path="$1"
      else
        err "unexpected argument: $1"
        usage
        exit 2
      fi
      ;;
  esac
  shift
done

[ -n "$target_path" ] || {
  err "missing path argument"
  usage
  exit 2
}

case "$subcmd" in
  gen) cmd_gen "$target_path" ;;
  gh) cmd_gh "$target_path" ;;
  fp) cmd_fp "$target_path" ;;
  env) cmd_env "$target_path" ;;
  *)
    err "unknown command: $subcmd"
    usage
    exit 2
    ;;
esac
