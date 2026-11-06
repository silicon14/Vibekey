#!/usr/bin/env bash
# Installs the cross compilation toolchain used to build Vibekey for Windows 7
# from Linux. Everything is installed below $HOME/toolchains, root is not
# required.
#
#   * llvm-mingw (MSVCRT flavour, runs on Windows 7 without the UCRT update)
#   * a nightly Rust toolchain with rust-src, used with -Z build-std for the
#     tier 3 win7 targets (the prebuilt std of the normal windows targets
#     requires Windows 10 since Rust 1.78).
#
# The script is idempotent.
set -euo pipefail

MINGW_VERSION="${VIBEKEY_LLVM_MINGW_VERSION:-20261104}"
TOOLCHAIN_DIR="${VIBEKEY_TOOLCHAIN_DIR:-$HOME/toolchains}"
MINGW_NAME="llvm-mingw-${MINGW_VERSION}-msvcrt-ubuntu-22.04-x86_64"
MINGW_DIR="${TOOLCHAIN_DIR}/${MINGW_NAME}"
DOWNLOAD_URL="https://github.com/mstorsjo/llvm-mingw/releases/download/${MINGW_VERSION}/${MINGW_NAME}.tar.xz"

mkdir -p "${TOOLCHAIN_DIR}"

if [ ! -x "${MINGW_DIR}/bin/x86_64-w64-mingw32-clang" ]; then
    echo "==> downloading llvm-mingw ${MINGW_VERSION}"
    curl -fL --retry 3 -o "${TOOLCHAIN_DIR}/llvm-mingw.tar.xz" "${DOWNLOAD_URL}"
    tar -xf "${TOOLCHAIN_DIR}/llvm-mingw.tar.xz" -C "${TOOLCHAIN_DIR}"
    rm -f "${TOOLCHAIN_DIR}/llvm-mingw.tar.xz"
fi

# Rust's *-windows-gnu targets ask the linker for libgcc/libgcc_eh, but
# llvm-mingw ships compiler-rt instead. Provide thin symlinks inside the
# sysroots so no -L ordering hacks are needed.
for ARCH in x86_64 i686; do
    SYSROOT="${MINGW_DIR}/${ARCH}-w64-mingw32"
    LIB="${SYSROOT}/lib"
    if [ "${ARCH}" = "x86_64" ]; then
        BUILTINS="${MINGW_DIR}/lib/clang/*/lib/windows/libclang_rt.builtins-x86_64.a"
    else
        BUILTINS="${MINGW_DIR}/lib/clang/*/lib/windows/libclang_rt.builtins-i386.a"
    fi
    ln -sf ${BUILTINS} "${LIB}/libgcc.a"
    ln -sf "${LIB}/libunwind.a" "${LIB}/libgcc_eh.a"
done

echo "==> rust nightly + rust-src"
if ! rustup toolchain list | grep -q '^nightly-'; then
    rustup toolchain install nightly --profile minimal --component rust-src
else
    rustup component add rust-src --toolchain nightly || true
fi

echo
echo "Toolchain ready. Source scripts/dev-env.sh and run scripts/build-win.sh."
