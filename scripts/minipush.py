#!/usr/bin/env python3
"""Push a kernel image to the Cogware chain-loader, over serial or TCP.

The Linux counterpart of scripts/minipush.ps1, with the same options and the
same wire protocol -- which is itself deliberately identical to
rust-raspberrypi-OS-tutorials' minipush.rb, so either tool drives either loader:

    target -> host   banner text          printed as it arrives
    target -> host   0x03 0x03 0x03       "send me a kernel"
    host   -> target u32 little-endian    payload length in bytes
    target -> host   "OK"                 length accepted
    host   -> target <payload>            the raw image
    both             free-running terminal the payload's own console

Two transports. `--port` is real hardware: a USB-TTL adapter on GPIO 14/15.
`--tcp` is QEMU, which is how this is tested without a board:

    qemu-system-aarch64 -M raspi3b -display none \
      -serial tcp::4444,server=on,wait=on -serial null \
      -kernel target/aarch64-unknown-none/release/examples/chainloader

Only one process may own a serial port. Close screen/minicom before running.

    python3 scripts/minipush.py --payload kernel8.img
    python3 scripts/minipush.py --payload kernel8.img --port /dev/ttyUSB1
    python3 scripts/minipush.py --payload kernel8.img --tcp localhost:4444

Press Ctrl-] to exit the terminal.
"""

import argparse
import os
import select
import socket
import sys
import termios
import time
import tty

DEFAULT_PORT = "/dev/serial/by-id/usb-FTDI_FT232R_USB_UART_AB5NONPJ-if00-port0"
ESCAPE = 0x1D  # Ctrl-]

BAUDS = {
    9600: termios.B9600,
    19200: termios.B19200,
    38400: termios.B38400,
    57600: termios.B57600,
    115200: termios.B115200,
    230400: termios.B230400,
}

# Set by main() when --log is given. emit() tees into it.
LOG = None


class Link:
    """A duplex byte channel to the target: serial device or TCP socket.

    Both transports are just file descriptors on Linux, so everything below
    works on either one; only opening and closing differ.
    """

    def __init__(self, fd, close):
        self.fd = fd
        self.close = close


def open_serial(path, baud):
    """Open the port in 8N1 raw mode with no flow control."""
    fd = os.open(path, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
    speed = BAUDS[baud]
    cc = list(termios.tcgetattr(fd)[6])
    cc[termios.VMIN] = 0
    cc[termios.VTIME] = 0
    termios.tcsetattr(
        fd,
        termios.TCSANOW,
        [0, 0, termios.CS8 | termios.CREAD | termios.CLOCAL, 0, speed, speed, cc],
    )
    termios.tcflush(fd, termios.TCIOFLUSH)
    return Link(fd, lambda: os.close(fd))


def open_tcp(spec, timeout=10.0):
    """Connect to a QEMU serial socket given as host:port (host optional)."""
    host, _, port = spec.rpartition(":")
    if not port.isdigit():
        raise SystemExit(f"error: --tcp wants host:port, got {spec!r}")
    try:
        sock = socket.create_connection((host or "localhost", int(port)), timeout)
    except OSError as exc:
        raise SystemExit(f"error: cannot connect to {spec}: {exc}") from exc
    sock.setblocking(False)
    return Link(sock.fileno(), sock.close)


def write_all(fd, data, timeout=10.0):
    """os.write on a non-blocking fd is a partial write. Loop until done."""
    sent = 0
    while sent < len(data):
        _, writable, _ = select.select([], [fd], [], timeout)
        if not writable:
            raise SystemExit("error: link write timed out")
        try:
            sent += os.write(fd, data[sent : sent + 4096])
        except BlockingIOError:
            continue
    return sent


def emit(data):
    """Copy target bytes to our stdout without mangling them."""
    sys.stdout.buffer.write(data)
    sys.stdout.buffer.flush()
    if LOG is not None:
        LOG.write(data)
        LOG.flush()


def wait_for_ready(fd, timeout):
    """Block until the loader asks for a kernel, echoing its banner meanwhile."""
    seen = 0
    deadline = time.monotonic() + timeout
    while seen < 3:
        if time.monotonic() > deadline:
            raise SystemExit(
                f"error: no request token after {timeout:.0f}s.\n"
                "       Is the loader running? Did you power-cycle the board?\n"
                "       Is another process (screen/minicom) holding the port?"
            )
        readable, _, _ = select.select([fd], [], [], 0.5)
        if not readable:
            continue
        data = os.read(fd, 4096)
        if not data:
            raise SystemExit("error: link closed while waiting for the loader")
        passthrough = bytearray()
        for byte in data:
            if byte == 0x03:
                seen += 1
                if seen == 3:
                    break
            else:
                seen = 0
                passthrough.append(byte)
        if passthrough:
            emit(bytes(passthrough))


def push(fd, payload):
    """Send the length, check the acknowledgement, then stream the image."""
    print(f"  -> length       : {len(payload)} bytes", flush=True)
    write_all(fd, len(payload).to_bytes(4, "little"))

    reply = b""
    deadline = time.monotonic() + 5.0
    while len(reply) < 2 and time.monotonic() < deadline:
        readable, _, _ = select.select([fd], [], [], 0.5)
        if readable:
            reply += os.read(fd, 2 - len(reply))
    if reply != b"OK":
        raise SystemExit(f"error: loader rejected the length: {reply!r}")
    print("  <- OK", flush=True)

    started = time.monotonic()
    sent = 0
    while sent < len(payload):
        sent += write_all(fd, payload[sent : sent + 4096])
        pct = 100 * sent // len(payload)
        print(f"\r  pushing         : {pct:3d}%  {sent}/{len(payload)}", end="", flush=True)
    elapsed = max(time.monotonic() - started, 0.001)
    print(f"\r  pushed          : {len(payload)} bytes in {elapsed:.2f}s "
          f"({len(payload) / 1024 / elapsed:.1f} KiB/s)", flush=True)


def capture(fd, seconds):
    """Read the payload's output for a fixed window, then return.

    The non-interactive counterpart of terminal(): used by the automated tests,
    which have no keyboard to forward and must not block forever.
    """
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        readable, _, _ = select.select([fd], [], [], 0.2)
        if not readable:
            continue
        data = os.read(fd, 4096)
        if not data:
            break
        emit(data)


def terminal(fd):
    """Plain pass-through terminal until Ctrl-] or EOF."""
    interactive = sys.stdin.isatty()
    saved = None
    if interactive:
        saved = termios.tcgetattr(sys.stdin.fileno())
        tty.setraw(sys.stdin.fileno())
    try:
        watch = [fd, sys.stdin.fileno()] if interactive else [fd]
        while True:
            readable, _, _ = select.select(watch, [], [], 1.0)
            if fd in readable:
                data = os.read(fd, 4096)
                if data:
                    emit(data)
            if interactive and sys.stdin.fileno() in readable:
                keys = os.read(sys.stdin.fileno(), 1024)
                if not keys or ESCAPE in keys:
                    break
                write_all(fd, keys)
    except KeyboardInterrupt:
        pass
    finally:
        if saved is not None:
            termios.tcsetattr(sys.stdin.fileno(), termios.TCSADRAIN, saved)


def main():
    global LOG

    parser = argparse.ArgumentParser(description="Push a kernel to the chain-loader.")
    parser.add_argument("--payload", required=True, help="raw kernel image to send")

    transport = parser.add_mutually_exclusive_group()
    transport.add_argument("--port", help=f"serial device (default {DEFAULT_PORT})")
    transport.add_argument("--tcp", help="QEMU serial socket, e.g. localhost:4444")

    parser.add_argument("--baud", type=int, default=115200, choices=sorted(BAUDS))
    parser.add_argument("--no-terminal", action="store_true",
                        help="exit after the push instead of becoming a terminal")
    parser.add_argument("--run-seconds", type=float, default=5.0,
                        help="with --no-terminal, how long to capture output")
    parser.add_argument("--log", help="also write everything received to this file")
    parser.add_argument("--handshake-timeout", type=float, default=30.0,
                        help="seconds to wait for the loader's request token")
    args = parser.parse_args()

    with open(args.payload, "rb") as handle:
        payload = handle.read()
    if not payload:
        raise SystemExit("error: payload is empty")
    if payload[:4] == b"\x7fELF":
        raise SystemExit(
            "error: that is an ELF file, not a raw image. Run objcopy -O binary first."
        )

    print(f"  payload         : {args.payload}")
    print(f"  size            : {len(payload)} bytes ({len(payload) / 1024:.1f} KiB)")

    if args.tcp:
        print(f"  link            : tcp {args.tcp}")
        link = open_tcp(args.tcp)
    else:
        port = args.port or DEFAULT_PORT
        print(f"  link            : {port} @ {args.baud}")
        link = open_serial(port, args.baud)

    if args.log:
        LOG = open(args.log, "wb")

    try:
        if args.tcp:
            print("  waiting for the loader...", flush=True)
        else:
            print("  waiting for the loader -- power-cycle the Pi now.", flush=True)
        wait_for_ready(link.fd, args.handshake_timeout)
        print("\n  loader ready.", flush=True)
        push(link.fd, payload)
        if args.no_terminal:
            print(f"  --- payload output ({args.run_seconds:g}s) ---\n", flush=True)
            capture(link.fd, args.run_seconds)
        else:
            print("  --- payload output follows (Ctrl-] to exit) ---\n", flush=True)
            terminal(link.fd)
    finally:
        link.close()
        if LOG is not None:
            LOG.close()


if __name__ == "__main__":
    main()