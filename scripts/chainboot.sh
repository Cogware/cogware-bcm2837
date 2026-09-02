#!/usr/bin/env bash
#
# chainboot.sh -- build an example and chain-load it onto the board over UART.
#
# Compiles the current sources, converts the ELF to the flat binary the
# loader expects, and pushes it to the loader already running on the Pi --
# no SD card cycle, no manual objcopy.
#
#     ./scripts/chainboot.sh                  # default example (shell)
#     ./scripts/chainboot.sh multicore        # a specific example
#     ./scripts/chainboot.sh hello-uart --build-only
#     ./scripts/chainboot.sh shell --port /dev/ttyUSB0
#
# ORDER MATTERS: start this script first, then power-cycle the Pi. The loader
# sends three 0x03 bytes on boot; if nothing is listening then, the push
# waits forever.
#
# The push needs exclusive access to the serial port, so this script refuses
# to start if it is already held open (e.g. by a detached `screen`).

set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
CRATE_MANIFEST="$REPO_ROOT/cogware-bcm2837/Cargo.toml"
MINIPUSH="$REPO_ROOT/scripts/minipush.py"
TARGET_TRIPLE="aarch64-unknown-none"
DEFAULT_EXAMPLE="shell"

# --------------------------------------------------------------------------
# Cargo manifest queries.
#
# Required features vary per example (see Cargo.toml); read them at run
# time rather than hardcoding a list that would rot.
# --------------------------------------------------------------------------

examples_list() {
    python3 - "$CRATE_MANIFEST" <<'PY'
import re, sys
src = open(sys.argv[1]).read()
for block in src.split('[[example]]')[1:]:
    m = re.search(r'name\s*=\s*"([^"]+)"', block)
    if m:
        print(m.group(1))
PY
}

required_features() {
    python3 - "$CRATE_MANIFEST" "$1" <<'PY'
import re, sys
src, want = open(sys.argv[1]).read(), sys.argv[2]
for block in src.split('[[example]]')[1:]:
    m = re.search(r'name\s*=\s*"([^"]+)"', block)
    if not m or m.group(1) != want:
        continue
    feats = re.search(r'required-features\s*=\s*\[([^\]]*)\]', block)
    print(','.join(re.findall(r'"([^"]+)"', feats.group(1))) if feats else '')
    sys.exit(0)
sys.exit(1)
PY
}

# Resolves the port before building, so a busy port is reported early;
# minipush.py still owns the real default.
detect_port() {
    local candidate
    for candidate in /dev/serial/by-id/*FTDI*; do
        if [[ -e "$candidate" ]]; then
            printf '%s' "$candidate"
            return 0
        fi
    done
    return 1
}

find_objcopy() {
    local tool
    for tool in rust-objcopy llvm-objcopy aarch64-linux-gnu-objcopy; do
        if command -v "$tool" >/dev/null 2>&1; then
            printf '%s' "$tool"
            return 0
        fi
    done
    return 1
}

usage() {
    cat <<EOF
Usage: scripts/chainboot.sh [example] [options]

Builds an example, converts it to a flat binary and chain-loads it over UART.

Arguments:
  example            Example to build and push (default: $DEFAULT_EXAMPLE)

Options:
  --list             List the examples this crate defines, then exit
  --build-only       Build and objcopy, but do not push (prints the .img path)
  --debug            Build the dev profile instead of release
  --profile <p>      release (default) or debug
  --port <dev>       Serial device (default: minipush.py's own default)
  --baud <rate>      Baud rate (default: minipush.py's own default)
  -h, --help         This message

Start the script first, then power-cycle the Pi.
EOF
}

EXAMPLE=""
PROFILE="release"
BUILD_ONLY=0
EXPLICIT_PORT=""
declare -a PUSH_ARGS=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        -h|--help)   usage; exit 0 ;;
        --list)      examples_list; exit 0 ;;
        --build-only) BUILD_ONLY=1; shift ;;
        --debug)     PROFILE="debug"; shift ;;
        --release)   PROFILE="release"; shift ;;
        --profile)
            [[ $# -ge 2 ]] || { echo "chainboot: --profile needs a value" >&2; exit 2; }
            PROFILE="$2"; shift 2 ;;
        --port)
            [[ $# -ge 2 ]] || { echo "chainboot: --port needs a value" >&2; exit 2; }
            EXPLICIT_PORT="$2"; PUSH_ARGS+=(--port "$2"); shift 2 ;;
        --baud)
            [[ $# -ge 2 ]] || { echo "chainboot: --baud needs a value" >&2; exit 2; }
            PUSH_ARGS+=(--baud "$2"); shift 2 ;;
        --) shift; PUSH_ARGS+=("$@"); break ;;
        -*) echo "chainboot: unknown option '$1'" >&2; usage >&2; exit 2 ;;
        *)
            if [[ -n "$EXAMPLE" ]]; then
                echo "chainboot: unexpected argument '$1'" >&2; exit 2
            fi
            EXAMPLE="$1"; shift ;;
    esac
done

EXAMPLE="${EXAMPLE:-$DEFAULT_EXAMPLE}"

case "$PROFILE" in
    release|debug) ;;
    *) echo "chainboot: --profile must be 'release' or 'debug'" >&2; exit 2 ;;
esac

# --- validate the example up front, with a useful error --------------------
if ! FEATURES="$(required_features "$EXAMPLE")"; then
    {
        echo "chainboot: no [[example]] named '$EXAMPLE' in the crate manifest."
        echo
        echo "Available examples:"
        examples_list | sed 's/^/  /'
    } >&2
    exit 1
fi

# --- refuse to build if the port is already owned by something else --------
if [[ "$BUILD_ONLY" -eq 0 ]]; then
    PORT_TO_CHECK="$EXPLICIT_PORT"
    if [[ -z "$PORT_TO_CHECK" ]]; then
        PORT_TO_CHECK="$(detect_port || true)"
    fi
    if [[ -n "$PORT_TO_CHECK" && -e "$PORT_TO_CHECK" ]] && command -v fuser >/dev/null 2>&1; then
        if fuser "$PORT_TO_CHECK" >/dev/null 2>&1; then
            echo "chainboot: $PORT_TO_CHECK is already open by another process:" >&2
            fuser -v "$PORT_TO_CHECK" >&2 || true
            echo >&2
            echo "Close it first (a detached screen is the usual culprit:" >&2
            echo "  screen -ls   then   screen -S <id> -X quit)" >&2
            exit 1
        fi
    fi
fi

# --- build -----------------------------------------------------------------
echo >&2 "==> building '$EXAMPLE' [$PROFILE] features: ${FEATURES:-none}"
declare -a CARGO_ARGS=(build --manifest-path "$CRATE_MANIFEST" --example "$EXAMPLE")
if [[ "$PROFILE" == "release" ]]; then
    CARGO_ARGS+=(--release)
fi
if [[ -n "$FEATURES" ]]; then
    CARGO_ARGS+=(--features "$FEATURES")
fi
cargo "${CARGO_ARGS[@]}"

ELF="$REPO_ROOT/target/$TARGET_TRIPLE/$PROFILE/examples/$EXAMPLE"
[[ -f "$ELF" ]] || { echo "chainboot: expected ELF not found at $ELF" >&2; exit 1; }

# --- flat binary -----------------------------------------------------------
# The ELF keeps its full DWARF for JTAG; objcopy -O binary emits only the
# loadable sections, which is what the chain-loader copies to 0x80000.
OBJCOPY="$(find_objcopy)" || {
    echo "chainboot: no objcopy found (tried rust-objcopy, llvm-objcopy, aarch64-linux-gnu-objcopy)" >&2
    exit 1
}
IMG="$REPO_ROOT/target/$TARGET_TRIPLE/$PROFILE/examples/$EXAMPLE.img"
"$OBJCOPY" -O binary "$ELF" "$IMG"

IMG_SIZE="$(stat -c %s "$IMG")"
echo >&2 "==> $EXAMPLE.img  ${IMG_SIZE} bytes  (via $OBJCOPY)"

if [[ "$BUILD_ONLY" -eq 1 ]]; then
    echo "$IMG"
    exit 0
fi

# --- push ------------------------------------------------------------------
[[ -f "$MINIPUSH" ]] || { echo "chainboot: $MINIPUSH not found" >&2; exit 1; }
echo >&2 "==> pushing. Power-cycle the Pi now. Ctrl-] exits the terminal."
exec python3 "$MINIPUSH" --payload "$IMG" ${PUSH_ARGS[@]+"${PUSH_ARGS[@]}"}