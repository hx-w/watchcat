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
if [ "$VERSION" = "latest" ]; then
  release_url="$(curl --proto '=https' --tlsv1.2 -fsSL -o /dev/null -w '%{url_effective}' "https://github.com/${REPOSITORY}/releases/latest")"
  VERSION="${release_url##*/}"
fi

printf '%s\n' "$VERSION" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+$' || {
  echo "invalid release version: $VERSION" >&2
  exit 1
}

archive="watchcat-${target}.tar.gz"
base_url="https://github.com/${REPOSITORY}/releases/download/${VERSION}"

if [ "$DRY_RUN" -eq 1 ]; then
  printf 'Would install %s for %s to %s\n' "$VERSION" "$target" "$INSTALL_DIR/watchcat"
  exit 0
fi

temporary="$(mktemp -d "${TMPDIR:-/tmp}/watchcat-install.XXXXXX")"
trap 'rm -rf "$temporary"' EXIT HUP INT TERM

curl --proto '=https' --tlsv1.2 -fsSL "${base_url}/${archive}" -o "${temporary}/${archive}"
curl --proto '=https' --tlsv1.2 -fsSL "${base_url}/SHA256SUMS" -o "${temporary}/SHA256SUMS"

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
mv "${INSTALL_DIR}/watchcat.tmp" "${INSTALL_DIR}/watchcat"

printf 'Installed Watchcat %s to %s\n' "$VERSION" "$INSTALL_DIR/watchcat"
case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) ;;
  *) printf 'Add %s to PATH to run watchcat.\n' "$INSTALL_DIR" ;;
esac
