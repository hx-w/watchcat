#!/bin/sh
set -eu

REPOSITORY="${WATCHCAT_REPOSITORY:-hx-w/watchcat}"
VERSION="${WATCHCAT_VERSION:-latest}"
INSTALL_DIR="${WATCHCAT_INSTALL_DIR:-${HOME}/.local/bin}"
DRY_RUN=0

usage() {
  cat <<'EOF'
Install or update Watchcat from GitHub Releases.

Usage: install.sh [--version VERSION] [--to DIRECTORY] [--dry-run]

Environment:
  WATCHCAT_VERSION       Release tag, such as v0.1.0. Default: latest
  WATCHCAT_INSTALL_DIR   Destination directory. Default: $HOME/.local/bin
  WATCHCAT_REPOSITORY    GitHub owner/repository. Default: hx-w/watchcat
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --version)
      [ "$#" -ge 2 ] || { echo "--version requires a value" >&2; exit 2; }
      VERSION="$2"
      shift 2
      ;;
    --to)
      [ "$#" -ge 2 ] || { echo "--to requires a value" >&2; exit 2; }
      INSTALL_DIR="$2"
      shift 2
      ;;
    --dry-run)
      DRY_RUN=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

command -v curl >/dev/null 2>&1 || { echo "curl is required" >&2; exit 1; }

case "$(uname -s)" in
  Darwin) platform="apple-darwin" ;;
  Linux) platform="unknown-linux-gnu" ;;
  *) echo "unsupported operating system: $(uname -s)" >&2; exit 1 ;;
esac

case "$(uname -m)" in
  x86_64|amd64) architecture="x86_64" ;;
  arm64|aarch64) architecture="aarch64" ;;
  *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

target="${architecture}-${platform}"

state_dir="${WATCHCAT_STATE_DIR:-}"
if [ -z "$state_dir" ]; then
  case "$platform" in
    apple-darwin) state_dir="${HOME}/Library/Application Support/ai.watchcat.watchcat" ;;
    unknown-linux-gnu) state_dir="${XDG_STATE_HOME:-${HOME}/.local/state}/watchcat" ;;
  esac
fi
if [ "$DRY_RUN" -ne 1 ] && [ -S "${state_dir}/watchcat.sock" ]; then
  echo "Watchcat service is running. Stop it before updating, then rerun the installer." >&2
  exit 1
fi

if [ "$VERSION" = "latest" ]; then
  release_url="$(curl --proto '=https' --tlsv1.2 -fsSL -o /dev/null -w '%{url_effective}' "https://github.com/${REPOSITORY}/releases/latest")"
  VERSION="${release_url##*/}"
fi

printf '%s\n' "$VERSION" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+$' || {
  echo "invalid release version: $VERSION" >&2
  exit 1
}

archive="watchcat-${target}.tar.gz"
base_url="${WATCHCAT_RELEASE_BASE_URL:-https://github.com/${REPOSITORY}/releases/download/${VERSION}}"

fetch() {
  case "$base_url" in
    https://*) curl --proto '=https' --tlsv1.2 --retry 3 --retry-delay 1 --retry-connrefused -fsSL "$1" -o "$2" ;;
    http://127.0.0.1:*|http://localhost:*) curl --proto '=http' --retry 3 --retry-delay 1 --retry-connrefused -fsSL "$1" -o "$2" ;;
    *) echo "release base URL must use HTTPS (localhost HTTP is allowed for tests)" >&2; exit 1 ;;
  esac
}

if [ "$DRY_RUN" -eq 1 ]; then
  printf 'Would install %s for %s to %s\n' "$VERSION" "$target" "$INSTALL_DIR"
  exit 0
fi

temporary="$(mktemp -d "${TMPDIR:-/tmp}/watchcat-install.XXXXXX")"
restore_install=0
lock_pid=""
cleanup() {
  status=$?
  if [ -n "$lock_pid" ]; then
    kill "$lock_pid" 2>/dev/null || true
    wait "$lock_pid" 2>/dev/null || true
  fi
  if [ "$restore_install" -eq 1 ]; then
    for binary in watchcat watchcatd; do
      if [ -f "${temporary}/${binary}.backup" ]; then
        mv "${temporary}/${binary}.backup" "${INSTALL_DIR}/${binary}"
      else
        rm -f "${INSTALL_DIR}/${binary}"
      fi
    done
  fi
  rm -rf "$temporary"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

fetch "${base_url}/${archive}" "${temporary}/${archive}"
fetch "${base_url}/SHA256SUMS" "${temporary}/SHA256SUMS"

expected="$(awk -v file="$archive" '$2 == file { print $1 }' "${temporary}/SHA256SUMS")"
[ -n "$expected" ] || { echo "checksum is missing for ${archive}" >&2; exit 1; }
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "${temporary}/${archive}" | awk '{print $1}')"
elif command -v shasum >/dev/null 2>&1; then
  actual="$(shasum -a 256 "${temporary}/${archive}" | awk '{print $1}')"
else
  echo "sha256sum or shasum is required" >&2
  exit 1
fi
[ "$actual" = "$expected" ] || { echo "checksum verification failed" >&2; exit 1; }

tar -xzf "${temporary}/${archive}" -C "$temporary"
mkdir -p "$INSTALL_DIR"
install -m 755 "${temporary}/watchcat-${target}/watchcat" "${INSTALL_DIR}/watchcat.tmp"
install -m 755 "${temporary}/watchcat-${target}/watchcatd" "${INSTALL_DIR}/watchcatd.tmp"

# Hold the same advisory lock as the daemon across the replacement. The
# socket-only preflight above is user-friendly, while this closes the startup
# and download TOCTOU windows.
mkdir -p "$state_dir"
chmod 700 "$state_dir" 2>/dev/null || true
lock_ready="${temporary}/update-lock-ready"
"${temporary}/watchcat-${target}/watchcatd" \
  --hold-update-lock "$state_dir/watchcat.lock" \
  --lock-ready "$lock_ready" &
lock_pid=$!
attempt=0
while [ ! -f "$lock_ready" ] && kill -0 "$lock_pid" 2>/dev/null && [ "$attempt" -lt 50 ]; do
  sleep 0.1
  attempt=$((attempt + 1))
done
if [ ! -f "$lock_ready" ]; then
  wait "$lock_pid" 2>/dev/null || true
  lock_pid=""
  echo "Watchcat service started while the update was downloading. Stop it and rerun the installer." >&2
  exit 1
fi
if [ -S "${state_dir}/watchcat.sock" ]; then
  echo "Watchcat service is running. Stop it before updating, then rerun the installer." >&2
  exit 1
fi
for binary in watchcat watchcatd; do
  if [ -f "${INSTALL_DIR}/${binary}" ]; then
    cp -p "${INSTALL_DIR}/${binary}" "${temporary}/${binary}.backup"
  fi
done
restore_install=1
mv "${INSTALL_DIR}/watchcat.tmp" "${INSTALL_DIR}/watchcat"
mv "${INSTALL_DIR}/watchcatd.tmp" "${INSTALL_DIR}/watchcatd"
test "$("${INSTALL_DIR}/watchcat" --version)" = "watchcat ${VERSION#v}"
test "$("${INSTALL_DIR}/watchcatd" --version)" = "watchcatd ${VERSION#v}"
restore_install=0

printf 'Installed Watchcat %s CLI and daemon to %s\n' "$VERSION" "$INSTALL_DIR"
case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) ;;
  *) printf 'Add %s to PATH to run watchcat.\n' "$INSTALL_DIR" ;;
esac
