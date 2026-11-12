#!/usr/bin/env bash
# Build fed in release mode and install the binary in ~/.local/bin.
set -euo pipefail

BIN_NAME="fed"
INSTALL_DIR="${HOME}/.local/bin"
PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

cd "${PROJECT_ROOT}"

echo "Building ${BIN_NAME} (release)..."
cargo build --release --locked --bin "${BIN_NAME}"

mkdir -p "${INSTALL_DIR}"
install -m 755 "target/release/${BIN_NAME}" "${INSTALL_DIR}/${BIN_NAME}"
echo "Installed ${INSTALL_DIR}/${BIN_NAME}"

case ":${PATH}:" in
	*":${INSTALL_DIR}:"*) ;;
	*) echo "Warning: ${INSTALL_DIR} is not in PATH" >&2 ;;
esac
