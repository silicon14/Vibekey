#!/usr/bin/env bash
# Verify that a Windows PE executable can run on a stock Windows 7 SP1.
#
# Usage:
#   scripts/verify-win7-pe.sh [options] <exe-or-directory> [...]
#
# Options:
#   -h, --help    show this help
#   -q, --quiet   only print one PASS/FAIL line per executable
#
# Checks for every executable:
#   1. PE32/PE32+ header: MajorSubsystemVersion.MinorSubsystemVersion <= 6.1,
#      i.e. the kernel loader is not asked for a Windows 8+ subsystem.
#   2. Import table: every imported DLL ships with Windows 7 SP1 (KERNEL32,
#      USER32, ..., msvcrt), and no runtime library that is missing on a stock
#      Win7 install is linked: api-ms-win-*, ucrtbase.dll, vcruntime*.dll,
#      msvcp*.dll, libunwind.dll, libgcc_s_*.dll, libwinpthread-1.dll.
#   3. Imported function names: no function that only exists on Windows 8 or
#      newer (GetSystemTimePreciseAsFileTime, SetThreadDescription, ...).
#
# A directory argument is searched recursively (up to three levels) for *.exe
# files, so
#   scripts/verify-win7-pe.sh dist/
# verifies a complete release tree (dist/vibekey-<version>-win7-*/vibekey.exe).
#
# The script needs llvm-readobj from llvm-mingw (the same toolchain that
# scripts/install-toolchain.sh installs). Resolution order:
#   $LLVM_READOBJ, llvm-readobj in $PATH, $VIBEKEY_TOOLCHAIN_DIR/llvm-mingw-*/bin.
#
# Exit status: 0 = all checks passed, 1 = at least one check failed,
#              2 = usage or tooling error.
set -uo pipefail

TOOLCHAIN_DIR="${VIBEKEY_TOOLCHAIN_DIR:-$HOME/toolchains}"

# DLLs that are part of a stock Windows 7 SP1 installation (or of msvcrt).
# Importing anything else makes the executable fail to start on a clean Win7.
ALLOWED_DLLS=(
    kernel32 user32 gdi32 shell32 advapi32 msvcrt ws2_32 bcrypt ntdll userenv
    ole32 oleaut32 comctl32 comdlg32 shlwapi crypt32 imm32 uxtheme version
    winmm psapi iphlpapi secur32 dnsapi rpcrt4 wintrust cabinet dbghelp
)

# Functions that were introduced after Windows 7. A static import of one of
# these makes the process fail to start on Win7 ("entry point not found").
DENIED_SYMBOLS=(
    GetSystemTimePreciseAsFileTime SetThreadDescription GetThreadDescription
    GetDpiForWindow GetDpiForSystem SetProcessDpiAwarenessContext
    SetThreadDpiAwarenessContext GetDpiForMonitor GetSystemMetricsForDpi
    AdjustWindowRectExForDpi WaitOnAddress WakeByAddressSingle WakeByAddressAll
    InitializeSynchronizationBarrier EnterSynchronizationBarrier
    DeleteSynchronizationBarrier GetFirmwareType RoInitialize RoUninitialize
)

QUIET=0
FAILED_CHECKS=0
FAILED_EXES=0
CHECKED=0

usage() {
    cat <<'EOF'
Verify that a Windows PE executable can run on a stock Windows 7 SP1.

Usage:
  scripts/verify-win7-pe.sh [options] <exe-or-directory> [...]

Options:
  -h, --help    show this help
  -q, --quiet   only print one PASS/FAIL line per executable

A directory argument is searched recursively (up to three levels) for *.exe
files, so "scripts/verify-win7-pe.sh dist/" verifies a release tree.

Exit status: 0 = passed, 1 = at least one check failed, 2 = usage/tool error.
EOF
}

if [ -t 1 ]; then
    RED=$'\033[31m'; GREEN=$'\033[32m'; YELLOW=$'\033[33m'; BOLD=$'\033[1m'; RESET=$'\033[0m'
else
    RED=''; GREEN=''; YELLOW=''; BOLD=''; RESET=''
fi

say() { [ "${QUIET}" -eq 1 ] || printf '%s\n' "$*"; }
ok() { [ "${QUIET}" -eq 1 ] || printf '  %s[ OK ]%s %s\n' "${GREEN}" "${RESET}" "$*"; }
warn() { [ "${QUIET}" -eq 1 ] || printf '  %s[WARN]%s %s\n' "${YELLOW}" "${RESET}" "$*" >&2; }
bad() { printf '  %s[FAIL]%s %s\n' "${RED}" "${RESET}" "$*" >&2; FAILED_CHECKS=$((FAILED_CHECKS + 1)); }

find_llvm_readobj() {
    if [ -n "${LLVM_READOBJ:-}" ]; then
        if [ -x "${LLVM_READOBJ}" ]; then
            printf '%s\n' "${LLVM_READOBJ}"
            return 0
        fi
        printf 'error: LLVM_READOBJ=%s is not an executable\n' "${LLVM_READOBJ}" >&2
        return 1
    fi
    if command -v llvm-readobj >/dev/null 2>&1; then
        command -v llvm-readobj
        return 0
    fi
    local dir
    for dir in "${TOOLCHAIN_DIR}"/llvm-mingw-*/bin; do
        if [ -x "${dir}/llvm-readobj" ]; then
            printf '%s\n' "${dir}/llvm-readobj"
            return 0
        fi
    done
    return 1
}

is_allowed_dll() {
    local dll="${1,,}" entry
    dll="${dll%.dll}"
    for entry in "${ALLOWED_DLLS[@]}"; do
        [ "${dll}" = "${entry}" ] && return 0
    done
    return 1
}

# Prints the reason a DLL is rejected on Win7; returns 1 when it is not on the
# deny list (the caller still has to check the allow list).
deny_reason() {
    case "${1,,}" in
        api-ms-win-*|ext-ms-win-*)
            printf 'API-set stub DLL, only provided by the Windows 10 UCRT' ;;
        ucrtbase.dll|ucrtbased.dll)
            printf 'Universal C Runtime, not part of a stock Windows 7' ;;
        vcruntime*.dll|msvcp*.dll|concrt*.dll|vccorlib*.dll)
            printf 'Visual C++ redistributable runtime, not part of Windows 7' ;;
        libunwind.dll|libgcc_s_*.dll|libwinpthread-1.dll)
            printf 'toolchain runtime DLL that is not shipped with Windows' ;;
        *)
            return 1 ;;
    esac
}

normalize_symbol() {
    local sym="$1"
    sym="${sym#_}"     # i686 leading underscore
    sym="${sym%%@*}"   # stdcall decoration "_Name@8"
    printf '%s\n' "${sym}"
}

is_denied_symbol() {
    local sym entry
    sym="$(normalize_symbol "$1")"
    [ "${sym}" = "???" ] && return 1
    for entry in "${DENIED_SYMBOLS[@]}"; do
        [ "${sym}" = "${entry}" ] && return 0
    done
    return 1
}

verify_file() {
    local exe="$1" headers imports arch major minor os_major os_minor subsystem
    local -a dlls symbols
    local dll sym reason checks_before=${FAILED_CHECKS} symbol_failures=0
    local denied=0

    CHECKED=$((CHECKED + 1))
    say ""
    printf '%s==> %s%s\n' "${BOLD}" "${exe}" "${RESET}"

    if [ ! -f "${exe}" ]; then
        bad "file not found"
    elif ! headers="$("${READOBJ}" --file-headers "${exe}" 2>&1)"; then
        bad "llvm-readobj --file-headers failed: ${headers}"
    elif ! imports="$("${READOBJ}" --coff-imports "${exe}" 2>&1)"; then
        bad "llvm-readobj --coff-imports failed: ${imports}"
    else
        arch="$(awk '/^Arch: /{print $2; exit}' <<<"${headers}")"
        major="$(awk '/MajorSubsystemVersion:/{print $2; exit}' <<<"${headers}")"
        minor="$(awk '/MinorSubsystemVersion:/{print $2; exit}' <<<"${headers}")"
        os_major="$(awk '/MajorOperatingSystemVersion:/{print $2; exit}' <<<"${headers}")"
        os_minor="$(awk '/MinorOperatingSystemVersion:/{print $2; exit}' <<<"${headers}")"
        subsystem="$(awk '/^  Subsystem: /{print $2; exit}' <<<"${headers}")"

        say "  arch: ${arch:-?}  subsystem: ${subsystem:-?}  OS version field: ${os_major:-?}.${os_minor:-0}"

        if ! [[ "${major}" =~ ^[0-9]+$ && "${minor}" =~ ^[0-9]+$ ]]; then
            bad "cannot read MajorSubsystemVersion/MinorSubsystemVersion from the PE header"
        elif (( major > 6 || (major == 6 && minor > 1) )); then
            bad "subsystem version ${major}.${minor} > 6.1: Windows 7 refuses to load this binary"
        else
            ok "subsystem version ${major}.${minor} <= 6.1"
        fi

        case "${subsystem}" in
            IMAGE_SUBSYSTEM_WINDOWS_GUI)
                ok "GUI subsystem" ;;
            IMAGE_SUBSYSTEM_WINDOWS_CUI)
                warn "console subsystem: release builds of Vibekey should use the GUI subsystem so no console window is shown" ;;
            "")
                bad "cannot read the Subsystem field" ;;
            *)
                warn "unusual subsystem ${subsystem} (expected IMAGE_SUBSYSTEM_WINDOWS_GUI)" ;;
        esac

        if [[ "${os_major}" =~ ^[0-9]+$ ]] && (( os_major > 6 )); then
            warn "MajorOperatingSystemVersion=${os_major}.${os_minor:-0} is newer than 6.1 (informational; the loader enforces the subsystem version)"
        fi

        mapfile -t dlls < <(awk '/Name: /{print $2}' <<<"${imports}")
        mapfile -t symbols < <(awk '/Symbol: /{print $2}' <<<"${imports}")

        if [ "${#dlls[@]}" -eq 0 ]; then
            bad "no imported DLLs found: not a normal Windows executable"
        else
            for dll in "${dlls[@]}"; do
                if reason="$(deny_reason "${dll}")"; then
                    bad "forbidden import ${dll}: ${reason}"
                    denied=$((denied + 1))
                elif ! is_allowed_dll "${dll}"; then
                    bad "import ${dll} is not on the Windows 7 DLL whitelist (if it really ships with Win7, add it to ALLOWED_DLLS)"
                    denied=$((denied + 1))
                fi
            done
            if [ "${denied}" -eq 0 ]; then
                ok "${#dlls[@]} imported DLLs, all part of Windows 7: ${dlls[*]}"
            fi

            for sym in "${symbols[@]}"; do
                if is_denied_symbol "${sym}"; then
                    bad "imported function ${sym} only exists on Windows 8 or newer"
                    symbol_failures=$((symbol_failures + 1))
                fi
            done
            if [ "${symbol_failures}" -eq 0 ]; then
                ok "no Windows 8+ only function imports"
            fi
        fi
    fi

    if [ "${FAILED_CHECKS}" -gt "${checks_before}" ]; then
        FAILED_EXES=$((FAILED_EXES + 1))
        [ "${QUIET}" -eq 1 ] && printf '%sFAIL%s %s\n' "${RED}" "${RESET}" "${exe}"
    else
        [ "${QUIET}" -eq 1 ] && printf '%sPASS%s %s\n' "${GREEN}" "${RESET}" "${exe}"
    fi
    return 0
}

READOBJ="$(find_llvm_readobj)" || {
    printf 'error: llvm-readobj not found.\n' >&2
    printf 'Install the toolchain with scripts/install-toolchain.sh, or set LLVM_READOBJ.\n' >&2
    exit 2
}

if [ "$#" -eq 0 ]; then
    usage >&2
    exit 2
fi

TARGETS=()
for arg in "$@"; do
    case "${arg}" in
        -h|--help)
            usage
            exit 0 ;;
        -q|--quiet)
            QUIET=1 ;;
        -*)
            printf 'error: unknown option: %s\n' "${arg}" >&2
            usage >&2
            exit 2 ;;
        *)
            if [ -d "${arg}" ]; then
                found=0
                while IFS= read -r -d '' file; do
                    TARGETS+=("${file}")
                    found=1
                done < <(find "${arg}" -maxdepth 3 -type f -iname '*.exe' -print0 | sort -z)
                if [ "${found}" -eq 0 ]; then
                    printf 'error: no .exe files in directory: %s\n' "${arg}" >&2
                    exit 2
                fi
            else
                TARGETS+=("${arg}")
            fi ;;
    esac
done

if [ "${#TARGETS[@]}" -eq 0 ]; then
    printf 'error: no executable to verify\n' >&2
    exit 2
fi

say "llvm-readobj: ${READOBJ}"
say "Verifying ${#TARGETS[@]} executable(s) for Windows 7 SP1 compatibility..."

for exe in "${TARGETS[@]}"; do
    verify_file "${exe}"
done

if [ "${QUIET}" -eq 0 ]; then
    printf '\n'
fi
if [ "${FAILED_EXES}" -eq 0 ]; then
    printf '%sPASS%s: %d executable(s) are Windows 7 SP1 compatible.\n' "${GREEN}" "${RESET}" "${CHECKED}"
    exit 0
else
    printf '%sFAIL%s: %d of %d executable(s) failed the Windows 7 checks.\n' "${RED}" "${RESET}" "${FAILED_EXES}" "${CHECKED}"
    exit 1
fi
