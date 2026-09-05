// SPDX-License-Identifier: MIT OR Apache-2.0
//! EL1 exception handling: vector table installation, the trap frame and
//! fault decoding.
//!
//! The assembly half lives in `exception.s`; the byte offsets there and the
//! field offsets of [`TrapFrame`] are cross-checked by the `const` assertions
//! at the bottom of this module, so a mismatch is a compile error.

use core::{
    fmt,
    mem::offset_of,
    sync::atomic::{AtomicUsize, Ordering},
};

use crate::arch::cpu::{self, write_sysreg};

/// CPU state saved on exception entry and restored by `eret`.
///
/// `#[repr(C)]` is load-bearing: `exception.s` indexes this structure with
/// literal offsets.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TrapFrame {
    /// `x0` through `x30`. `x30` is the link register.
    pub gpr: [u64; 31],
    /// Address the exception was taken from; `eret` returns here.
    pub elr_el1: u64,
    /// Saved `PSTATE`.
    pub spsr_el1: u64,
    /// Exception Syndrome Register: class and cause.
    pub esr_el1: u64,
    /// Fault Address Register: valid for aborts and alignment faults.
    pub far_el1: u64,
    /// `SP_EL0`. Unused by this kernel but saved for completeness.
    pub sp_el0: u64,
    /// Index of the vector table slot that was taken, 0..=15.
    pub kind: u64,
    _padding: u64,
    /// Floating-point status register.
    pub fpsr: u64,
    /// Floating-point control register.
    pub fpcr: u64,
    /// `q0` through `q31`, the full NEON register file.
    pub fpregs: [u128; 32],
}

/// Which of the sixteen vector slots an exception arrived through.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExceptionKind {
    /// Synchronous exception, current EL, `SP_EL0`.
    CurrentElSp0Synchronous,
    /// IRQ, current EL, `SP_EL0`.
    CurrentElSp0Irq,
    /// FIQ, current EL, `SP_EL0`.
    CurrentElSp0Fiq,
    /// SError, current EL, `SP_EL0`.
    CurrentElSp0SError,
    /// Synchronous exception, current EL, `SP_ELx`. The common case.
    CurrentElSpxSynchronous,
    /// IRQ, current EL, `SP_ELx`. The common case.
    CurrentElSpxIrq,
    /// FIQ, current EL, `SP_ELx`.
    CurrentElSpxFiq,
    /// SError, current EL, `SP_ELx`.
    CurrentElSpxSError,
    /// Synchronous exception from a lower EL executing AArch64.
    LowerElAarch64Synchronous,
    /// IRQ from a lower EL executing AArch64.
    LowerElAarch64Irq,
    /// FIQ from a lower EL executing AArch64.
    LowerElAarch64Fiq,
    /// SError from a lower EL executing AArch64.
    LowerElAarch64SError,
    /// Synchronous exception from a lower EL executing AArch32.
    LowerElAarch32Synchronous,
    /// IRQ from a lower EL executing AArch32.
    LowerElAarch32Irq,
    /// FIQ from a lower EL executing AArch32.
    LowerElAarch32Fiq,
    /// SError from a lower EL executing AArch32.
    LowerElAarch32SError,
}

impl ExceptionKind {
    /// Decodes the raw slot index stored by the vector table.
    pub const fn from_index(index: u64) -> Option<Self> {
        Some(match index {
            0 => Self::CurrentElSp0Synchronous,
            1 => Self::CurrentElSp0Irq,
            2 => Self::CurrentElSp0Fiq,
            3 => Self::CurrentElSp0SError,
            4 => Self::CurrentElSpxSynchronous,
            5 => Self::CurrentElSpxIrq,
            6 => Self::CurrentElSpxFiq,
            7 => Self::CurrentElSpxSError,
            8 => Self::LowerElAarch64Synchronous,
            9 => Self::LowerElAarch64Irq,
            10 => Self::LowerElAarch64Fiq,
            11 => Self::LowerElAarch64SError,
            12 => Self::LowerElAarch32Synchronous,
            13 => Self::LowerElAarch32Irq,
            14 => Self::LowerElAarch32Fiq,
            15 => Self::LowerElAarch32SError,
            _ => return None,
        })
    }

    /// Whether this slot carries an IRQ.
    pub const fn is_irq(self) -> bool {
        matches!(
            self,
            Self::CurrentElSp0Irq
                | Self::CurrentElSpxIrq
                | Self::LowerElAarch64Irq
                | Self::LowerElAarch32Irq
        )
    }

    /// Human-readable slot name.
    pub const fn name(self) -> &'static str {
        match self {
            Self::CurrentElSp0Synchronous => "current EL, SP_EL0, synchronous",
            Self::CurrentElSp0Irq => "current EL, SP_EL0, IRQ",
            Self::CurrentElSp0Fiq => "current EL, SP_EL0, FIQ",
            Self::CurrentElSp0SError => "current EL, SP_EL0, SError",
            Self::CurrentElSpxSynchronous => "current EL, SP_ELx, synchronous",
            Self::CurrentElSpxIrq => "current EL, SP_ELx, IRQ",
            Self::CurrentElSpxFiq => "current EL, SP_ELx, FIQ",
            Self::CurrentElSpxSError => "current EL, SP_ELx, SError",
            Self::LowerElAarch64Synchronous => "lower EL (AArch64), synchronous",
            Self::LowerElAarch64Irq => "lower EL (AArch64), IRQ",
            Self::LowerElAarch64Fiq => "lower EL (AArch64), FIQ",
            Self::LowerElAarch64SError => "lower EL (AArch64), SError",
            Self::LowerElAarch32Synchronous => "lower EL (AArch32), synchronous",
            Self::LowerElAarch32Irq => "lower EL (AArch32), IRQ",
            Self::LowerElAarch32Fiq => "lower EL (AArch32), FIQ",
            Self::LowerElAarch32SError => "lower EL (AArch32), SError",
        }
    }
}

/// Decoded `ESR_EL1`.
#[derive(Clone, Copy)]
pub struct Syndrome(u64);

impl Syndrome {
    /// Exception class, `ESR_EL1.EC`.
    pub const fn class(self) -> u8 {
        ((self.0 >> 26) & 0x3F) as u8
    }

    /// Whether the trapping instruction was 32 bits wide, `ESR_EL1.IL`.
    pub const fn instruction_length_32(self) -> bool {
        self.0 & (1 << 25) != 0
    }

    /// Instruction-specific syndrome, `ESR_EL1.ISS`.
    pub const fn iss(self) -> u32 {
        (self.0 & 0x01FF_FFFF) as u32
    }

    /// Description of the exception class.
    pub const fn class_name(self) -> &'static str {
        match self.class() {
            0x00 => "unknown reason",
            0x01 => "trapped WFI/WFE",
            0x07 => "trapped SIMD/FP access (CPACR_EL1.FPEN)",
            0x0E => "illegal execution state",
            0x15 => "SVC from AArch64",
            0x18 => "trapped MSR/MRS/system instruction",
            0x20 => "instruction abort, lower EL",
            0x21 => "instruction abort, same EL",
            0x22 => "PC alignment fault",
            0x24 => "data abort, lower EL",
            0x25 => "data abort, same EL",
            0x26 => "SP alignment fault",
            0x28 => "FP exception, AArch32",
            0x2C => "FP exception, AArch64",
            0x2F => "SError",
            0x30 | 0x31 => "breakpoint",
            0x32 | 0x33 => "software step",
            0x34 | 0x35 => "watchpoint",
            0x3C => "BRK instruction",
            _ => "reserved / unhandled class",
        }
    }

    /// Whether this syndrome describes a memory abort with a valid `FAR_EL1`.
    pub const fn is_abort(self) -> bool {
        matches!(self.class(), 0x20 | 0x21 | 0x22 | 0x24 | 0x25 | 0x26)
    }

    /// Decoded data/instruction fault status code, when applicable.
    pub const fn fault_status(self) -> Option<&'static str> {
        if !matches!(self.class(), 0x20 | 0x21 | 0x24 | 0x25) {
            return None;
        }

        Some(match self.iss() & 0x3F {
            0b000000..=0b000011 => "address size fault",
            0b000100 => "translation fault, level 0",
            0b000101 => "translation fault, level 1",
            0b000110 => "translation fault, level 2",
            0b000111 => "translation fault, level 3",
            0b001001 => "access flag fault, level 1",
            0b001010 => "access flag fault, level 2",
            0b001011 => "access flag fault, level 3",
            0b001101 => "permission fault, level 1",
            0b001110 => "permission fault, level 2",
            0b001111 => "permission fault, level 3",
            0b010000 => "synchronous external abort",
            0b011000 => "synchronous parity/ECC error",
            0b100001 => "alignment fault",
            0b110000 => "TLB conflict abort",
            0b110101 | 0b110100 => "implementation-defined (lockdown/atomic)",
            _ => "unknown fault status",
        })
    }

    /// For data aborts: `true` if the access was a write.
    pub const fn is_write(self) -> Option<bool> {
        match self.class() {
            0x24 | 0x25 => Some(self.iss() & (1 << 6) != 0),
            _ => None,
        }
    }
}

impl fmt::Display for Syndrome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "EC=0x{:02x} ({}), IL={}, ISS=0x{:07x}",
            self.class(),
            self.class_name(),
            u8::from(self.instruction_length_32()),
            self.iss()
        )
    }
}

impl TrapFrame {
    /// Decoded `ESR_EL1`.
    pub const fn syndrome(&self) -> Syndrome {
        Syndrome(self.esr_el1)
    }

    /// Decoded vector slot.
    pub const fn exception_kind(&self) -> Option<ExceptionKind> {
        ExceptionKind::from_index(self.kind)
    }

    /// Advances the return address past the faulting instruction.
    ///
    /// Only correct for 32-bit instructions, which is every AArch64 encoding.
    pub const fn skip_faulting_instruction(&mut self) {
        self.elr_el1 = self.elr_el1.wrapping_add(4);
    }
}

/// Registered IRQ callback, stored as a raw function pointer so installation is
/// a single atomic store.
///
/// A lock would be wrong here: the handler runs with interrupts masked in
/// exception context, so blocking on a lock held by the interrupted code would
/// deadlock the core.
static IRQ_HANDLER: AtomicUsize = AtomicUsize::new(0);

/// Callback invoked for every IRQ taken at EL1.
pub type IrqHandler = fn(&mut TrapFrame);

/// Installs the IRQ callback. Replaces any previous one.
///
/// Unused until the BCM2837 interrupt controller lands; the vector table
/// already routes IRQs here.
#[allow(dead_code)]
pub fn set_irq_handler(handler: IrqHandler) {
    IRQ_HANDLER.store(handler as usize, Ordering::Release);
}

fn irq_handler() -> Option<IrqHandler> {
    match IRQ_HANDLER.load(Ordering::Acquire) {
        0 => None,
        // SAFETY: only ever written from `set_irq_handler` with a value
        // produced by casting an `IrqHandler`.
        address => Some(unsafe { core::mem::transmute::<usize, IrqHandler>(address) }),
    }
}

unsafe extern "C" {
    /// Defined in `exception.s`.
    static __exception_vector_table: u8;
}

/// Points `VBAR_EL1` at the kernel's vector table for the calling core.
///
/// `VBAR_EL1` is banked per core, so every core has to do this for itself.
/// Interrupts stay masked; [`cpu::unmask_interrupts`] enables them once a
/// handler is registered.
///
/// # Safety
///
/// Must run at EL1 with the stack pointer valid.
pub unsafe fn install_vector_table() {
    let address = core::ptr::addr_of!(__exception_vector_table) as u64;
    debug_assert_eq!(address % 2048, 0, "VBAR_EL1 requires 2 KiB alignment");
    unsafe { write_sysreg!("VBAR_EL1", address) };
}

/// Address currently programmed into `VBAR_EL1`.
pub fn vector_table_address() -> usize {
    core::ptr::addr_of!(__exception_vector_table) as usize
}

/// Deliberately raises a synchronous exception to prove the vector table works.
///
/// `BRK #0` reports `EC = 0x3C`. The handler steps over it and returns.
pub fn trigger_breakpoint() {
    // SAFETY: `brk` is a defined instruction; the handler below resumes past it.
    unsafe { core::arch::asm!("brk #0", options(nostack, preserves_flags)) };
}

/// Rust entry point for every exception. Called from `__trap_entry`.
///
/// # Safety
///
/// Called by the exception vector with a valid frame on the current stack.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __trap_handler(frame: &mut TrapFrame) {
    let kind = frame.exception_kind();
    let syndrome = frame.syndrome();

    if kind.is_some_and(ExceptionKind::is_irq) {
        match irq_handler() {
            Some(handler) => handler(frame),
            None => {
                // Reporting must not wait for code interrupted with UART held.
                let _ = crate::console::with_emergency_writer(|w| {
                    writeln!(
                        w,
                        "[core {}] unexpected IRQ with no handler installed",
                        cpu::core_id()
                    )
                });
            }
        }
        return;
    }

    // `BRK` is used by `trigger_breakpoint` and by JTAG sessions; step over it
    // instead of treating it as fatal.
    if syndrome.class() == 0x3C {
        let _ = crate::console::with_emergency_writer(|w| {
            writeln!(
                w,
                "[core {}] breakpoint at {:#018x} (BRK #{})",
                cpu::core_id(),
                frame.elr_el1,
                syndrome.iss() & 0xFFFF
            )
        });
        frame.skip_faulting_instruction();
        return;
    }

    dump(frame);
    let _ = crate::console::with_emergency_writer(|w| {
        writeln!(w, "[core {}] halting.", cpu::core_id())
    });
    cpu::halt();
}

/// Prints a full description of the trapped state.
pub fn dump(frame: &TrapFrame) {
    let syndrome = frame.syndrome();
    let core = cpu::core_id();

    // Prefer one locked report, but never borrow an interrupted UART driver.
    // If the UART fails there is no second channel on which to report that.
    let _ = crate::console::with_emergency_writer(|w| {
        writeln!(w)?;
        writeln!(
            w,
            "=============== EXCEPTION on core {core} ==============="
        )?;
        match frame.exception_kind() {
            Some(kind) => writeln!(w, "vector : {} (slot {})", kind.name(), frame.kind),
            None => writeln!(w, "vector : invalid slot {}", frame.kind),
        }?;
        writeln!(w, "ESR_EL1: {:#018x}  {}", frame.esr_el1, syndrome)?;

        if let Some(status) = syndrome.fault_status() {
            writeln!(w, "fault  : {status}")?;
        }
        if let Some(write) = syndrome.is_write() {
            writeln!(w, "access : {}", if write { "write" } else { "read" })?;
        }
        if syndrome.is_abort() {
            writeln!(w, "FAR_EL1: {:#018x}", frame.far_el1)?;
        }

        writeln!(w, "ELR_EL1: {:#018x}", frame.elr_el1)?;
        writeln!(w, "SPSR_EL1:{:#018x}", frame.spsr_el1)?;
        writeln!(w, "SP_EL0 : {:#018x}", frame.sp_el0)?;
        writeln!(w, "FPSR/CR: {:#010x} / {:#010x}", frame.fpsr, frame.fpcr)?;

        // x0-x27 in rows of four, then x28-x30 labelled with the ABI roles that
        // matter for reading a backtrace.
        for row in 0..7 {
            let base = row * 4;
            writeln!(
                w,
                "x{:<2}={:#018x} x{:<2}={:#018x} x{:<2}={:#018x} x{:<2}={:#018x}",
                base,
                frame.gpr[base],
                base + 1,
                frame.gpr[base + 1],
                base + 2,
                frame.gpr[base + 2],
                base + 3,
                frame.gpr[base + 3]
            )?;
        }
        writeln!(
            w,
            "x28={:#018x} x29={:#018x} (fp) x30={:#018x} (lr)",
            frame.gpr[28], frame.gpr[29], frame.gpr[30]
        )?;
        writeln!(
            w,
            "========================================================"
        )
    });
}

// The assembly in `exception.s` hard-codes these offsets.
const _: () = {
    assert!(offset_of!(TrapFrame, gpr) == 0);
    assert!(offset_of!(TrapFrame, elr_el1) == 248);
    assert!(offset_of!(TrapFrame, spsr_el1) == 256);
    assert!(offset_of!(TrapFrame, esr_el1) == 264);
    assert!(offset_of!(TrapFrame, far_el1) == 272);
    assert!(offset_of!(TrapFrame, sp_el0) == 280);
    assert!(offset_of!(TrapFrame, kind) == 288);
    assert!(offset_of!(TrapFrame, fpsr) == 304);
    assert!(offset_of!(TrapFrame, fpcr) == 312);
    assert!(offset_of!(TrapFrame, fpregs) == 320);
    assert!(core::mem::size_of::<TrapFrame>() == 832);
    // `sub sp, sp, #832` must keep the stack 16-byte aligned.
    assert!(core::mem::size_of::<TrapFrame>().is_multiple_of(16));
};
