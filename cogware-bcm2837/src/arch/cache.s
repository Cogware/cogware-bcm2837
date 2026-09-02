/* SPDX-License-Identifier: MIT OR Apache-2.0
 *
 * Cortex-A53 cache maintenance: the standard ARMv8-A "operate on the whole
 * data cache by set/way" routine, as used by U-Boot and TF-A. Written in
 * assembly because it walks CLIDR_EL1/CCSIDR_EL1 with dynamically computed
 * set/way encodings.
 *
 * Four entry points share one walk, differing in how deep they go and whether
 * they clean before invalidating.
 *
 *                              depth              dirty lines
 *   __dcache_invalidate_all    Level of Coherency discarded
 *   __dcache_invalidate_l1     level 0 only       discarded
 *   __dcache_clean_invalidate_all  LoC            written back
 *   __dcache_clean_invalidate_l1   level 0 only   written back
 *
 * The discarding variants are only safe at reset, before anything of value has
 * been written. Elsewhere -- especially before handing memory to a core running
 * with its caches off -- use the cleaning variants, or dirty lines are evicted
 * over the top of whatever replaced them.
 */

.section .text

/* ---------------------------------------------------------------------------
 * fn __dcache_invalidate_all()
 *
 * Invalidates without cleaning every data cache level up to the Level of
 * Coherency; dirty lines are discarded, so this is only safe at reset.
 *
 * On the Cortex-A53 the L2 is shared by all four cores, so run this on the
 * primary only, before the secondaries are released: a secondary calling it
 * while core 0 is live would discard core 0's dirty L2 lines. Secondaries call
 * __dcache_invalidate_l1 instead.
 *
 * Clobbers x0-x11, x16, x17. Leaf function, no stack use.
 * ------------------------------------------------------------------------ */
.global __dcache_invalidate_all
.type __dcache_invalidate_all, %function
__dcache_invalidate_all:
    mov     w6, #0                      /* invalidate only          */
    b       .L_dcache_loc
.size __dcache_invalidate_all, . - __dcache_invalidate_all

/* ---------------------------------------------------------------------------
 * fn __dcache_clean_invalidate_all()
 *
 * Writes every dirty line back to the point of coherency and invalidates it,
 * for all levels up to the Level of Coherency.
 *
 * Required before a core disables its data cache and leaves memory to somebody
 * else: disabling the cache does not empty it, and the eventual eviction of a
 * still-dirty line lands on whatever occupies that address by then.
 *
 * Touches the shared L2, so run it only when the other cores are parked with
 * their caches off.
 *
 * Clobbers x0-x11, x16, x17. Leaf function, no stack use.
 * ------------------------------------------------------------------------ */
.global __dcache_clean_invalidate_all
.type __dcache_clean_invalidate_all, %function
__dcache_clean_invalidate_all:
    mov     w6, #1                      /* clean, then invalidate   */
.L_dcache_loc:
    mrs     x0, CLIDR_EL1
    and     w3, w0, #0x07000000         /* LoC in bits [26:24]      */
    lsr     w3, w3, #23                 /* w3 = LoC * 2             */
    b       .L_dcache_op
.size __dcache_clean_invalidate_all, . - __dcache_clean_invalidate_all

/* ---------------------------------------------------------------------------
 * fn __dcache_invalidate_l1()
 *
 * As __dcache_invalidate_all, but stops after level 0. The L1 data cache is
 * private to the calling core, so this is safe on a secondary while the other
 * cores are already executing out of the shared L2.
 *
 * Clobbers x0-x11, x16, x17. Leaf function, no stack use.
 * ------------------------------------------------------------------------ */
.global __dcache_invalidate_l1
.type __dcache_invalidate_l1, %function
__dcache_invalidate_l1:
    mov     w6, #0                      /* invalidate only          */
    b       .L_dcache_l1
.size __dcache_invalidate_l1, . - __dcache_invalidate_l1

/* ---------------------------------------------------------------------------
 * fn __dcache_clean_invalidate_l1()
 *
 * Cleans and invalidates the calling core's private L1 data cache.
 *
 * The counterpart of __dcache_clean_invalidate_all for a secondary: makes this
 * core's dirty lines visible to the rest of the cluster without touching the
 * shared L2, so it is safe while another core is still running.
 *
 * Clobbers x0-x11, x16, x17. Leaf function, no stack use.
 * ------------------------------------------------------------------------ */
.global __dcache_clean_invalidate_l1
.type __dcache_clean_invalidate_l1, %function
__dcache_clean_invalidate_l1:
    mov     w6, #1                      /* clean, then invalidate   */
.L_dcache_l1:
    mrs     x0, CLIDR_EL1
    and     w3, w0, #0x07000000         /* LoC in bits [26:24]      */
    lsr     w3, w3, #23                 /* w3 = LoC * 2             */
    mov     w4, #2
    cmp     w3, w4
    csel    w3, w3, w4, lt              /* w3 = min(LoC * 2, 2)     */
    /* fall through */

/* w0 = CLIDR_EL1, w3 = 2 * deepest level to touch, w6 = clean flag. */
.L_dcache_op:
    cbz     w3, 5f                      /* no cache at all          */

    mov     w10, #0                     /* w10 = 2 * current level  */
    mov     w8, #1
1:
    add     w2, w10, w10, lsr #1        /* w2 = 3 * level           */
    lsr     w1, w0, w2
    and     w1, w1, #0x7                /* w1 = cache type          */
    cmp     w1, #2
    b.lt    4f                          /* no data cache here       */

    msr     CSSELR_EL1, x10             /* select this cache level  */
    isb
    mrs     x1, CCSIDR_EL1
    and     w2, w1, #7                  /* log2(line bytes) - 4     */
    add     w2, w2, #4
    ubfx    w4, w1, #3, #10             /* max way number           */
    clz     w5, w4                      /* bit position of way field*/
    lsl     w9, w4, w5
    lsl     w16, w8, w5
2:
    ubfx    w7, w1, #13, #15            /* max set number           */
3:
    lsl     w17, w7, w2
    orr     w11, w10, w9
    orr     w11, w11, w17
    cbz     w6, 6f
    dc      cisw, x11                   /* clean and invalidate     */
    b       7f
6:
    dc      isw, x11                    /* invalidate only          */
7:
    subs    w7, w7, #1
    b.ge    3b
    subs    x9, x9, x16
    b.ge    2b
4:
    add     w10, w10, #2
    cmp     w3, w10
    b.gt    1b
5:
    mov     x10, #0
    msr     CSSELR_EL1, x10
    dsb     sy
    isb
    ret
.size __dcache_clean_invalidate_l1, . - __dcache_clean_invalidate_l1