#!/usr/bin/env sh
set -eu

REPO="parsablelabs/relaypod"
INSTALL_DIR="${RELAYPOD_INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${RELAYPOD_VERSION:-}"

usage() {
  cat <<EOF
Install the RelayPod CLI/server binary (relaypod) from GitHub Releases.

Usage:
  sh install.sh [--version v0.1.0] [--dir /path/to/bin]

Environment variables:
  RELAYPOD_VERSION      Release tag to install (defaults to latest release)
  RELAYPOD_INSTALL_DIR  Destination directory (defaults to \$HOME/.local/bin)
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    -v|--version)
      VERSION="${2:-}"
      shift 2
      ;;
    -d|--dir)
      INSTALL_DIR="${2:-}"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

OS_RAW="$(uname -s)"
ARCH_RAW="$(uname -m)"

case "$OS_RAW" in
  Darwin) OS="macos" ;;
  Linux)  OS="linux" ;;
  *)
    echo "Unsupported OS: $OS_RAW (supported: macOS, Linux)" >&2
    exit 1
    ;;
esac

case "$ARCH_RAW" in
  arm64|aarch64) ARCH="arm64" ;;
  x86_64|amd64)  ARCH="x86_64" ;;
  *)
    echo "Unsupported architecture: $ARCH_RAW" >&2
    exit 1
    ;;
esac

if [ "$OS" = "linux" ] && [ "$ARCH" != "x86_64" ]; then
  echo "Prebuilt Linux binaries are currently published for x86_64 only (detected: $ARCH_RAW)." >&2
  exit 1
fi

if [ -z "$VERSION" ]; then
  echo "Fetching latest RelayPod release..."
  VERSION="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
    | sed -nE 's/.*"tag_name"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/p' \
    | head -n 1)"
  if [ -z "$VERSION" ]; then
    echo "Failed to determine latest release tag from GitHub API." >&2
    exit 1
  fi
fi

ASSET="relaypod-${VERSION}-${OS}-${ARCH}.tar.gz"
BASE_URL="https://github.com/${REPO}/releases/download/${VERSION}"

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT INT TERM

echo "Downloading ${ASSET}..."
curl -fsSL -o "${TMP_DIR}/${ASSET}" "${BASE_URL}/${ASSET}"

if curl -fsSL -o "${TMP_DIR}/SHA256SUMS.txt" "${BASE_URL}/SHA256SUMS.txt" 2>/dev/null; then
  EXPECTED="$(grep " ${ASSET}\$" "${TMP_DIR}/SHA256SUMS.txt" | awk '{print $1}' | head -n 1 || true)"
  if [ -n "$EXPECTED" ]; then
    if command -v shasum >/dev/null 2>&1; then
      ACTUAL="$(shasum -a 256 "${TMP_DIR}/${ASSET}" | awk '{print $1}')"
    elif command -v sha256sum >/dev/null 2>&1; then
      ACTUAL="$(sha256sum "${TMP_DIR}/${ASSET}" | awk '{print $1}')"
    else
      ACTUAL=""
    fi
    if [ -n "$ACTUAL" ] && [ "$ACTUAL" != "$EXPECTED" ]; then
      echo "Checksum verification failed for ${ASSET}!" >&2
      echo "  expected: ${EXPECTED}" >&2
      echo "  actual:   ${ACTUAL}" >&2
      exit 1
    fi
    echo "Checksum verified."
  fi
fi

tar -xzf "${TMP_DIR}/${ASSET}" -C "$TMP_DIR"

BIN_SRC="${TMP_DIR}/relaypod-${VERSION}-${OS}-${ARCH}/relaypod"
if [ ! -f "$BIN_SRC" ]; then
  echo "Expected binary not found in archive: ${BIN_SRC}" >&2
  exit 1
fi

mkdir -p "$INSTALL_DIR"
install -m 755 "$BIN_SRC" "${INSTALL_DIR}/relaypod"

CONFIG_DIR="${RELAYPOD_CONFIG_DIR:-$HOME/.relaypod}"
CONFIG_FILE="${CONFIG_DIR}/config.yaml"
DEFAULT_ADMIN="${USER:-admin}"

mkdir -p "$CONFIG_DIR"
chmod 700 "$CONFIG_DIR"

if [ ! -f "$CONFIG_FILE" ]; then
  cat > "$CONFIG_FILE" <<EOF
# RelayPod configuration
# Server:  relaypod serve
# Watcher: relaypod watch

server:
  # Human handles granted admin privileges (config-only, never stored in SQLite)
  admin_handles:
    - ${DEFAULT_ADMIN}

watcher:
  url: http://127.0.0.1:8787
  global_max_sessions: 10
  max_consecutive_agent_turns: 6
  # instructions_file: ~/.relaypod/agent-instructions.md

  # Uncomment and configure one or more agent handles to run with \`relaypod watch\`:
  # handles:
  #   codex-local:
  #     runner: herdr              # 'herdr' (interactive panes) or 'exec' (headless CLI)
  #     agent_type: codex          # codex, claude, gemini, agy, etc.
  #     token: \${RELAYPOD_CODEX_TOKEN}
  #     cwd: ~/projects
  #     auto_ack: false
  #     drain_on_start: true
  #     cooldown_secs: 0
  #     max_sessions: 5
  #     # herdr_session: main
  #     # herdr_workspace: relaypod
  #     # args: --full-auto
  #     # env:
  #     #   RUST_LOG: info
EOF
  chmod 600 "$CONFIG_FILE"
  echo "Created starter config at ${CONFIG_FILE} (admin: ${DEFAULT_ADMIN})"
else
  echo "Keeping existing config at ${CONFIG_FILE}"
fi

echo "Installed relaypod ${VERSION} to ${INSTALL_DIR}/relaypod"
case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) ;;
  *)
    echo ""
    echo "Note: ${INSTALL_DIR} is not in your PATH. Add it with:"
    echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
    ;;
esac
