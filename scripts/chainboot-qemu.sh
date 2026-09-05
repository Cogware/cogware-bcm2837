#!/usr/bin/env bash
#
# chainboot-qemu.sh -- boot the chain-loader under QEMU and push a payload in.
#
# Emulates the hardware debug loop: build a payload, start the loader, push,
# watch it boot. Use to test loader/protocol changes without a board.
#
#     ./scripts/chainboot-qemu.sh
#     ./scripts/chainboot-qemu.sh --payload multicore
#     ./scripts/chainboot-qemu.sh --payload ./kernel8.img --run-seconds 15
#
# QEMU's serial socket uses `wait=on`: with `wait=off` the guest starts
# immediately, and output before the pusher connects (including the 0x03
# request token) is lost, so the push times out.
#
# This script must not connect to probe the port: with wait=on, the first
# connection releases the guest. It checks for a listening socket with `ss`
# instead.
#
# CAVEAT: passing here proves nothing about cache maintenance. QEMU treats
# set/way ops as no-ops and has no store buffer to leave dirty, so a handover
# that corrupts itself on real silicon can pass cleanly. See
# docs/22-chainloading.md.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd)"
CRATE_MANIFEST="$REPO_ROOT/cogware-bcm2837/Cargo.toml"
LOADER="$REPO_ROOT/target/aarch64-unknown-none/release/examples/chainloader"

PAYLOAD="hello-uart"
PORT=4444
RUN_SECONDS=8
LOG_PATH=""
PAD_TO_MAX=false
EXPECT=""

usage() {
    cat <<EOF
Usage: scripts/chainboot-qemu.sh [options]

Boots the chain-loader in QEMU and pushes a payload into it over TCP.

Options:
  --payload <name|path>  Example name to build, or an existing flat image
                         (default: $PAYLOAD)
  --port <n>             TCP port for the emulated PL011 (default: $PORT)
  --run-seconds <n>      How long to capture the payload's output (default: $RUN_SECONDS)
  --log <path>           Write the whole transcript here as well
  --pad-to-max           Pad a private payload copy to the supported 8 MiB limit
  --expect <text>        Fail unless this literal target output is captured
  -h, --help             This message
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        -h|--help) usage; exit 0 ;;
        --payload)     [[ $# -ge 2 ]] || { echo "chainboot-qemu: --payload needs a value" >&2; exit 2; }
                       PAYLOAD="$2"; shift 2 ;;
        --port)        [[ $# -ge 2 ]] || { echo "chainboot-qemu: --port needs a value" >&2; exit 2; }
                       PORT="$2"; shift 2 ;;
        --run-seconds) [[ $# -ge 2 ]] || { echo "chainboot-qemu: --run-seconds needs a value" >&2; exit 2; }
                       RUN_SECONDS="$2"; shift 2 ;;
        --log)         [[ $# -ge 2 ]] || { echo "chainboot-qemu: --log needs a value" >&2; exit 2; }
                       LOG_PATH="$2"; shift 2 ;;
        --pad-to-max)  PAD_TO_MAX=true; shift ;;
        --expect)      [[ $# -ge 2 ]] || { echo "chainboot-qemu: --expect needs a value" >&2; exit 2; }
                       EXPECT="$2"; shift 2 ;;
        *) echo "chainboot-qemu: unknown option '$1'" >&2; usage >&2; exit 2 ;;
    esac
done

cd "$REPO_ROOT"

# --- payload: an existing image, or an example name to build ---------------
if [[ -f "$PAYLOAD" ]]; then
    IMAGE="$(cd -- "$(dirname -- "$PAYLOAD")" && pwd)/$(basename -- "$PAYLOAD")"
    echo "==> payload: $IMAGE (pre-built)" >&2
else
    # chainboot.sh --build-only prints the image path on stdout, progress on
    # stderr; reusing it keeps feature resolution and objcopy discovery in
    # one place.
    IMAGE="$("$SCRIPT_DIR/chainboot.sh" "$PAYLOAD" --build-only)"
fi

# --- the loader itself ------------------------------------------------------
echo "==> building the chain-loader" >&2
cargo build --manifest-path "$CRATE_MANIFEST" --release --example chainloader \
    --features panic-handler,allocator
[[ -f "$LOADER" ]] || { echo "chainboot-qemu: loader not built at $LOADER" >&2; exit 1; }

# --- QEMU -------------------------------------------------------------------
# The FIRST -serial is the PL011 the kernel prints on; the second is the AUX
# mini UART and is discarded. Never pass -smp: raspi3b fixes it at 4.
QEMU_PID=""
PADDED_IMAGE=""
cleanup() {
    if [[ -n "$QEMU_PID" ]] && kill -0 "$QEMU_PID" 2>/dev/null; then
        kill "$QEMU_PID" 2>/dev/null || true
        wait "$QEMU_PID" 2>/dev/null || true
    fi
    if [[ -n "$PADDED_IMAGE" ]]; then
        rm -f -- "$PADDED_IMAGE"
    fi
}
trap cleanup EXIT

if "$PAD_TO_MAX"; then
    padded_candidate="$REPO_ROOT/target/chainboot-padded-$$-$PORT.img"
    python3 - "$IMAGE" "$padded_candidate" <<'PY'
import pathlib
import sys
data = pathlib.Path(sys.argv[1]).read_bytes()
limit = 8 * 1024 * 1024
if len(data) > limit:
    raise SystemExit("chainboot-qemu: payload exceeds the 8 MiB loader limit")
with open(sys.argv[2], "xb") as output:
    output.write(data)
    output.truncate(limit)
PY
    PADDED_IMAGE="$padded_candidate"
    IMAGE="$PADDED_IMAGE"
fi
if { "$PAD_TO_MAX" || [[ -n "$EXPECT" ]]; } && [[ -z "$LOG_PATH" ]]; then
    LOG_PATH="$REPO_ROOT/target/chainboot-qemu-$PORT.log"
fi

echo "==> starting QEMU (PL011 on tcp:$PORT, waiting for the pusher)" >&2
qemu-system-aarch64 \
    -M raspi3b \
    -display none \
    -serial "tcp:127.0.0.1:$PORT,server=on,wait=on" \
    -serial null \
    -kernel "$LOADER" &
QEMU_PID=$!

# Must not connect to check (that would start the guest); poll for a
# listening socket instead.
if command -v ss >/dev/null 2>&1; then
    for _ in $(seq 1 50); do
        if ss -ltnH "sport = :$PORT" 2>/dev/null | grep -q .; then break; fi
        sleep 0.2
    done
else
    sleep 2
fi

declare -a PUSH_ARGS=(
    --payload "$IMAGE"
    --tcp "localhost:$PORT"
    --no-terminal
    --run-seconds "$RUN_SECONDS"
)
if [[ -n "$LOG_PATH" ]]; then
    PUSH_ARGS+=(--log "$LOG_PATH")
fi

python3 "$SCRIPT_DIR/minipush.py" "${PUSH_ARGS[@]}"

if "$PAD_TO_MAX" && ! grep -Eq 'received[[:space:]]*:[[:space:]]*8388608' "$LOG_PATH"; then
    echo "chainboot-qemu: loader did not receive the complete 8 MiB regression payload" >&2
    exit 1
fi
if [[ -n "$EXPECT" ]] && ! grep -Fq -- "$EXPECT" "$LOG_PATH"; then
    echo "chainboot-qemu: expected target output was not observed: $EXPECT" >&2
    exit 1
fi