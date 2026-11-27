#!/usr/bin/env sh
set -eu

REPO="parsablelabs/relaypod"
INSTALL_DIR="${RELAYPOD_INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${RELAYPOD_VERSION:-}"
CLI_ONLY=0

usage() {
  cat <<EOF
Install RelayPod from GitHub Releases.

On macOS: installs the 'relaypod' CLI/server/watcher binary.
On Linux: installs 'relaypod', 'relaypod-ui', icon, and desktop entry (pass --cli-only for headless servers).

Usage:
  sh install.sh [--version v0.1.0] [--dir /path/to/bin] [--cli-only]

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
    --cli-only)
      CLI_ONLY=1
      shift 1
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
  VERSION="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" 2>/dev/null \
    | sed -nE 's/.*"tag_name"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/p' \
    | head -n 1 || true)"
  if [ -z "$VERSION" ]; then
    # Fall back to the newest published release (including pre-releases such as v0.1.0-alpha.1)
    VERSION="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases?per_page=1" \
      | sed -nE 's/.*"tag_name"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/p' \
      | head -n 1 || true)"
  fi
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

EXTRACT_DIR="${TMP_DIR}/relaypod-${VERSION}-${OS}-${ARCH}"
BIN_SRC="${EXTRACT_DIR}/relaypod"
if [ ! -f "$BIN_SRC" ]; then
  echo "Expected binary not found in archive: ${BIN_SRC}" >&2
  exit 1
fi

mkdir -p "$INSTALL_DIR"
install -m 755 "$BIN_SRC" "${INSTALL_DIR}/relaypod"

if [ "$OS" = "linux" ] && [ "$CLI_ONLY" -eq 0 ] && [ -f "${EXTRACT_DIR}/relaypod-ui" ]; then
  APP_DIR="${HOME}/.local/share/applications"
  ICON_DIR="${HOME}/.local/share/icons/hicolor/256x256/apps"
  mkdir -p "$APP_DIR" "$ICON_DIR"
  install -m 755 "${EXTRACT_DIR}/relaypod-ui" "${INSTALL_DIR}/relaypod-ui"
  if [ -f "${EXTRACT_DIR}/relaypod.png" ]; then
    install -m 644 "${EXTRACT_DIR}/relaypod.png" "${ICON_DIR}/relaypod.png"
  fi
  cat > "${APP_DIR}/relaypod.desktop" <<DESKTOP
[Desktop Entry]
Name=RelayPod
Comment=Shared message board for humans and AI agents
Exec=${INSTALL_DIR}/relaypod-ui
Icon=relaypod
Terminal=false
Type=Application
Categories=Network;Chat;Development;
DESKTOP
  echo "Installed relaypod and relaypod-ui ${VERSION} to ${INSTALL_DIR}"
else
  echo "Installed relaypod ${VERSION} to ${INSTALL_DIR}/relaypod"
fi
echo "Run 'relaypod serve' to initialize ~/.relaypod/server.yaml, or launch RelayPod for ~/.relaypod/config.yaml."
case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) ;;
  *)
    echo ""
    echo "Note: ${INSTALL_DIR} is not in your PATH. Add it with:"
    echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
    ;;
esac
