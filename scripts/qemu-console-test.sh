#!/usr/bin/env bash
#
# qemu-console-test.sh -- boot the kernel in QEMU and drive its console
# programmatically.
#
# The Linux counterpart of scripts/qemu-console-test.ps1: drives the kernel
# console over a unix socket, opened with wait=on so the guest cannot run (or
# print) before this script attaches.
#
# Everything the kernel prints is echoed here and captured to --log-path.
#
#     ./scripts/qemu-console-test.sh --commands "h m s t c e"
#     ./scripts/qemu-console-test.sh --commands "d" --log-path abort.log
#     ./scripts/qemu-console-test.sh --commands "f g" --screenshot card.ppm
#
# Exits non-zero if the console could not be driven, or if a requested
# screendump was not produced, so it is safe to use in a gate.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd)"

EXAMPLE="shell"
PROFILE="release"
SETTLE_MS=1200
TIMEOUT_SEC=60
LOG_PATH="console-test.log"
SD_IMAGE=""
SD_SIZE_MB=64
SCREENSHOT=""
MONITOR_PORT=55555
BUS_DEVICES=0
declare -a COMMANDS=()

usage() {
    cat <<EOF
Usage: scripts/qemu-console-test.sh [example] [options]

Boots an example in QEMU and sends console keystrokes once boot goes quiet.

Arguments:
  example              Example kernel to boot (default: $EXAMPLE)

Options:
  --commands <list>    Space-separated keystrokes to send, in order
                       e.g. --commands "h m s t c e"
  --settle-ms <n>      Silence required before the next command (default: $SETTLE_MS)
  --timeout-sec <n>    Hard limit on the whole run (default: $TIMEOUT_SEC)
  --log-path <path>    Where to write the transcript (default: $LOG_PATH)
  --profile <p>        release (default) or debug
  --example <name>     Same as the positional argument
  --sd-image <path>    Raw disk image to attach as the SD card
  --sd-size-mb <n>     Size of an SD image this script creates (default: $SD_SIZE_MB)
  --screenshot <path>  Write a PPM screendump once all commands have been sent
  --monitor-port <n>   Loopback port for the monitor socket (default: $MONITOR_PORT)
  --bus-devices        Attach the virtual I2C/SPI devices
  -h, --help           This message

The example must already be built; this script does not build for you.
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        -h|--help) usage; exit 0 ;;
        --commands)     [[ $# -ge 2 ]] || { echo "console-test: --commands needs a value" >&2; exit 2; }
                        read -r -a COMMANDS <<<"$2"; shift 2 ;;
        --settle-ms)    [[ $# -ge 2 ]] || { echo "console-test: --settle-ms needs a value" >&2; exit 2; }
                        SETTLE_MS="$2"; shift 2 ;;
        --timeout-sec)  [[ $# -ge 2 ]] || { echo "console-test: --timeout-sec needs a value" >&2; exit 2; }
                        TIMEOUT_SEC="$2"; shift 2 ;;
        --log-path)     [[ $# -ge 2 ]] || { echo "console-test: --log-path needs a value" >&2; exit 2; }
                        LOG_PATH="$2"; shift 2 ;;
        --profile)      [[ $# -ge 2 ]] || { echo "console-test: --profile needs a value" >&2; exit 2; }
                        PROFILE="$2"; shift 2 ;;
        --example)      [[ $# -ge 2 ]] || { echo "console-test: --example needs a value" >&2; exit 2; }
                        EXAMPLE="$2"; shift 2 ;;
        --sd-image)     [[ $# -ge 2 ]] || { echo "console-test: --sd-image needs a value" >&2; exit 2; }
                        SD_IMAGE="$2"; shift 2 ;;
        --sd-size-mb)   [[ $# -ge 2 ]] || { echo "console-test: --sd-size-mb needs a value" >&2; exit 2; }
                        SD_SIZE_MB="$2"; shift 2 ;;
        --screenshot)   [[ $# -ge 2 ]] || { echo "console-test: --screenshot needs a value" >&2; exit 2; }
                        SCREENSHOT="$2"; shift 2 ;;
        --monitor-port) [[ $# -ge 2 ]] || { echo "console-test: --monitor-port needs a value" >&2; exit 2; }
                        MONITOR_PORT="$2"; shift 2 ;;
        --bus-devices)  BUS_DEVICES=1; shift ;;
        -*) echo "console-test: unknown option '$1'" >&2; usage >&2; exit 2 ;;
        *)  EXAMPLE="$1"; shift ;;
    esac
done

cd "$REPO_ROOT"

KERNEL="$REPO_ROOT/target/aarch64-unknown-none/$PROFILE/examples/$EXAMPLE"
if [[ ! -f "$KERNEL" ]]; then
    echo "console-test: example '$EXAMPLE' not found at $KERNEL" >&2
    echo "              build it first: cargo build --$PROFILE --example $EXAMPLE" >&2
    exit 1
fi

[[ "$SCREENSHOT" == "" || "$SCREENSHOT" == /* ]] || SCREENSHOT="$REPO_ROOT/$SCREENSHOT"
[[ "$LOG_PATH" == /* ]] || LOG_PATH="$REPO_ROOT/$LOG_PATH"

SOCK="${TMPDIR:-/tmp}/cogware-console-$$.sock"

# wait=on: QEMU holds the CPU until this script attaches, so no boot output
# is missed.
declare -a QEMU_ARGS=(
    -M raspi3b
    -serial "unix:$SOCK,server=on,wait=on"
    -serial null
    -display none
    -kernel "$KERNEL"
)

# QEMU 9.0+ models SPI0 and the BSC controllers for real, so virtual
# peripherals can attach. Needed by the bus examples.
if [[ "$EXAMPLE" == "buses" || "$EXAMPLE" == "async-bus" || "$BUS_DEVICES" -eq 1 ]]; then
    QEMU_ARGS+=(
        -device ds1338,address=0x68,bus=i2c-bus.1
        -device tmp105,address=0x49,bus=i2c-bus.1
        -device sst25vf016b,bus=spi
    )
fi

if [[ -n "$SCREENSHOT" ]]; then
    # QEMU 11+ requires `-object monitor-hmp`; `-monitor`/`-mon` produce a
    # monitor that never answers.
    QEMU_ARGS+=(
        -chardev "socket,id=mon0,host=127.0.0.1,port=$MONITOR_PORT,server=on,wait=off"
        -object monitor-hmp,id=hmp0,chardev=mon0
    )
fi

if [[ -n "$SD_IMAGE" ]]; then
    [[ "$SD_IMAGE" == /* ]] || SD_IMAGE="$REPO_ROOT/$SD_IMAGE"
    if [[ ! -f "$SD_IMAGE" ]]; then
        qemu-img create -f raw "$SD_IMAGE" "${SD_SIZE_MB}M" >/dev/null
    fi
    QEMU_ARGS+=(-drive "file=$SD_IMAGE,if=sd,format=raw")
fi

QEMU_PID=""
cleanup() {
    if [[ -n "$QEMU_PID" ]] && kill -0 "$QEMU_PID" 2>/dev/null; then
        kill "$QEMU_PID" 2>/dev/null || true
        wait "$QEMU_PID" 2>/dev/null || true
    fi
    rm -f "$SOCK"
}
trap cleanup EXIT

qemu-system-aarch64 "${QEMU_ARGS[@]}" &
QEMU_PID=$!

rc=0
python3 - "$SOCK" "$SETTLE_MS" "$TIMEOUT_SEC" "$LOG_PATH" "$SCREENSHOT" "$MONITOR_PORT" "${COMMANDS[@]+"${COMMANDS[@]}"}" <<'PY' || rc=$?
import os, select, socket, sys, time

sock_path = sys.argv[1]
settle = int(sys.argv[2]) / 1000.0
timeout = float(sys.argv[3])
log_path = sys.argv[4]
screenshot = sys.argv[5]
monitor_port = int(sys.argv[6])
commands = list(sys.argv[7:])

sock = None
for _ in range(100):
    try:
        candidate = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        candidate.connect(sock_path)
        sock = candidate
        break
    except OSError:
        time.sleep(0.1)
if sock is None:
    print(f"console-test: could not connect to {sock_path}", file=sys.stderr)
    raise SystemExit(1)
sock.setblocking(False)

fd = sock.fileno()
transcript = bytearray()
deadline = time.monotonic() + timeout
last_output = time.monotonic()

while time.monotonic() < deadline:
    readable, _, _ = select.select([fd], [], [], 0.2)
    if readable:
        data = os.read(fd, 4096)
        if not data:
            break
        transcript += data
        sys.stdout.buffer.write(data)
        sys.stdout.buffer.flush()
        last_output = time.monotonic()
        continue

    idle = time.monotonic() - last_output
    if commands and idle >= settle:
        command = commands.pop(0)
        note = f"\n<<< sending '{command}' >>>\n"
        transcript += note.encode()
        sys.stdout.write(note)
        sys.stdout.flush()
        os.write(fd, command.encode())
        last_output = time.monotonic()
    elif not commands and idle >= 3.0:
        break

if screenshot:
    # HMP is a plain text protocol: connect, let the banner arrive, write the
    # command, then give QEMU a moment to walk the framebuffer.
    try:
        mon = socket.create_connection(("127.0.0.1", monitor_port), 5)
        time.sleep(0.5)
        mon.sendall(f"screendump {screenshot}\n".encode())
        time.sleep(2.0)
        mon.close()
    except OSError as exc:
        print(f"console-test: monitor connection failed: {exc}", file=sys.stderr)

with open(log_path, "wb") as handle:
    handle.write(transcript)

sock.close()
PY

echo
echo "transcript -> $LOG_PATH"
if [[ -n "$SCREENSHOT" ]]; then
    if [[ -f "$SCREENSHOT" ]]; then
        echo "screendump -> $SCREENSHOT ($(stat -c %s "$SCREENSHOT") bytes)"
    else
        echo "screendump FAILED: no file at $SCREENSHOT" >&2
        rc=1
    fi
fi

exit "$rc"