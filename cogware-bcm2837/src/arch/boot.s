/* SPDX-License-Identifier: MIT OR Apache-2.0
 *
 * cogware-bcm2837 — AArch64 reset / boot path.
 *
 * Entered by:
 *   - core 0, from the Raspberry Pi firmware (armstub8) at EL2, or from QEMU
 *     at EL3 (observed cpsr 0x400003cd);
 *   - cores 1..3, from the firmware SMP spin table at 0xd8+N*8 on hardware;
 *     QEMU's raspi3b releases all four cores at 0x80000 instead.
 *
 * The reset path normalises EL3, EL2 or EL1 to EL1h, so every case above works.
 *
 * Everything up to the branch into Rust is assembly: the kernel targets the
 * HARD-FLOAT `aarch64-unknown-none`, where LLVM may use V registers for plain
 * struct/slice moves, so FP/SIMD must be ungated before any compiler-generated
 * instruction runs.
 *
 * Register usage across this file:
 *   x19 - core id (0..3)
 *   x20 - stack top for this core
 * Neither is preserved for the callee; the branch into Rust is a tail call.
 */

.equ CURRENTEL_EL1, 0b0100
.equ CURRENTEL_EL2, 0b1000
.equ CURRENTEL_EL3, 0b1100

/* SPSR for `eret` into EL2h / EL1h with D, A, I and F masked. */
.equ SPSR_EL2H_MASKED, 0x3C9
.equ SPSR_EL1H_MASKED, 0x3C5

/* SCR_EL3: NS | RES1(4,5) | SMD | HCE | RW(AArch64 below EL3) */
.equ SCR_EL3_VALUE, 0x5B1

/* CPTR_EL2 with every RES1 bit set and every trap bit clear (E2H == 0). */
.equ CPTR_EL2_NO_TRAPS, 0x33FF

/* SCTLR_EL1 with only the ARMv8.0 RES1 bits (11, 20, 22, 23, 28, 29) set.
 *
 * MMU off, caches off, little-endian, no alignment or stack-alignment checks.
 * SCTLR_EL1's reset value is UNKNOWN outside its RES1 bits and the Pi firmware
 * guarantees nothing, so EL1 must be given a defined starting point before the
 * `eret`: an inherited `EE` (big-endian), `A` or `M` bit would fault on the
 * first EL1 instruction with VBAR_EL1 not yet installed. */
.equ SCTLR_EL1_RES1, 0x30D00800

/* Load the address of a symbol PC-relatively (+/- 4 GiB). */
.macro ADR_ABS reg, symbol
    adrp    \reg, \symbol
    add     \reg, \reg, #:lo12:\symbol
.endm

/* ------------------------------------------------------------------------
 * Early boot trace (`boot-trace` feature).
 *
 * `TRACE_INIT` programs GPIO14/15 and the PL011 from scratch; `TRACE` then
 * emits single characters straight to the data register. Nothing here relies
 * on the firmware having left UART0 usable: the Raspberry Pi firmware
 * guarantees no particular UART state at kernel entry.
 *
 * The result is output from the very first instruction of the reset path:
 * before FP/SIMD is ungated, before the MMU, and before any Rust runs.
 *
 * `TRACE` clobbers x9-x12, touches no stack and uses no `bl`, so it is safe to
 * drop in anywhere below. Its FIFO wait is bounded so that a misconfigured
 * UART cannot itself masquerade as a hang.
 * ---------------------------------------------------------------------- */
.set BOOT_TRACE_ON, {CONST_BOOT_TRACE}

/* 8N1 at 115200 from the 48 MHz UART clock pinned by `init_uart_clock`:
 * 48e6 / (16 * 115200) = 26.0417 -> IBRD 26, FBRD round(0.0417 * 64) = 3. */
.equ PL011_IBRD_115200, 26
.equ PL011_FBRD_115200, 3

.macro TRACE_INIT
.if BOOT_TRACE_ON
    /* Core 0 only: a secondary arriving later must not disable the UART in
     * the middle of core 0's transmission. */
    mrs     x9, MPIDR_EL1
    and     x9, x9, #{CONST_CORE_ID_MASK}
    cbnz    x9, 9990f

    movz    x10, #((({CONST_UART0_BASE}) >> 16) & 0xFFFF), lsl #16
    movk    x10, #(({CONST_UART0_BASE}) & 0xFFFF)
    sub     x11, x10, #0x1000           /* the GPIO block sits 4 KiB below */

    str     wzr, [x10, #0x30]           /* CR = 0: off while reprogrammed */

    /* GPIO14 (TXD0) and GPIO15 (RXD0) -> ALT0 (0b100). */
    ldr     w12, [x11, #0x04]           /* GPFSEL1 */
    bic     w12, w12, #0x3F000          /* clear FSEL14 and FSEL15 */
    movz    w13, #0x4000
    movk    w13, #0x2, lsl #16          /* (0b100 << 12) | (0b100 << 15) */
    orr     w12, w12, w13
    str     w12, [x11, #0x04]

    /* Detach both pins from the pull-up/pull-down network. */
    str     wzr, [x11, #0x94]           /* GPPUD = off */
    mov     w13, #150
9991:
    subs    w13, w13, #1
    b.ne    9991b
    movz    w13, #0xC000                /* clock GPIO14 and GPIO15 */
    str     w13, [x11, #0x98]           /* GPPUDCLK0 */
    mov     w13, #150
9992:
    subs    w13, w13, #1
    b.ne    9992b
    str     wzr, [x11, #0x98]

    movz    w13, #0x7FF
    str     w13, [x10, #0x44]           /* ICR: clear every interrupt */
    mov     w13, #PL011_IBRD_115200
    str     w13, [x10, #0x24]           /* IBRD */
    mov     w13, #PL011_FBRD_115200
    str     w13, [x10, #0x28]           /* FBRD */
    mov     w13, #0x70                  /* WLEN 8, FEN */
    str     w13, [x10, #0x2C]           /* LCRH */
    str     wzr, [x10, #0x38]           /* IMSC: no interrupts */
    movz    w13, #0x301                 /* UARTEN | TXE | RXE */
    str     w13, [x10, #0x30]           /* CR */
9990:
.endif
.endm

/* ------------------------------------------------------------------------
 * ACT LED blink, GPIO 29 on both the Pi 3B+ and the Pi Zero 2 W.
 *
 * A liveness signal independent of the UART: blinks with no serial output mean
 * the reset path is executing and the fault is in the UART configuration;
 * neither appearing means the kernel was never entered. GPIO 29 is internal on
 * both boards, so driving it cannot disturb the 40-pin header.
 *
 * Clobbers x9, x10, x12, w13, x14, w15. Core 0 only, no stack, no `bl`.
 * ---------------------------------------------------------------------- */
.macro LED_BLINK count
.if BOOT_TRACE_ON
    mrs     x9, MPIDR_EL1
    and     x9, x9, #{CONST_CORE_ID_MASK}
    cbnz    x9, 9995f

    movz    x10, #((({CONST_UART0_BASE}) >> 16) & 0xFFFF), lsl #16
    movk    x10, #(({CONST_UART0_BASE}) & 0xFFFF)
    sub     x12, x10, #0x1000           /* GPIO base */

    ldr     w13, [x12, #0x08]           /* GPFSEL2 */
    bic     w13, w13, #0x38000000       /* clear FSEL29 */
    orr     w13, w13, #0x08000000       /* FSEL29 = 0b001: output */
    str     w13, [x12, #0x08]

    movz    w13, #0x2000, lsl #16       /* GPIO 29 */
    mov     w15, #\count
9994:
    str     w13, [x12, #0x1C]           /* GPSET0 */
    movz    x14, #0x600, lsl #16
9996:
    subs    x14, x14, #1
    b.ne    9996b
    str     w13, [x12, #0x28]           /* GPCLR0 */
    movz    x14, #0x600, lsl #16
9997:
    subs    x14, x14, #1
    b.ne    9997b
    subs    w15, w15, #1
    b.ne    9994b
9995:
.endif
.endm

/* Emits the low byte of \reg (a w-register, not w10/w11/w12) to the PL011, so
 * the boot path can report a value rather than a fixed marker. */
.macro TRACE_REG reg
.if BOOT_TRACE_ON
    movz    x9, #((({CONST_UART0_BASE}) >> 16) & 0xFFFF), lsl #16
    movk    x9, #(({CONST_UART0_BASE}) & 0xFFFF)
    movz    w12, #0x20, lsl #16
9986:
    ldr     w10, [x9, #0x18]            /* FR */
    tbz     w10, #5, 9987f
    subs    w12, w12, #1
    b.ne    9986b
9987:
    str     \reg, [x9]                  /* DR */
.endif
.endm

.macro TRACE ch
.if BOOT_TRACE_ON
    movz    x9, #((({CONST_UART0_BASE}) >> 16) & 0xFFFF), lsl #16
    movk    x9, #(({CONST_UART0_BASE}) & 0xFFFF)
    movz    w12, #0x20, lsl #16         /* bound the wait: ~2M polls */
9998:
    ldr     w10, [x9, #0x18]            /* FR */
    tbz     w10, #5, 9999f              /* TXFF clear: the FIFO has room */
    subs    w12, w12, #1
    b.ne    9998b
9999:
    mov     w11, #\ch
    str     w11, [x9]                   /* DR */
.endif
.endm

.section .text.boot, "ax"
.global _start
.type _start, %function

_start:
    /* Mask every asynchronous exception until VBAR_EL1 is installed. */
    msr     DAIFSet, #0b1111
    TRACE_INIT
    TRACE   0x31                        /* '1' reset entry */
    TRACE   0x41                        /* 'A' past the LED blink */

    /* x19 = core id. Aff1 is always 0 on the BCM2837, so masking Aff0 with
     * 0b11 is what the firmware stub and QEMU's trampoline both do. */
    mrs     x19, MPIDR_EL1
    and     x19, x19, #{CONST_CORE_ID_MASK}

    /* x20 = __stacks_end - core_id * KERNEL_STACK_SIZE.
     *
     * The stacks live in a NOLOAD section past .bss, so installing SP here is
     * safe even though .bss has not been zeroed yet. */
    ADR_ABS x0, __stacks_end
    mov     x1, #{CONST_KERNEL_STACK_SIZE}
    mul     x1, x1, x19
    sub     x20, x0, x1
    mov     sp, x20
    TRACE   0x42                        /* 'B' stack installed */

    /* ------------------------------------------------------------------
     * Normalise the exception level to EL1h. Hardware and QEMU both enter at
     * EL2; the EL3 path costs four instructions and also makes the kernel
     * bootable from a custom armstub.
     * ----------------------------------------------------------------- */
    mrs     x0, CurrentEL
    lsr     w13, w0, #2
    add     w13, w13, #0x30             /* '1', '2' or '3' */
    TRACE_REG w13
    cmp     x0, #CURRENTEL_EL3
    b.eq    .L_el3
    cmp     x0, #CURRENTEL_EL2
    b.eq    .L_el2
    cmp     x0, #CURRENTEL_EL1
    b.eq    .L_el1
    b       .L_park                     /* EL0: impossible, but never fall through */

.L_el3:
    TRACE   0x43                        /* 'C' entered at EL3 */
    mov     x0, #SCR_EL3_VALUE
    msr     SCR_EL3, x0
    msr     CPTR_EL3, xzr               /* do not trap FP/SIMD to EL3 */

    /* Cortex-A53 CPUECTLR_EL1.SMPEN: join the coherency domain and enable
     * broadcast of TLB/cache maintenance. Done here and nowhere else, because
     * EL3 is the only level with unconditional access (see .L_el1). The Pi's
     * armstub8 already sets SMPEN on every core before dropping to EL2, so only
     * a custom stub reaches this. */
    mrs     x0, S3_1_C15_C2_1
    orr     x0, x0, #(1 << 6)
    msr     S3_1_C15_C2_1, x0
    isb
    mov     x0, #SPSR_EL2H_MASKED
    msr     SPSR_EL3, x0
    ADR_ABS x0, .L_el2
    msr     ELR_EL3, x0
    eret

.L_el2:
    TRACE   0x32                        /* '2' running at EL2 */
    mov     sp, x20                     /* SP_EL2 */

    /* EL1 runs AArch64. */
    mov     x0, #(1 << 31)              /* HCR_EL2.RW */
    msr     HCR_EL2, x0

    /* Do not trap FP/SIMD (CPTR_EL2.TFP) or SVE (TZ) from EL1. */
    mov     x0, #CPTR_EL2_NO_TRAPS
    msr     CPTR_EL2, x0

    /* Let EL1 read the physical/virtual counters, and make CNTVCT_EL0 track
     * CNTPCT_EL0 so time.rs can use either. */
    mrs     x0, CNTHCTL_EL2
    orr     x0, x0, #0b11               /* EL1PCTEN | EL1PCEN */
    msr     CNTHCTL_EL2, x0
    msr     CNTVOFF_EL2, xzr

    /* Give EL1 a defined SCTLR before entering it. */
    movz    x0, #(SCTLR_EL1_RES1 & 0xFFFF)
    movk    x0, #((SCTLR_EL1_RES1 >> 16) & 0xFFFF), lsl #16
    msr     SCTLR_EL1, x0
    isb

    mov     x0, #SPSR_EL1H_MASKED
    msr     SPSR_EL2, x0
    ADR_ABS x0, .L_el1
    msr     ELR_EL2, x0
    eret

.L_el1:
    TRACE   0x33                        /* '3' dropped to EL1 */
    mov     sp, x20                     /* SP_EL1, SPSel == 1 */

    /* ------------------------------------------------------------------
     * Ungate FP/SIMD. MUST happen before any compiler-generated code runs: on
     * `aarch64-unknown-none` the V registers are part of the ABI and LLVM
     * emits `ldp q0, q1` for ordinary memory copies. Executing one with
     * CPACR_EL1.FPEN == 0 traps with ESR EC=0x07.
     * ----------------------------------------------------------------- */
    mrs     x0, CPACR_EL1
    orr     x0, x0, #(0b11 << 20)       /* FPEN = 0b11: no trapping at EL0/EL1 */
    msr     CPACR_EL1, x0
    isb
    TRACE   0x34                        /* '4' FPEN set, no FP executed yet */

    /* Deterministic FP control/status now that the unit is reachable. */
    msr     FPCR, xzr
    msr     FPSR, xzr
    TRACE   0x35                        /* '5' first FP register access survived */

    /* ------------------------------------------------------------------
     * CPUECTLR_EL1.SMPEN is deliberately NOT written here.
     *
     * It is IMPLEMENTATION DEFINED, and on the Cortex-A53 access from EL1 is
     * gated by ACTLR_EL2. The Raspberry Pi's armstub8 sets ACTLR_EL3 but leaves
     * ACTLR_EL2 at zero, so an `mrs` here traps to EL2 -- where VBAR_EL2 is
     * still the stub's -- and the core vanishes into firmware with no output.
     * QEMU does not model the gating, so this reproduces only on silicon.
     *
     * It is also unnecessary: armstub8 already sets SMPEN on every core before
     * dropping to EL2. The EL3 path above covers the custom-stub case.
     * ----------------------------------------------------------------- */
    TRACE   0x36                        /* '6' past the SMPEN decision */

    /* ------------------------------------------------------------------
     * Split by role *before* any cache maintenance.
     *
     * The A53's L1 data cache is private per core, but the L2 is shared by all
     * four. `dc isw` invalidates *without* cleaning, so a secondary walking
     * every level would silently discard core 0's dirty L2 lines, and
     * secondaries are released long after core 0 is live. Core 0 alone walks to
     * the Level of Coherency; secondaries stop at L1 and also skip .bss
     * zeroing, which core 0 already did.
     * ----------------------------------------------------------------- */
    cbnz    x19, .L_secondary

    /* Discard anything the firmware left in the data caches before writing
     * translation tables with the MMU still off. */
    bl      __dcache_invalidate_all
    ic      iallu
    dsb     nsh
    isb
    TRACE   0x37                        /* '7' caches invalidated */

    /* ------------------------------------------------------------------
     * Core 0: zero .bss. The linker aligns both bounds to 16 bytes.
     * ----------------------------------------------------------------- */
    ADR_ABS x0, __bss_start
    ADR_ABS x1, __bss_end
.L_bss_loop:
    cmp     x0, x1
    b.hs    .L_bss_done
    stp     xzr, xzr, [x0], #16
    b       .L_bss_loop
.L_bss_done:

    TRACE   0x38                        /* '8' entering Rust */
    mov     x0, x19
    b       kernel_boot_primary

.L_secondary:
    /* L1 only -- the shared L2 belongs to core 0 by now. See the role split. */
    bl      __dcache_invalidate_l1
    ic      iallu
    dsb     nsh
    isb
    mov     x0, x19
    b       kernel_boot_secondary

.L_park:
    TRACE   0x50                        /* 'P' parked: unexpected EL */
    msr     DAIFSet, #0b1111
1:
    wfe
    b       1b

.size  _start, . - _start
