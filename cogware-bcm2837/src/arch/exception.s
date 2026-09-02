// SPDX-License-Identifier: MIT OR Apache-2.0
// AArch64 EL1 exception vector table and trap entry/exit.
//
// The layout below must match `TrapFrame` in `exception.rs` exactly:
//
//   0   .. 248   x0 - x30                    (31 * 8)
//   248 .. 256   ELR_EL1
//   256 .. 264   SPSR_EL1
//   264 .. 272   ESR_EL1
//   272 .. 280   FAR_EL1
//   280 .. 288   SP_EL0
//   288 .. 296   kind
//   296 .. 304   padding (keeps the FP area 16-byte aligned)
//   304 .. 312   FPSR
//   312 .. 320   FPCR
//   320 .. 832   q0 - q31                    (32 * 16)
//
// FPSR/FPCR sit before the vector registers because 64-bit `stp` encodes a
// scaled signed 7-bit offset (+-504): #816 would not fit, #304 does. The
// 128-bit `stp` scale is 16, so the highest vector offset used, #800, is also
// encodable.
//
// The target `aarch64-unknown-none` is hard-float with NEON enabled, and stable
// Rust cannot disable NEON for a single function, so the handler must save the
// entire FP/SIMD state.

.macro VECTOR_ENTRY kind
    .balign 128
    sub  sp, sp, #832
    stp  x0, x1, [sp, #0]
    mov  x0, #\kind
    b    __trap_entry
.endm

.section .text.exception, "ax", %progbits

.balign 2048
.global __exception_vector_table
.type __exception_vector_table, %function
__exception_vector_table:

    // Current EL, SP_EL0 (SPSel == 0). Never taken: the kernel runs on SP_EL1.
    VECTOR_ENTRY 0      // Synchronous
    VECTOR_ENTRY 1      // IRQ
    VECTOR_ENTRY 2      // FIQ
    VECTOR_ENTRY 3      // SError

    // Current EL, SP_ELx (SPSel == 1). Where everything lands.
    VECTOR_ENTRY 4      // Synchronous
    VECTOR_ENTRY 5      // IRQ
    VECTOR_ENTRY 6      // FIQ
    VECTOR_ENTRY 7      // SError

    // Lower EL, AArch64. Unused: nothing runs at EL0.
    VECTOR_ENTRY 8      // Synchronous
    VECTOR_ENTRY 9      // IRQ
    VECTOR_ENTRY 10     // FIQ
    VECTOR_ENTRY 11     // SError

    // Lower EL, AArch32. Unused: the kernel never drops to 32-bit.
    VECTOR_ENTRY 12     // Synchronous
    VECTOR_ENTRY 13     // IRQ
    VECTOR_ENTRY 14     // FIQ
    VECTOR_ENTRY 15     // SError

.size __exception_vector_table, . - __exception_vector_table

// On entry: sp already lowered by 832, x0/x1 saved at [sp, #0], x0 == kind.
.type __trap_entry, %function
__trap_entry:
    stp  x2,  x3,  [sp, #16]
    stp  x4,  x5,  [sp, #32]
    stp  x6,  x7,  [sp, #48]
    stp  x8,  x9,  [sp, #64]
    stp  x10, x11, [sp, #80]
    stp  x12, x13, [sp, #96]
    stp  x14, x15, [sp, #112]
    stp  x16, x17, [sp, #128]
    stp  x18, x19, [sp, #144]
    stp  x20, x21, [sp, #160]
    stp  x22, x23, [sp, #176]
    stp  x24, x25, [sp, #192]
    stp  x26, x27, [sp, #208]
    stp  x28, x29, [sp, #224]
    str  x30,      [sp, #240]

    str  x0,       [sp, #288]           // kind

    mrs  x1, ELR_EL1
    mrs  x2, SPSR_EL1
    stp  x1, x2,   [sp, #248]

    mrs  x1, ESR_EL1
    mrs  x2, FAR_EL1
    stp  x1, x2,   [sp, #264]

    mrs  x1, SP_EL0
    str  x1,       [sp, #280]

    mrs  x1, FPSR
    mrs  x2, FPCR
    stp  x1, x2,   [sp, #304]

    stp  q0,  q1,  [sp, #320]
    stp  q2,  q3,  [sp, #352]
    stp  q4,  q5,  [sp, #384]
    stp  q6,  q7,  [sp, #416]
    stp  q8,  q9,  [sp, #448]
    stp  q10, q11, [sp, #480]
    stp  q12, q13, [sp, #512]
    stp  q14, q15, [sp, #544]
    stp  q16, q17, [sp, #576]
    stp  q18, q19, [sp, #608]
    stp  q20, q21, [sp, #640]
    stp  q22, q23, [sp, #672]
    stp  q24, q25, [sp, #704]
    stp  q26, q27, [sp, #736]
    stp  q28, q29, [sp, #768]
    stp  q30, q31, [sp, #800]

    mov  x0, sp                         // &mut TrapFrame
    bl   __trap_handler

    ldp  q0,  q1,  [sp, #320]
    ldp  q2,  q3,  [sp, #352]
    ldp  q4,  q5,  [sp, #384]
    ldp  q6,  q7,  [sp, #416]
    ldp  q8,  q9,  [sp, #448]
    ldp  q10, q11, [sp, #480]
    ldp  q12, q13, [sp, #512]
    ldp  q14, q15, [sp, #544]
    ldp  q16, q17, [sp, #576]
    ldp  q18, q19, [sp, #608]
    ldp  q20, q21, [sp, #640]
    ldp  q22, q23, [sp, #672]
    ldp  q24, q25, [sp, #704]
    ldp  q26, q27, [sp, #736]
    ldp  q28, q29, [sp, #768]
    ldp  q30, q31, [sp, #800]

    ldp  x1, x2,   [sp, #304]
    msr  FPSR, x1
    msr  FPCR, x2

    ldp  x1, x2,   [sp, #248]
    msr  ELR_EL1, x1
    msr  SPSR_EL1, x2

    ldr  x1,       [sp, #280]
    msr  SP_EL0, x1

    ldp  x0,  x1,  [sp, #0]
    ldp  x2,  x3,  [sp, #16]
    ldp  x4,  x5,  [sp, #32]
    ldp  x6,  x7,  [sp, #48]
    ldp  x8,  x9,  [sp, #64]
    ldp  x10, x11, [sp, #80]
    ldp  x12, x13, [sp, #96]
    ldp  x14, x15, [sp, #112]
    ldp  x16, x17, [sp, #128]
    ldp  x18, x19, [sp, #144]
    ldp  x20, x21, [sp, #160]
    ldp  x22, x23, [sp, #176]
    ldp  x24, x25, [sp, #192]
    ldp  x26, x27, [sp, #208]
    ldp  x28, x29, [sp, #224]
    ldr  x30,      [sp, #240]

    add  sp, sp, #832
    eret

.size __trap_entry, . - __trap_entry
