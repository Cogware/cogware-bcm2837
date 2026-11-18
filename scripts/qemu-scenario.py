#!/usr/bin/env python3
"""Boot a kernel in QEMU and run a scenario against it: wait for console
output, act on the emulated hardware through QMP, check what it reports.

    scripts/qemu-scenario.py SCENARIO... [--qemu BIN] [--debug]

With several scenarios, runs each and ends with a summary; exits 1 if any
failed.

A scenario is a text file, one step per line:

    boot <example> [qemu args...]    first step: which example (or a path to
                                     any ELF, relative to the scenario), and
                                     extra devices
    expect <regex>                   wait for console output to match
    send <text>                      type into the console (\\n for Enter)
    drive <pin>=<0|1|z>[,...]        drive GPIO pins from outside (cogware QEMU)
    check <path>.<prop> <op> <value> read a QOM property; op is ==, !=, >=, <=,
                                     or ~ (within 1 %)
    set <path>.<prop> <value>        write a QOM property, e.g. a panel's
                                     snapshot (a file path for what it shows)
    qmp <command> [json-args]        run any QMP command
    screendump <file.ppm> [device]   save the display, or one device's (by id)
    sleep <ms>                       let guest time pass

Blank lines and lines starting with # are ignored. Exits 1 on the first
failed step, after printing the console transcript.
"""
import argparse
import json
import os
import re
import socket
import subprocess
import sys
import tempfile
import time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DEFAULT_QEMU = os.path.expanduser("~/qemu-cogware/build/qemu-system-aarch64")
TIMEOUT = 20.0


class Scenario:
    def __init__(self, qemu, elf, extra, workdir):
        self.console_path = os.path.join(workdir, "console.sock")
        self.qmp_path = os.path.join(workdir, "qmp.sock")
        # A file, not a pipe: an unread pipe that fills stalls QEMU's main
        # loop, and QMP with it.
        self.log_path = os.path.join(workdir, "qemu.log")
        self.log = open(self.log_path, "wb")
        self.proc = subprocess.Popen(
            # Paused (-S) until QMP is ready: QEMU can leave a QMP handshake
            # that lands mid-boot unanswered, and nothing should run unseen.
            [qemu, "-M", "raspi3b", "-display", "none", "-S",
             "-chardev", f"socket,id=con,path={self.console_path},server=on,wait=on",
             "-serial", "chardev:con", "-serial", "null",
             "-qmp", f"unix:{self.qmp_path},server=on,wait=off",
             "-kernel", elf, *extra],
            stdout=subprocess.DEVNULL, stderr=self.log)
        self.console = self._connect(self.console_path)
        self.console.setblocking(False)
        self.qmp = self._connect(self.qmp_path).makefile("rw")
        json.loads(self.qmp.readline())
        self._qmp("qmp_capabilities")
        self.transcript = ""
        self.cursor = 0
        self._qmp("cont")

    def _connect(self, path):
        deadline = time.time() + 10
        while True:
            try:
                s = socket.socket(socket.AF_UNIX)
                s.connect(path)
                return s
            except OSError:
                if time.time() > deadline or self.proc.poll() is not None:
                    raise SystemExit(f"could not connect to {path}: "
                                     f"{open(self.log_path).read()}")
                time.sleep(0.05)

    def _qmp(self, command, arguments=None):
        message = {"execute": command}
        if arguments is not None:
            message["arguments"] = arguments
        self.qmp.write(json.dumps(message) + "\n")
        self.qmp.flush()
        while True:
            reply = json.loads(self.qmp.readline())
            if "return" in reply:
                return reply["return"]
            if "error" in reply:
                raise StepFailed(f"QMP {command}: {reply['error']['desc']}")

    def _pump(self):
        try:
            while True:
                data = self.console.recv(65536)
                if not data:
                    break
                self.transcript += data.decode(errors="replace")
        except BlockingIOError:
            pass

    def expect(self, pattern):
        regex = re.compile(pattern)
        deadline = time.time() + TIMEOUT
        while time.time() < deadline:
            self._pump()
            match = regex.search(self.transcript, self.cursor)
            if match:
                self.cursor = match.end()
                return match.group(0)
            time.sleep(0.02)
        raise StepFailed(f"no console output matching /{pattern}/")

    def send(self, text):
        self.console.sendall(text.encode().decode("unicode_escape").encode())

    def qom_get(self, path, prop):
        return self._qmp("qom-get", {"path": path, "property": prop})

    def qom_set(self, path, prop, value):
        self._qmp("qom-set", {"path": path, "property": prop, "value": value})

    def close(self):
        self._pump()
        self.proc.kill()
        self.proc.wait()


class StepFailed(Exception):
    pass


GPIO = "/machine/soc/peripherals/gpio"


def compare(actual, op, expected):
    try:
        actual_n, expected_n = float(actual), float(expected)
    except (TypeError, ValueError):
        return {"==": str(actual) == expected, "!=": str(actual) != expected}.get(op, False)
    return {
        "==": actual_n == expected_n,
        "!=": actual_n != expected_n,
        ">=": actual_n >= expected_n,
        "<=": actual_n <= expected_n,
        "~": abs(actual_n - expected_n) <= abs(expected_n) * 0.01,
    }[op]


def run_step(sc, verb, rest, here):
    """Runs one step; @here is the scenario's directory, which relative
    output paths are taken from."""
    def output(path):
        return os.path.normpath(os.path.join(here, path))

    if verb == "expect":
        return sc.expect(rest)
    if verb == "send":
        sc.send(rest)
    elif verb == "drive":
        sc.qom_set(GPIO, "drive", rest)
    elif verb == "check":
        # A missing value means the empty string, e.g. "last-error ==".
        target, op, expected = (rest.split(None, 2) + [""])[:3]
        path, prop = target.rsplit(".", 1)
        actual = sc.qom_get(path, prop)
        if not compare(actual, op, expected):
            raise StepFailed(f"{target} is {actual}, wanted {op} {expected}")
        return f"{actual}"
    elif verb == "set":
        target, _, value = rest.partition(" ")
        path, prop = target.rsplit(".", 1)
        if prop == "snapshot":
            value = output(value)
        sc.qom_set(path, prop, value)
    elif verb == "qmp":
        command, _, args = rest.partition(" ")
        return json.dumps(sc._qmp(command, json.loads(args) if args else None))
    elif verb == "screendump":
        filename, _, device = rest.partition(" ")
        arguments = {"filename": output(filename)}
        if device:
            arguments["device"] = device
        sc._qmp("screendump", arguments)
    elif verb == "sleep":
        time.sleep(int(rest) / 1000)
    else:
        raise StepFailed(f"unknown step '{verb}'")
    return ""


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("scenarios", nargs="+", metavar="scenario")
    parser.add_argument("--qemu", default=os.environ.get("COGWARE_QEMU", DEFAULT_QEMU))
    parser.add_argument("--debug", action="store_true", help="boot the debug build")
    args = parser.parse_args()

    results = [(path, run_scenario(path, args)) for path in args.scenarios]
    if len(results) > 1:
        print()
        for path, ok in results:
            print(f"  {'PASS' if ok else 'FAIL'}  {path}")
    sys.exit(0 if all(ok for _, ok in results) else 1)


def run_scenario(scenario, args):
    """Runs one scenario file; True if every step passed."""
    steps = []
    for number, line in enumerate(open(scenario), 1):
        line = line.strip()
        if line and not line.startswith("#"):
            verb, _, rest = line.partition(" ")
            steps.append((number, verb, rest.strip()))
    if not steps or steps[0][1] != "boot":
        print(f"{scenario}: a scenario starts with a 'boot <example>' step")
        return False
    example, *extra = steps[0][2].split()
    profile = "debug" if args.debug else "release"
    if "/" in example:
        elf = os.path.join(os.path.dirname(os.path.abspath(scenario)), example)
    else:
        elf = os.path.join(REPO, "target", "aarch64-unknown-none", profile, "examples", example)

    with tempfile.TemporaryDirectory() as workdir:
        sc = Scenario(args.qemu, elf, extra, workdir)
        failed = None
        try:
            for number, verb, rest in steps[1:]:
                try:
                    result = run_step(sc, verb, rest,
                                      os.path.dirname(os.path.abspath(scenario)))
                    print(f"  ok   {number:>3}: {verb} {rest}" + (f"  -> {result}" if result else ""))
                except StepFailed as error:
                    failed = f"  FAIL {number:>3}: {verb} {rest}\n         {error}"
                    break
        finally:
            sc.close()
        if failed:
            print(failed)
            print("--- console ---")
            print(sc.transcript)
            print("--- qemu ---")
            print(open(sc.log_path, errors="replace").read()[-4000:])
            return False
    print(f"{os.path.basename(scenario)}: PASS")
    return True



if __name__ == "__main__":
    main()
