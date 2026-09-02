/* SPDX-License-Identifier: MIT OR Apache-2.0
 *
 * cogware-bcm2837 — linker script for the BCM2837 (Raspberry Pi 3).
 *
 * The Raspberry Pi firmware loads `kernel8.img` at physical 0x80000 and enters
 * it at offset 0 in AArch64 mode. QEMU's `raspi3b` machine uses the same load
 * address, so one script serves both.
 *
 * Layout (all identity mapped, see src/arch/mmu.rs):
 *
 *   0x00000000 .. 0x00080000   firmware stub + SMP spin table (never touched)
 *   0x00080000 .. __text_end   .text                            RX, read-only
 *              .. __rodata_end .rodata + .eh_frame               RO, no-execute
 *              .. __data_end   .data + .bss (incl. page tables)  RW, no-execute
 *              .. __stacks_end per-core stacks                   RW, no-execute
 *
 * Section starts are page aligned so that the MMU can hand out per-section
 * permissions with 4 KiB granularity.
 */

__kernel_load_addr = 0x80000;

PAGE_SIZE         = 4K;
KERNEL_STACK_SIZE = 128K;   /* must match bsp::memory::KERNEL_STACK_SIZE */
CORE_COUNT        = 4;      /* must match bsp::memory::CORE_COUNT        */

ENTRY(_start)

PHDRS
{
    text   PT_LOAD FLAGS(5);   /* R-X */
    rodata PT_LOAD FLAGS(4);   /* R-- */
    data   PT_LOAD FLAGS(6);   /* RW- */
}

SECTIONS
{
    . = __kernel_load_addr;
    __kernel_start = .;

    /* --------------------------------------------------------------------
     * Code
     * ----------------------------------------------------------------- */
    .text : ALIGN(PAGE_SIZE)
    {
        __text_start = .;
        KEEP(*(.text.boot))     /* _start must be the very first byte */
        *(.text .text.*)
        . = ALIGN(PAGE_SIZE);
        __text_end = .;
    } :text

    /* --------------------------------------------------------------------
     * Read-only data. `.eh_frame` is kept (not discarded) so that GDB over
     * JTAG can unwind through optimised frames.
     * ----------------------------------------------------------------- */
    .rodata : ALIGN(PAGE_SIZE)
    {
        __rodata_start = .;
        *(.rodata .rodata.*)
        . = ALIGN(8);
        __eh_frame_start = .;
        KEEP(*(.eh_frame))
        KEEP(*(.eh_frame_hdr))
        __eh_frame_end = .;
        . = ALIGN(PAGE_SIZE);
        __rodata_end = .;
    } :rodata

    /* --------------------------------------------------------------------
     * Read/write data. The translation tables live in .bss (they are
     * `static` arrays of zeroes that boot code fills in before the MMU is
     * switched on), so nothing extra is needed here.
     * ----------------------------------------------------------------- */
    .data : ALIGN(PAGE_SIZE)
    {
        __data_start = .;
        *(.data .data.*)
        . = ALIGN(16);
    } :data

    .bss (NOLOAD) : ALIGN(16)
    {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(16);
        __bss_end = .;
    } :data

    . = ALIGN(PAGE_SIZE);
    __data_end = .;

    /* --------------------------------------------------------------------
     * Per-core stacks.
     *
     * NOLOAD and placed *after* .bss on purpose: the boot code installs SP
     * before it zeroes .bss, so the stacks must not be part of the region
     * that gets wiped.
     *
     * Stacks grow downwards. Core N owns
     *     (__stacks_end - (N+1)*KERNEL_STACK_SIZE) ..= (__stacks_end - N*KERNEL_STACK_SIZE)
     * ----------------------------------------------------------------- */
    .stacks (NOLOAD) : ALIGN(PAGE_SIZE)
    {
        __stacks_start = .;
        . += KERNEL_STACK_SIZE * CORE_COUNT;
        __stacks_end = .;
    } :data

    . = ALIGN(PAGE_SIZE);
    __kernel_end = .;

    /* --------------------------------------------------------------------
     * Sanity checks
     * ----------------------------------------------------------------- */
    .got : { *(.got .got.plt) }
    ASSERT(SIZEOF(.got) == 0,
           "GOT is not empty: the kernel must be linked position-dependent")

    ASSERT(__kernel_end <= 16M,
           "Kernel image outgrew the 4 KiB-granular MMU window (arch::mmu::L3_TABLE_COUNT)")

    /DISCARD/ :
    {
        *(.comment)
        *(.note .note.*)
        *(.dynamic) *(.dynsym) *(.dynstr) *(.hash) *(.gnu.hash)
        *(.interp)
    }
}
