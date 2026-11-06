#!/usr/bin/env bash
# Builds the Vibekey executable for Windows 7.
#
#   scripts/build-win.sh                # x86_64 (64 bit)
#   scripts/build-win.sh i686-win7-windows-gnu
#
# Requires scripts/install-toolchain.sh to have been run once.
set -euo pipefail
cd "$(dirname "$0")/.."

# shellcheck source=scripts/dev-env.sh
. scripts/dev-env.sh

TARGET="${1:-x86_64-win7-windows-gnu}"

echo "==> building vibekey for ${TARGET}"
cargo +nightly build --release -Z build-std=std,panic_abort --target "${TARGET}" -p vibekey

OUT="target/${TARGET}/release/vibekey.exe"
if [ ! -f "${OUT}" ]; then
    echo "build failed: ${OUT} not found" >&2
    exit 1
fi
echo "==> built ${OUT}"
