#!/usr/bin/env bash
#
# run-qemu.sh -- boot a cogware-bcm2837 example in QEMU.
#
# The Linux counterpart of scripts/run-qemu.ps1. The kernel console
# (PL011/UART0) is attached to this terminal for boot output and keystrokes.
#
# Ctrl-A then X quits QEMU.
#
#     ./scripts/run-qemu.sh
#     ./scripts/run-qemu.sh hello-uart
#     ./scripts/run-qemu.sh display --display
#     ./scripts/run-qemu.sh --gdb
#     ./scripts/run-qemu.sh --display --sd-image ./sd.img
#     ./scripts/run-qemu.sh --auto-draw --sd-image ./sd.img

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd)"
CRATE_MANIFEST="$REPO_ROOT/cogware-bcm2837/Cargo.toml"

EXAMPLE="shell"
PROFILE="release"
GDB=0
TRACE=0
NO_BUILD=0
DISPLAY_WINDOW=0
SD_IMAGE=""
SD_SIZE_MB=64
AUTO_DRAW=0
MONITOR_PORT=0
BUS_DEVICES=0
declare -a STARTUP=(f g)

usage() {
    cat <<EOF
Usage: scripts/run-qemu.sh [example] [options]

Boots a cogware-bcm2837 example in QEMU with the PL011 on this terminal.

Arguments:
  example              Example kernel to boot (default: $EXAMPLE)

Options:
  --example <name>     Same as the positional argument
  --profile <p>        release (default) or debug
  --no-build           Boot whatever is already built
  --gdb                GDB stub on :1234, CPU halted before the first
                       instruction. Attach with:
                         aarch64-none-elf-gdb <elf> -ex "target remote :1234"
  --trace              Log taken exceptions and interrupts to int.log
  --display            Open a window showing the VideoCore framebuffer
  --sd-image <path>    Raw disk image to attach as the SD card, created at
                       --sd-size-mb if missing
  --sd-size-mb <n>     Size of an SD image this script creates (default: $SD_SIZE_MB)
  --auto-draw          Send --startup commands once boot goes quiet, then hand
                       the keyboard back. Implies --display
  --startup <cmds>     Commands --auto-draw sends (default: "${STARTUP[*]}")
  --monitor-port <n>   With --auto-draw, expose the QEMU monitor on this
                       loopback port. 0 (default) leaves it off
  --bus-devices        Attach the virtual I2C/SPI devices even if the example
                       is not one of the bus demos
  -h, --help           This message

Quit with Ctrl-A then X (or Ctrl-C with --auto-draw).
EOF
}

examples_list() {
    python3 - "$CRATE_MANIFEST" <<'PY'
import re, sys
for block in open(sys.argv[1]).read().split('[[example]]')[1:]:
    m = re.search(r'name\s*=\s*"([^"]+)"', block)
    if m:
        print(m.group(1))
PY
}

# required-features vary per example (see Cargo.toml); read at run time.
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

while [[ $# -gt 0 ]]; do
    case "$1" in
        -h|--help) usage; exit 0 ;;
        --example)      [[ $# -ge 2 ]] || { echo "run-qemu: --example needs a value" >&2; exit 2; }
                        EXAMPLE="$2"; shift 2 ;;
        --profile)      [[ $# -ge 2 ]] || { echo "run-qemu: --profile needs a value" >&2; exit 2; }
                        PROFILE="$2"; shift 2 ;;
        --no-build)     NO_BUILD=1; shift ;;
        --gdb)          GDB=1; shift ;;
        --trace)        TRACE=1; shift ;;
        --display)      DISPLAY_WINDOW=1; shift ;;
        --sd-image)     [[ $# -ge 2 ]] || { echo "run-qemu: --sd-image needs a value" >&2; exit 2; }
                        SD_IMAGE="$2"; shift 2 ;;
        --sd-size-mb)   [[ $# -ge 2 ]] || { echo "run-qemu: --sd-size-mb needs a value" >&2; exit 2; }
                        SD_SIZE_MB="$2"; shift 2 ;;
        --auto-draw)    AUTO_DRAW=1; shift ;;
        --startup)      [[ $# -ge 2 ]] || { echo "run-qemu: --startup needs a value" >&2; exit 2; }
                        read -r -a STARTUP <<<"$2"; shift 2 ;;
        --monitor-port) [[ $# -ge 2 ]] || { echo "run-qemu: --monitor-port needs a value" >&2; exit 2; }
                        MONITOR_PORT="$2"; shift 2 ;;
        --bus-devices)  BUS_DEVICES=1; shift ;;
        -*) echo "run-qemu: unknown option '$1'" >&2; usage >&2; exit 2 ;;
        *)  EXAMPLE="$1"; shift ;;
    esac
done

case "$PROFILE" in
    release|debug) ;;
    *) echo "run-qemu: --profile must be 'release' or 'debug'" >&2; exit 2 ;;
esac

if ! FEATURES="$(required_features "$EXAMPLE")"; then
    {
        echo "run-qemu: no [[example]] named '$EXAMPLE' in the crate manifest."
        echo
        echo "Available examples:"
        examples_list | sed 's/^/  /'
    } >&2
    exit 1
fi

cd "$REPO_ROOT"

if [[ "$NO_BUILD" -eq 0 ]]; then
    echo "Building example '$EXAMPLE'..."
    declare -a CARGO_ARGS=(build --manifest-path "$CRATE_MANIFEST" --example "$EXAMPLE")
    if [[ "$PROFILE" == "release" ]]; then CARGO_ARGS+=(--release); fi
    if [[ -n "$FEATURES" ]]; then CARGO_ARGS+=(--features "$FEATURES"); fi
    cargo "${CARGO_ARGS[@]}"
fi

KERNEL="$REPO_ROOT/target/aarch64-unknown-none/$PROFILE/examples/$EXAMPLE"
if [[ ! -f "$KERNEL" ]]; then
    echo "run-qemu: example '$EXAMPLE' not built at $KERNEL (drop --no-build, or check the name)" >&2
    exit 1
fi

# --auto-draw drives the console programmatically, so the PL011 moves onto
# a unix socket that this script relays by hand.
if [[ "$AUTO_DRAW" -eq 1 ]]; then DISPLAY_WINDOW=1; fi
SOCK="${TMPDIR:-/tmp}/cogware-run-$$.sock"

# -M raspi3b        BCM2837, 1 GiB, 4x Cortex-A53. Never pass -smp: this
#                   machine hard-codes min == max == 4.
# -serial mon:stdio serial0 is the PL011, the kernel console. 'mon:' puts the
#                   QEMU monitor on the same channel, so Ctrl-A X quits.
# -serial null      serial1 is the AUX mini UART; discarded.
declare -a QEMU_ARGS=(-M raspi3b)
if [[ "$AUTO_DRAW" -eq 1 ]]; then
    # wait=on holds the CPU until the relay attaches; wait=off loses the
    # start of the banner.
    QEMU_ARGS+=(-serial "unix:$SOCK,server=on,wait=on")
else
    QEMU_ARGS+=(-serial mon:stdio)
fi
QEMU_ARGS+=(-serial null -kernel "$KERNEL")

# QEMU 9.0+ models SPI0 and the BSC controllers for real (not stubs), so
# virtual peripherals can attach. Needed by the bus examples; harmless
# otherwise.
if [[ "$EXAMPLE" == "buses" || "$EXAMPLE" == "async-bus" || "$BUS_DEVICES" -eq 1 ]]; then
    QEMU_ARGS+=(
        -device ds1338,address=0x68,bus=i2c-bus.1
        -device tmp105,address=0x49,bus=i2c-bus.1
        -device sst25vf016b,bus=spi
    )
fi

if [[ "$DISPLAY_WINDOW" -eq 1 ]]; then
    # Window stays blank until the kernel allocates a framebuffer ('f').
    # show-cursor has been a display sub-option since QEMU 8. The top-level
    # -show-cursor flag was removed in QEMU 11 and is a hard parse error.
    QEMU_ARGS+=(-display default,show-cursor=on)
else
    QEMU_ARGS+=(-display none)
fi

if [[ -n "$SD_IMAGE" ]]; then
    if [[ "$SD_IMAGE" != /* ]]; then SD_IMAGE="$REPO_ROOT/$SD_IMAGE"; fi
    if [[ ! -f "$SD_IMAGE" ]]; then
        echo "Creating ${SD_SIZE_MB}MiB SD image at $SD_IMAGE"
        qemu-img create -f raw "$SD_IMAGE" "${SD_SIZE_MB}M" >/dev/null
    fi
    # raspi3b wires if=sd to the EMMC controller; the image must be raw, not
    # qcow2.
    QEMU_ARGS+=(-drive "file=$SD_IMAGE,if=sd,format=raw")
fi

if [[ "$AUTO_DRAW" -eq 1 && "$MONITOR_PORT" -gt 0 ]]; then
    # QEMU 11+ requires `-object monitor-hmp`; `-monitor`/`-mon` produce a
    # monitor that never answers.
    QEMU_ARGS+=(
        -chardev "socket,id=mon0,host=127.0.0.1,port=$MONITOR_PORT,server=on,wait=off"
        -object monitor-hmp,id=hmp0,chardev=mon0
    )
fi

if [[ "$TRACE" -eq 1 ]]; then QEMU_ARGS+=(-d int,guest_errors -D "$REPO_ROOT/int.log"); fi
if [[ "$GDB" -eq 1 ]]; then QEMU_ARGS+=(-s -S); fi

echo
echo "  kernel : $KERNEL"
echo "  qemu   : $(command -v qemu-system-aarch64)"
[[ "$DISPLAY_WINDOW" -eq 1 ]] && echo "  display: window open."
if [[ "$AUTO_DRAW" -eq 1 ]]; then
    echo "  startup: will send '${STARTUP[*]}' once boot settles."
elif [[ "$DISPLAY_WINDOW" -eq 1 ]]; then
    echo "  hint   : press f then g in this console to draw the test card."
fi
[[ -n "$SD_IMAGE" ]] && echo "  sd     : $SD_IMAGE"
[[ "$GDB" -eq 1 ]] && echo "  gdb    : stub on :1234, CPU halted. Attach, then \`continue\`."
if [[ "$AUTO_DRAW" -eq 1 ]]; then
    echo "  quit   : Ctrl-C, or close the QEMU window."
else
    echo "  quit   : Ctrl-A then X"
fi
echo
printf '%*s\n' 72 '' | tr ' ' -

if [[ "$AUTO_DRAW" -eq 0 ]]; then
    set +e
    qemu-system-aarch64 "${QEMU_ARGS[@]}"
    rc=$?
    set -e
    printf '%*s\n' 72 '' | tr ' ' -
    echo "QEMU exited with $rc."
    exit "$rc"
fi

# --- --auto-draw: relay the kernel console by hand ---------------------------
#
# Boot output is watched until it goes quiet, the startup commands are fired,
# and from then on this terminal behaves like a normal console.
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

python3 - "$SOCK" "${STARTUP[@]}" <<'PY'
import os, select, socket, sys, termios, time, tty

sock_path, commands = sys.argv[1], list(sys.argv[2:])
QUIET_SECONDS = 1.2
ESCAPE = 0x1D  # Ctrl-]

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
    raise SystemExit(f"run-qemu: could not connect to {sock_path}")
sock.setblocking(False)

fd = sock.fileno()
interactive = sys.stdin.isatty()
saved = None
if interactive:
    saved = termios.tcgetattr(sys.stdin.fileno())
    tty.setraw(sys.stdin.fileno())

last_output = time.monotonic()
try:
    watch = [fd, sys.stdin.fileno()] if interactive else [fd]
    while True:
        readable, _, _ = select.select(watch, [], [], 0.05)
        if fd in readable:
            data = os.read(fd, 4096)
            if not data:
                break
            sys.stdout.buffer.write(data)
            sys.stdout.buffer.flush()
            last_output = time.monotonic()
            continue
        if commands and time.monotonic() - last_output >= QUIET_SECONDS:
            command = commands.pop(0)
            sys.stdout.write(f"\r\n<<< {command} >>>\r\n")
            sys.stdout.flush()
            os.write(fd, command.encode())
            last_output = time.monotonic()
            continue
        if interactive and sys.stdin.fileno() in readable:
            keys = os.read(sys.stdin.fileno(), 1024)
            if not keys or ESCAPE in keys:
                break
            os.write(fd, keys)
            last_output = time.monotonic()
except KeyboardInterrupt:
    pass
finally:
    if saved is not None:
        termios.tcsetattr(sys.stdin.fileno(), termios.TCSADRAIN, saved)
    sock.close()
PY

echo
printf '%*s\n' 72 '' | tr ' ' -
echo "QEMU exited."