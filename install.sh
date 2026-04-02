#!/usr/bin/env bash
set -euo pipefail

REPO_OWNER="JacobLinCool"
REPO_NAME="gemini-live-transcribe"
BINARY_NAME="gemini-live-transcribe"
INSTALL_DIR="${GEMINI_LIVE_TRANSCRIBE_INSTALL_DIR:-$HOME/.local/bin}"

require_command() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "Missing required command: $1" >&2
    exit 1
  fi
}

resolve_target() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"

  if [[ "$os" != "Darwin" ]]; then
    echo "This installer only supports macOS." >&2
    exit 1
  fi

  case "$arch" in
    arm64|aarch64)
      echo "aarch64-apple-darwin"
      ;;
    x86_64)
      echo "x86_64-apple-darwin"
      ;;
    *)
      echo "Unsupported macOS architecture: $arch" >&2
      exit 1
      ;;
  esac
}

main() {
  require_command curl
  require_command tar
  require_command install
  require_command mktemp

  local target asset_name download_url temp_dir archive_path extracted_binary
  target="$(resolve_target)"
  asset_name="${BINARY_NAME}-${target}.tar.gz"
  download_url="https://github.com/${REPO_OWNER}/${REPO_NAME}/releases/latest/download/${asset_name}"
  temp_dir="$(mktemp -d)"
  archive_path="${temp_dir}/${asset_name}"
  extracted_binary="${temp_dir}/${BINARY_NAME}"

  trap 'rm -rf "$temp_dir"' EXIT

  mkdir -p "$INSTALL_DIR"

  echo "Downloading ${asset_name}..."
  curl \
    --fail \
    --silent \
    --show-error \
    --location \
    --proto '=https' \
    --tlsv1.2 \
    "$download_url" \
    --output "$archive_path"

  tar -xzf "$archive_path" -C "$temp_dir"

  if [[ ! -f "$extracted_binary" ]]; then
    echo "Release archive did not contain ${BINARY_NAME}." >&2
    exit 1
  fi

  install -m 0755 "$extracted_binary" "${INSTALL_DIR}/${BINARY_NAME}"
  echo "Installed ${BINARY_NAME} to ${INSTALL_DIR}/${BINARY_NAME}"
  echo "Run '${BINARY_NAME} update' to install newer releases later."

  case ":$PATH:" in
    *":${INSTALL_DIR}:"*) ;;
    *)
      echo "Add ${INSTALL_DIR} to PATH if you want to run ${BINARY_NAME} directly."
      ;;
  esac
}

main "$@"
