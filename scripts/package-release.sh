#!/usr/bin/env bash
# Build and package the Windows 7 release variants of Vibekey.
#
# Usage:
#   scripts/package-release.sh [options]
#
# Options:
#   --skip-build    reuse target/<triple>/release/vibekey.exe instead of building
#   --no-verify     skip the PE / Windows 7 compatibility check (not recommended)
#   --version X.Y.Z override the version taken from Cargo.toml
#   -h, --help      show this help
#
# For every supported target a self contained directory is produced:
#
#   dist/vibekey-<version>-win7-x64/     64 bit (x86_64-win7-windows-gnu)
#   dist/vibekey-<version>-win7-win32/   32 bit (i686-win7-windows-gnu)
#
# Each directory contains vibekey.exe, config.example.toml, README.md, LICENSE
# and the user documentation. The SHA-256 of the executables is written to
# dist/SHA256SUMS. The script fails if scripts/verify-win7-pe.sh rejects either
# executable.
#
# Requirements: scripts/install-toolchain.sh must have been run once, exactly
# like for scripts/build-win.sh.
set -euo pipefail
cd "$(dirname "$0")/.."

BUILD=1
VERIFY=1
VERSION_OVERRIDE=""

usage() {
    cat <<'EOF'
Build and package the Windows 7 release variants of Vibekey.

Usage:
  scripts/package-release.sh [options]

Options:
  --skip-build    reuse target/<triple>/release/vibekey.exe instead of building
  --no-verify     skip the PE / Windows 7 compatibility check (not recommended)
  --version X.Y.Z override the version taken from Cargo.toml
  -h, --help      show this help

Output:
  dist/vibekey-<version>-win7-x64/
  dist/vibekey-<version>-win7-win32/
  dist/SHA256SUMS
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --skip-build) BUILD=0 ;;
        --no-verify) VERIFY=0 ;;
        --version)
            [ "$#" -ge 2 ] || { echo "error: --version needs a value" >&2; exit 2; }
            VERSION_OVERRIDE="$2"
            shift ;;
        -h|--help)
            usage
            exit 0 ;;
        *)
            echo "error: unknown argument: $1" >&2
            usage >&2
            exit 2 ;;
    esac
    shift
done

workspace_version() {
    awk '
        /^\[workspace\.package\]/ { in_pkg = 1; next }
        /^\[/ { in_pkg = 0 }
        in_pkg && $1 == "version" {
            gsub(/"/, "", $3)
            print $3
            exit
        }
    ' Cargo.toml
}

if [ -n "${VERSION_OVERRIDE}" ]; then
    VERSION="${VERSION_OVERRIDE}"
else
    VERSION="${VIBEKEY_VERSION:-$(workspace_version)}"
fi
if ! [[ "${VERSION}" =~ ^[0-9A-Za-z._+-]+$ ]]; then
    echo "error: invalid version '${VERSION}'" >&2
    exit 2
fi

DIR="$(pwd)"
DIST="${DIR}/dist"
EXE_NAME="vibekey.exe"

require_file() {
    if [ ! -f "$1" ]; then
        echo "error: required file is missing: $1" >&2
        exit 1
    fi
}

# variant <directory-suffix> <rust-target>
package_variant() {
    local suffix="$1" target="$2"
    local exe="target/${target}/release/${EXE_NAME}"
    local out="${DIST}/vibekey-${VERSION}-${suffix}"

    if [ "${BUILD}" -eq 1 ]; then
        scripts/build-win.sh "${target}"
    fi
    if [ ! -f "${exe}" ]; then
        echo "error: ${exe} not found (run scripts/build-win.sh ${target} first)" >&2
        exit 1
    fi

    rm -rf -- "${out}"
    mkdir -p "${out}/docs"
    install -m 0644 "${exe}" "${out}/${EXE_NAME}"
    install -m 0644 assets/config.example.toml "${out}/config.example.toml"
    install -m 0644 README.md "${out}/README.md"
    install -m 0644 LICENSE "${out}/LICENSE"
    local doc
    for doc in docs/config.md docs/win7.md docs/webdav.md docs/testing.md; do
        if [ -f "${doc}" ]; then
            install -m 0644 "${doc}" "${out}/docs/"
        fi
    done

    if [ "${VERIFY}" -eq 1 ]; then
        scripts/verify-win7-pe.sh "${out}/${EXE_NAME}"
    fi

    echo "==> packaged ${out}"
}

require_file Cargo.toml
require_file assets/config.example.toml
require_file README.md
require_file LICENSE

mkdir -p "${DIST}"
if [ "${BUILD}" -eq 1 ]; then
    echo "==> building vibekey ${VERSION} for Windows 7 (x86_64 + i686)"
fi

package_variant win7-x64 x86_64-win7-windows-gnu
package_variant win7-win32 i686-win7-windows-gnu

# Checksums of the shipped executables, for release notes / verifying a
# download.
(
    cd "${DIST}"
    : > SHA256SUMS
    for dir in vibekey-"${VERSION}"-win7-*; do
        if [ -f "${dir}/${EXE_NAME}" ]; then
            sha256sum "${dir}/${EXE_NAME}" >> SHA256SUMS
        fi
    done
)

echo
echo "==> release ${VERSION} ready:"
find "${DIST}" -maxdepth 2 -type f | sort
echo
echo "Note: dist/ contains the Windows 7 SP1 builds (GNU/MSVCRT, runs on Win7+)."
echo "      Windows 10+ users can also use a stable MSVC build (see README.md)."
