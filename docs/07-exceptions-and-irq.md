# Exceptions and IRQ

The exception path is split between assembly and Rust. The vector table and the
save/restore sequence live in `cogware-bcm2837\src\arch\exception.s`; the Rust trap
frame, syndrome decoding and dispatch live in `cogware-bcm2837\src\arch\exception.rs`.

The design is conservative because the kernel is built for hard-float
`aarch64-unknown-none`. The compiler may use NEON registers in ordinary Rust, so
exception entry saves the whole FP/SIMD register file before calling Rust. See
[01-toolchain-and-build.md](01-toolchain-and-build.md) for the target choice and
[03-memory-and-mmu.md](03-memory-and-mmu.md) for the MMU context.

## Vector table layout

`__exception_vector_table` is defined in `cogware-bcm2837\src\arch\exception.s`. It
is placed in `.text.exception` and aligned with `.balign 2048` at
`cogware-bcm2837\src\arch\exception.s`, as required by `VBAR_EL1`.

Each slot is produced by `VECTOR_ENTRY` at
`cogware-bcm2837\src\arch\exception.s`:

```asm
.macro VECTOR_ENTRY kind
    .balign 128
    sub  sp, sp, #832
    stp  x0, x1, [sp, #0]
    mov  x0, #\kind
    b    __trap_entry
.endm
```

There are sixteen 128-byte slots:

```text
0   current EL, SP_EL0, synchronous
1   current EL, SP_EL0, IRQ
2   current EL, SP_EL0, FIQ
3   current EL, SP_EL0, SError
4   current EL, SP_ELx, synchronous
5   current EL, SP_ELx, IRQ
6   current EL, SP_ELx, FIQ
7   current EL, SP_ELx, SError
8   lower EL, AArch64, synchronous
9   lower EL, AArch64, IRQ
10  lower EL, AArch64, FIQ
11  lower EL, AArch64, SError
12  lower EL, AArch32, synchronous
13  lower EL, AArch32, IRQ
14  lower EL, AArch32, FIQ
15  lower EL, AArch32, SError
```

The current kernel normally uses slot 4 for synchronous exceptions and slot 5
for IRQs, because it runs at EL1 on `SP_EL1`. The unused lower-EL and AArch32
slots are documented in `cogware-bcm2837\src\arch\exception.s`.

## Installing the table

Rust imports the vector table symbol at `cogware-bcm2837\src\arch\exception.rs`.
`install_vector_table` writes its address to `VBAR_EL1`
(`cogware-bcm2837\src\arch\exception.rs`) and debug-asserts the 2 KiB alignment at
`cogware-bcm2837\src\arch\exception.rs`.

`VBAR_EL1` is banked per core. Every core must install the table for itself.
Installing it does not unmask interrupts; the source comment at
`cogware-bcm2837\src\arch\exception.rs` keeps the intended order explicit:
install the table, register a handler, then unmask IRQs.

## Trap frame layout

`TrapFrame` is declared at `cogware-bcm2837\src\arch\exception.rs`. It is
`#[repr(C)]`, and that representation is load-bearing because assembly indexes
the frame with literal offsets.

The frame is exactly 832 bytes:

```text
gpr[31]      @ 0       x0..x30, with x30 as lr
elr_el1      @ 248     exception return address
spsr_el1     @ 256     saved PSTATE
esr_el1      @ 264     syndrome
far_el1      @ 272     fault address for aborts and alignment faults
sp_el0       @ 280     saved SP_EL0
kind         @ 288     vector slot index
_padding     @ 296     keeps the FP area 16-byte aligned
fpsr         @ 304     FP status register
fpcr         @ 312     FP control register
fpregs[32]   @ 320     q0..q31, 512 bytes
```

The compile-time assertions are at `cogware-bcm2837\src\arch\exception.rs`. They
check every offset, `size_of::<TrapFrame>() == 832`, and 16-byte stack
alignment. The assembly repeats the same layout in its header comment
(`cogware-bcm2837\src\arch\exception.s`) and in its stores.

Because exception entry subtracts 832 bytes, the per-core stack is deliberately
generous: `KERNEL_STACK_SIZE = 128 * 1024` at `cogware-bcm2837\src\bsp\memory.rs`.

## Why `fpsr` and `fpcr` come before `fpregs`

The order is dictated by AArch64 encoding. `FPSR` and `FPCR` are stored with a
64-bit `stp` at offset `#304` (`cogware-bcm2837\src\arch\exception.s`). A 64-bit
`stp` immediate is signed 7-bit and scaled by 8, so the largest positive offset
is `63 * 8 = 504`.

If `fpsr` and `fpcr` were placed after all vector registers, they would be at
`#816`, which cannot be encoded by that 64-bit `stp`. The vector registers use
128-bit `stp`, scaled by 16, so the last vector pair at `#800`
(`cogware-bcm2837\src\arch\exception.s`) is encodable: `800 / 16 = 50`.

## Entry and return

`__trap_entry` starts at `cogware-bcm2837\src\arch\exception.s`. On entry, the macro
has already saved `x0` and `x1`; the common entry code then saves:

* `x2..x30` at `#16..#240` (`cogware-bcm2837\src\arch\exception.s`);
* `kind` at `#288` (`cogware-bcm2837\src\arch\exception.s`);
* `ELR_EL1` and `SPSR_EL1` at `#248` (`cogware-bcm2837\src\arch\exception.s`);
* `ESR_EL1` and `FAR_EL1` at `#264` (`cogware-bcm2837\src\arch\exception.s`);
* `SP_EL0` at `#280` (`cogware-bcm2837\src\arch\exception.s`);
* `FPSR` and `FPCR` at `#304` (`cogware-bcm2837\src\arch\exception.s`);
* `q0..q31` at `#320..#800` (`cogware-bcm2837\src\arch\exception.s`).

It passes `sp` as `&mut TrapFrame` and calls `__trap_handler`
(`cogware-bcm2837\src\arch\exception.s`). After Rust returns, it restores the
same state in reverse (`cogware-bcm2837\src\arch\exception.s`), adds 832 to
`sp`, and executes `eret` (`cogware-bcm2837\src\arch\exception.s`).

Rust may edit the frame before return. The breakpoint path advances `ELR_EL1`
so `eret` resumes after the `BRK` instruction.

## Cost of hard float

All 32 NEON `q` registers are saved and restored on every exception. This is
required because hard-float `aarch64-unknown-none` lets the compiler use NEON
registers anywhere, and stable Rust cannot disable NEON for one function with a
stable `#[target_feature]`-off attribute.

The cost is concrete: `32 * 16 = 512` extra bytes per trap frame, plus `FPSR`
and `FPCR`. This is the cost of choosing the Cortex-A53's integrated FPU
over a soft-float target.

## Slot and syndrome decoding

`ExceptionKind` is declared at `cogware-bcm2837\src\arch\exception.rs`.
`from_index` maps raw slot numbers to names (`cogware-bcm2837\src\arch\exception.rs`),
`is_irq` recognises IRQ slots (`cogware-bcm2837\src\arch\exception.rs`), and `name`
returns the printed text (`cogware-bcm2837\src\arch\exception.rs`).
`TrapFrame::exception_kind` is the wrapper at `cogware-bcm2837\src\arch\exception.rs`.

`Syndrome` wraps `ESR_EL1` (`cogware-bcm2837\src\arch\exception.rs`) and decodes:

* exception class, `EC`, with `class()` (`cogware-bcm2837\src\arch\exception.rs`);
* instruction length, `IL`, with `instruction_length_32()`
  (`cogware-bcm2837\src\arch\exception.rs`);
* instruction-specific syndrome, `ISS`, with `iss()`
  (`cogware-bcm2837\src\arch\exception.rs`);
* class text with `class_name()` (`cogware-bcm2837\src\arch\exception.rs`);
* whether `FAR_EL1` is meaningful with `is_abort()`
  (`cogware-bcm2837\src\arch\exception.rs`);
* data/instruction fault status with `fault_status()`
  (`cogware-bcm2837\src\arch\exception.rs`);
* data-abort read versus write with `is_write()`
  (`cogware-bcm2837\src\arch\exception.rs`).

The `Display` implementation at `cogware-bcm2837\src\arch\exception.rs` produces the
compact log form:

```text
EC=0x25 (data abort, same EL), IL=1, ISS=0x0000005
```

## Rust dispatch

`__trap_handler` starts at `cogware-bcm2837\src\arch\exception.rs`. It decodes the
kind and syndrome, then handles cases in this order:

1. IRQ slot: call the registered handler, or print that no handler is installed
   (`cogware-bcm2837\src\arch\exception.rs`);
2. `BRK`: print a breakpoint message, call `skip_faulting_instruction`, and
   return (`cogware-bcm2837\src\arch\exception.rs`);
3. everything else: dump the frame, print a halting line, and halt the core
   (`cogware-bcm2837\src\arch\exception.rs`).

`trigger_breakpoint` emits `brk #0` (`cogware-bcm2837\src\arch\exception.rs`).
`TrapFrame::skip_faulting_instruction` adds four to `ELR_EL1`
(`cogware-bcm2837\src\arch\exception.rs`), which is correct for AArch64 fixed-width
instructions. The `BRK #0` self-test has been verified to trap and return
cleanly.

## Worked data abort

A deliberate read of unmapped `0xC000_0000` has been verified to produce:

```text
ESR_EL1: 0x0000000096000005  EC=0x25 (data abort, same EL), IL=1, ISS=0x0000005
fault: translation fault, level 1
access: read
FAR_EL1: 0x00000000c0000000
SPSR_EL1: 0x00000000600003c5
```

The raw ESR explains the text: `EC = 0x25` is a same-EL data abort; `IL = 1`
means a 32-bit instruction; `ISS = 0x5` maps to “translation fault, level 1”;
bit 6 of `ISS` is clear, so this was a read; and because this is an abort,
`FAR_EL1` contains the faulting address. The relevant printing code is in
`dump` at `cogware-bcm2837\src\arch\exception.rs`.

## Register dumps and the console lock

`dump` starts at `cogware-bcm2837\src\arch\exception.rs`. It calls
`console::with_emergency_writer` once for the whole report, taking the UART lock
when it is immediately available. The report includes the vector
slot, `ESR_EL1`, fault status, access direction, `FAR_EL1`, `ELR_EL1`,
`SPSR_EL1`, `SP_EL0`, `FPSR/CR`, and `x0..x30`
(`cogware-bcm2837\src\arch\exception.rs`).

`console::with_emergency_writer` uses `try_lock`, not `lock`
(`cogware-bcm2837\src\console.rs`). A fault can occur on a core already inside
`println!`, and therefore already holding the UART lock. Blocking there would
deadlock the core. The emergency writer instead uses stateless external MMIO:
it never borrows the shared driver or changes its counters. Contended reports
may interleave, but do not fabricate a second mutable reference to UART state.
Bounded FIFO waits return `fmt::Error` if the UART cannot progress. The fault
path has no second diagnostic channel on which to report a failed UART.

Breakpoint messages, unexpected IRQ messages and the final halting line use
the same path. Panic reporting also uses it rather than forcibly unlocking
another core's console. See [06-synchronisation.md](06-synchronisation.md).

## IRQ handler registration

The vector table already routes IRQs to Rust. The callback slot is:

```rust
static IRQ_HANDLER: AtomicUsize = AtomicUsize::new(0);
pub type IrqHandler = fn(&mut TrapFrame);
```

These are at `cogware-bcm2837\src\arch\exception.rs`.
`set_irq_handler` stores a raw function pointer with release ordering
(`cogware-bcm2837\src\arch\exception.rs`). The private `irq_handler` helper uses
an acquire load and transmutes a non-zero address back to `IrqHandler`
(`cogware-bcm2837\src\arch\exception.rs`).

A lock would be wrong. The handler runs in exception context with interrupts
masked. If it tried to take a spin lock held by the interrupted code, it would
wait for code that cannot run until the handler returns. The atomic function
pointer avoids blocking in the IRQ path.

If no handler is installed, the IRQ path prints an unexpected-IRQ message
(`cogware-bcm2837\src\arch\exception.rs`).

## The interrupt controller

`drivers::interrupt::init` installs its dispatcher in this slot. The dispatcher
reads the calling core's ARM-local `IRQ_SOURCE` register and handles, in order,
the core's virtual-timer tick, mailbox 0 (inter-processor interrupts) and the
"GPU" source, which carries the SoC's 64 peripheral interrupts and is routed to
core 0. The controller itself, its handler table and the per-core tick are in
[24-interrupts.md](24-interrupts.md).

The PL011 hooks (`Pl011Uart::enable_rx_interrupts`, `clear_interrupts`) are
still unused: the console is polled, and with a tick running it is polled at
the tick rate. Making it interrupt-driven means registering a handler for IRQ
57 that acknowledges with `ICR_ALL` and wakes the task awaiting the byte, and
that handler must not take `UART`.

## Relationship to SMP and MMU state

Exception entry itself does not take a `SpinLock`; it writes the trap frame to
the current stack and calls Rust. Emergency reporting never uses exclusive
atomics with the MMU off. With the MMU on it may try the UART lock, but it
accesses only external MMIO in either case, not an interrupted driver's RAM.

For normal SMP operation, every secondary has enabled its MMU before its first
`println!`, before it takes a `SpinLock`, and before it waits on a `Barrier`.
That keeps exception reporting compatible with the global synchronisation rule
rather than relying on special exception-only behaviour.

## Failure modes

If an exception dump hangs before printing registers, suspect a blocking lock in
exception context. If a dump prints but lines from different cores interleave,
that is the accepted failure mode of stateless emergency output while the
ordinary UART lock is held. Ordinary `with_writer` never bypasses contention.

If an IRQ fires repeatedly, suspect missing acknowledgement. For PL011, the
acknowledgement hook is `clear_interrupts`, which writes `ICR_ALL = 0x7FF` to
`ICR`.

If the trap frame changes, update both `TrapFrame` and `exception.s`, then
extend the `const` offset assertions. The current code is safe because the Rust
and assembly views of the frame are mechanically checked at build time.
