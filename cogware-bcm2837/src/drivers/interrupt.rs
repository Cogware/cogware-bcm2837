// SPDX-License-Identifier: GPL-3.0-only
//! Interrupt delivery: the SoC interrupt controller, the per-core ARM-local
//! sources, and the dispatcher that the exception vector calls.
//!
//! The SoC's 64 peripheral interrupts arrive at one core, core 0 by default,
//! through the ARM-local "GPU" source. Each core also has its own virtual
//! timer tick and four mailboxes for inter-processor interrupts.
//!
//! **A handler must never take a [`crate::sync::SpinLock`].** It runs on top
//! of whatever it interrupted, which may hold that very lock, and the core
//! would spin on itself forever. Handlers acknowledge their source, touch
//! atomics and wake tasks; everything else belongs in the task they wake.
#![allow(dead_code)]

use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use super::mmio::{ReadOnly, RegisterBlock, WriteOnly};
use crate::{
    arch::{
        cpu::{self, read_sysreg, write_sysreg},
        exception::{self, TrapFrame},
    },
    bsp::memory::{CORE_COUNT, local, map},
};

/// Number of SoC peripheral interrupts.
pub const SOC_IRQS: usize = 64;

/// ARM-local source bits in a core's `IRQ_SOURCE` register.
const SOURCE_CNTV: u32 = 1 << 3;
const SOURCE_MAILBOX0: u32 = 1 << 4;
const SOURCE_GPU: u32 = 1 << 8;

/// `CNTV_CTL_EL0.ENABLE`, with `IMASK` clear.
const CNTV_ENABLE: u64 = 1;

/// Bit in a core's timer interrupt control word routing `CNTV` to its IRQ.
const TIMER_CNTV_IRQ: u32 = 1 << 3;

#[repr(C)]
struct Registers {
    _basic_pending: ReadOnly<u32>,  // 0x00
    pending: [ReadOnly<u32>; 2],    // 0x04, 0x08
    _fiq_control: ReadOnly<u32>,    // 0x0c
    enable: [WriteOnly<u32>; 2],    // 0x10, 0x14
    _enable_basic: WriteOnly<u32>,  // 0x18
    disable: [WriteOnly<u32>; 2],   // 0x1c, 0x20
    _disable_basic: WriteOnly<u32>, // 0x24
}

const _: () = assert!(core::mem::offset_of!(Registers, disable) == 0x1c);

/// Why an interrupt could not be registered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// There is no SoC interrupt with that number.
    NoSuchIrq(u32),
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoSuchIrq(irq) => write!(f, "no SoC interrupt {irq}"),
        }
    }
}

/// A peripheral interrupt handler. It must acknowledge its source, or it is
/// called again the moment it returns.
pub type Handler = fn();

/// Handlers, as function pointers so registering one is a single store.
static HANDLERS: [AtomicUsize; SOC_IRQS] = [const { AtomicUsize::new(0) }; SOC_IRQS];

/// Which SoC interrupts are enabled; the controller's own enable state is
/// write-only.
static ENABLED: AtomicU64 = AtomicU64::new(0);

/// Tick period per core in counter ticks, zero when not ticking.
static TICK_PERIOD: [AtomicU64; CORE_COUNT] = [const { AtomicU64::new(0) }; CORE_COUNT];

/// Ticks taken per core.
static TICKS: [AtomicU64; CORE_COUNT] = [const { AtomicU64::new(0) }; CORE_COUNT];

/// Inter-processor interrupts received per core.
static IPIS: [AtomicU64; CORE_COUNT] = [const { AtomicU64::new(0) }; CORE_COUNT];

/// SoC interrupts that arrived with no handler, and were disabled for it.
static SPURIOUS: AtomicU32 = AtomicU32::new(0);

fn soc() -> &'static Registers {
    // SAFETY: `map::IRQ_CONTROLLER` is mapped Device-nGnRnE for the whole of
    // boot. Every register touched is either read-only or write-1-to-act, so
    // sharing them unlocked between cores and the handler is sound.
    unsafe { &*(map::IRQ_CONTROLLER as *const Registers) }
}

fn read_local(address: usize) -> u32 {
    // SAFETY: every `local` address is a mapped ARM-local register.
    unsafe { RegisterBlock::<()>::read_raw(address, 0) }
}

fn write_local(address: usize, value: u32) {
    // SAFETY: as above.
    unsafe { RegisterBlock::<()>::write_raw(address, 0, value) }
}

/// Installs the dispatcher and leaves peripheral interrupts routed to core 0.
///
/// Interrupts stay masked on every core until [`unmask`].
pub fn init() {
    write_local(local::GPU_IRQ_ROUTING, 0);
    exception::set_irq_handler(dispatch);
}

/// Unmasks interrupts on the calling core.
///
/// # Safety
///
/// [`init`] must have run, and every interrupt enabled for this core must have
/// a handler that takes no lock.
pub unsafe fn unmask() {
    // SAFETY: the vector table is installed during boot on every core, and
    // the caller guarantees the dispatcher is.
    unsafe { cpu::unmask_interrupts() };
}

/// Sets the handler for SoC interrupt `irq`. Does not enable it.
pub fn register(irq: u32, handler: Handler) -> Result<(), Error> {
    let slot = HANDLERS.get(irq as usize).ok_or(Error::NoSuchIrq(irq))?;
    // Release: the handler must be visible before the interrupt is enabled.
    slot.store(handler as usize, Ordering::Release);
    Ok(())
}

/// Lets SoC interrupt `irq` through to core 0.
pub fn enable(irq: u32) -> Result<(), Error> {
    if irq as usize >= SOC_IRQS {
        return Err(Error::NoSuchIrq(irq));
    }
    ENABLED.fetch_or(1 << irq, Ordering::Relaxed);
    soc().enable[irq as usize / 32].write(1 << (irq % 32));
    Ok(())
}

/// Blocks SoC interrupt `irq`.
pub fn disable(irq: u32) -> Result<(), Error> {
    if irq as usize >= SOC_IRQS {
        return Err(Error::NoSuchIrq(irq));
    }
    soc().disable[irq as usize / 32].write(1 << (irq % 32));
    ENABLED.fetch_and(!(1 << irq), Ordering::Relaxed);
    Ok(())
}

/// Starts the calling core's virtual-timer tick at `hz`.
///
/// The tick bounds how long a polled future waits: each one wakes the core,
/// and the executor re-polls everything waiting on hardware.
pub fn start_tick(hz: u32) {
    let core = cpu::core_id();
    let period = crate::time::frequency_hz() / u64::from(hz.max(1));
    TICK_PERIOD[core].store(period, Ordering::Relaxed);
    // SAFETY: the virtual timer is this core's own and EL1 may program it.
    unsafe {
        write_sysreg!("CNTV_CVAL_EL0", read_sysreg!("CNTVCT_EL0") + period);
        write_sysreg!("CNTV_CTL_EL0", CNTV_ENABLE);
    }
    // Only this core writes its own timer routing word.
    let control = local::timer_irq_control(core);
    write_local(control, read_local(control) | TIMER_CNTV_IRQ);
}

/// Stops the calling core's tick.
pub fn stop_tick() {
    let core = cpu::core_id();
    let control = local::timer_irq_control(core);
    write_local(control, read_local(control) & !TIMER_CNTV_IRQ);
    // SAFETY: as in `start_tick`.
    unsafe { write_sysreg!("CNTV_CTL_EL0", 0) };
    TICK_PERIOD[core].store(0, Ordering::Relaxed);
}

/// Ticks `core` has taken since boot.
pub fn tick_count(core: usize) -> u64 {
    TICKS.get(core).map_or(0, |t| t.load(Ordering::Relaxed))
}

/// Lets the calling core receive inter-processor interrupts.
pub fn enable_ipi() {
    write_local(local::mailbox_irq_control(cpu::core_id()), 1);
}

/// Interrupts `core`, waking it from `wfi` or `wfe`.
pub fn send_ipi(core: usize) {
    if core < CORE_COUNT {
        write_local(local::mailbox_write_set(core, 0), 1);
    }
}

/// Inter-processor interrupts `core` has received since boot.
pub fn ipi_count(core: usize) -> u64 {
    IPIS.get(core).map_or(0, |t| t.load(Ordering::Relaxed))
}

/// SoC interrupts that fired with no handler registered.
pub fn spurious_count() -> u32 {
    SPURIOUS.load(Ordering::Relaxed)
}

/// The IRQ vector's handler: works out which sources are pending on this core
/// and runs each one's handler.
fn dispatch(_frame: &mut TrapFrame) {
    let core = cpu::core_id();
    let source = read_local(local::irq_source(core));

    if source & SOURCE_CNTV != 0 {
        let period = TICK_PERIOD[core].load(Ordering::Relaxed);
        // SAFETY: as in `start_tick`.
        unsafe {
            if period == 0 {
                write_sysreg!("CNTV_CTL_EL0", 0);
            } else {
                // From the previous deadline, not from now, so the tick does
                // not drift by the interrupt latency each time; but never into
                // the past, or a long stall would replay every missed tick.
                let now = read_sysreg!("CNTVCT_EL0");
                let next = read_sysreg!("CNTV_CVAL_EL0")
                    .wrapping_add(period)
                    .max(now + 1);
                write_sysreg!("CNTV_CVAL_EL0", next);
            }
        }
        TICKS[core].fetch_add(1, Ordering::Relaxed);
    }

    if source & SOURCE_MAILBOX0 != 0 {
        write_local(local::mailbox_read_clear(core, 0), u32::MAX);
        IPIS[core].fetch_add(1, Ordering::Relaxed);
        // The executor sleeps in `wfe`; taking the interrupt woke this core,
        // and `sev` makes sure a core that checked just before still wakes.
        cpu::sev();
    }

    if source & SOURCE_GPU != 0 {
        let enabled = ENABLED.load(Ordering::Relaxed);
        for bank in 0..2 {
            let mut pending = soc().pending[bank].read() & (enabled >> (bank * 32)) as u32;
            while pending != 0 {
                let bit = pending.trailing_zeros();
                pending &= pending - 1;
                let irq = bank as u32 * 32 + bit;
                match HANDLERS[irq as usize].load(Ordering::Acquire) {
                    0 => {
                        // Nobody to acknowledge it, so it would fire forever.
                        let _ = disable(irq);
                        SPURIOUS.fetch_add(1, Ordering::Relaxed);
                    }
                    address => {
                        // SAFETY: only `register` stores here, and only a
                        // `Handler`.
                        let handler = unsafe { core::mem::transmute::<usize, Handler>(address) };
                        handler();
                    }
                }
            }
        }
    }
}
