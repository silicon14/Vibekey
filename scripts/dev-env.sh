#!/usr/bin/env bash
# Source this file (bash: . scripts/dev-env.sh) to build Vibekey for Windows 7
# from Linux. It expects scripts/install-toolchain.sh to have been run once.
set -euo pipefail

export PATH="$HOME/.cargo/bin:$PATH"

MINGW_VERSION="${VIBEKEY_LLVM_MINGW_VERSION:-20261104}"
TOOLCHAIN_DIR="${VIBEKEY_TOOLCHAIN_DIR:-$HOME/toolchains}"
MINGW_DIR="${TOOLCHAIN_DIR}/llvm-mingw-${MINGW_VERSION}-msvcrt-ubuntu-22.04-x86_64"

if [ ! -x "${MINGW_DIR}/bin/x86_64-w64-mingw32-clang" ]; then
    echo "llvm-mingw not found in ${MINGW_DIR}; run scripts/install-toolchain.sh" >&2
    return 1 2>/dev/null || exit 1
fi

export PATH="${MINGW_DIR}/bin:${PATH}"
export CARGO_TARGET_X86_64_WIN7_WINDOWS_GNU_LINKER="${MINGW_DIR}/bin/x86_64-w64-mingw32-clang"
export CARGO_TARGET_I686_WIN7_WINDOWS_GNU_LINKER="${MINGW_DIR}/bin/i686-w64-mingw32-clang"
export CARGO_TARGET_X86_64_WIN7_WINDOWS_GNU_RUSTFLAGS="-C link-arg=-Wl,--major-subsystem-version=6 -C link-arg=-Wl,--minor-subsystem-version=1"
export CARGO_TARGET_I686_WIN7_WINDOWS_GNU_RUSTFLAGS="-C link-arg=-Wl,--major-subsystem-version=6 -C link-arg=-Wl,--minor-subsystem-version=1"
export CC_x86_64_win7_windows_gnu="${MINGW_DIR}/bin/x86_64-w64-mingw32-clang"
export CC_i686_win7_windows_gnu="${MINGW_DIR}/bin/i686-w64-mingw32-clang"
export AR_x86_64_win7_windows_gnu="${MINGW_DIR}/bin/llvm-ar"
export AR_i686_win7_windows_gnu="${MINGW_DIR}/bin/llvm-ar"
