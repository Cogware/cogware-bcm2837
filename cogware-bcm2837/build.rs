// SPDX-License-Identifier: GPL-3.0-only
//! Build script for the BCM2837 BSP.
//!
//! Linker script wiring, in two parts:
//!
//! 1. **This package's own examples**: `-T<link.ld>` is passed directly.
//!    `cargo::rustc-link-arg` applies to this package's benches, bins, examples
//!    and tests only.
//!
//! 2. **Downstream crates**: `link.ld` is copied into `OUT_DIR`, which is added
//!    to the linker search path. Link arguments do not cross a dependency edge,
//!    so a downstream binary must request `-C link-arg=-Tlink.ld` itself, but
//!    need not know the script's location.
//!
//! Also declares rebuild triggers for the assembly sources, which
//! `global_asm!(include_str!(..))` pulls in without Cargo noticing.

use std::{env, fs, path::PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("cargo always sets CARGO_MANIFEST_DIR"),
    );
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("cargo always sets OUT_DIR"));

    let linker_script = manifest_dir.join("link.ld");

    // (2) Publish the script where a downstream `-Tlink.ld` will find it.
    fs::copy(&linker_script, out_dir.join("link.ld")).expect("failed to stage link.ld in OUT_DIR");
    println!("cargo::rustc-link-search={}", out_dir.display());

    // (1) `rust-lld` is invoked directly for `aarch64-unknown-none`, so the
    // flag is passed unwrapped rather than as `-Wl,-T`.
    println!("cargo::rustc-link-arg=-T{}", linker_script.display());

    // Keep the ELF's symbol and DWARF sections intact for JTAG debugging.
    println!("cargo::rustc-link-arg=--build-id=none");

    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed={}", linker_script.display());
    for source in [
        "src/arch/boot.s",
        "src/arch/cache.s",
        "src/arch/exception.s",
    ] {
        println!(
            "cargo::rerun-if-changed={}",
            manifest_dir.join(source).display()
        );
    }
}
