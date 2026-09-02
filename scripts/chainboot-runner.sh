#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# A Cargo *runner* that chain-loads onto a Pi instead of launching QEMU.
#
# `cargo chain` (defined in .cargo/config.toml) overrides the target's runner
# with this script, so build, objcopy and push happen in one command.
#
# Cargo invokes this as:  chainboot-runner.sh <elf> [args...]
#
# Everything after `cargo chain --` arrives in [args...] and is forwarded
# verbatim to minipush.py:
#
#     cargo chain --example shell
#     cargo chain --example multicore -- --port /dev/ttyUSB0
#     cargo chain --example hello-uart -- --tcp localhost:4444 --no-terminal
#
# Use scripts/chainboot.sh instead for --build-only or to push an existing
# image.

set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

elf="${1:?chainboot-runner: cargo did not pass an ELF path}"
shift

find_objcopy() {
    for tool in rust-objcopy llvm-objcopy aarch64-linux-gnu-objcopy aarch64-none-elf-objcopy; do
        if command -v "$tool" >/dev/null 2>&1; then
            echo "$tool"
            return 0
        fi
    done
    return 1
}

objcopy="$(find_objcopy)" || {
    echo "chainboot: no objcopy found (tried rust-objcopy, llvm-objcopy," >&2
    echo "           aarch64-linux-gnu-objcopy, aarch64-none-elf-objcopy)." >&2
    echo "           cargo install cargo-binutils && rustup component add llvm-tools" >&2
    exit 1
}

# Lands beside the ELF inside target/, which git already ignores.
img="$elf.img"

# The ELF keeps its full DWARF for JTAG; -O binary emits only the loadable
# sections, which is what the loader copies to 0x80000 and jumps to.
"$objcopy" -O binary "$elf" "$img" >&2

echo "chainboot: $(stat -c%s "$img") bytes ready; power-cycle the Pi now." >&2
exec python3 "$script_dir/minipush.py" --payload "$img" "$@"