# 17 — Floating point and `math`

The Cortex-A53's integrated FPU is the reason this project targets
`aarch64-unknown-none` rather than `aarch64-unknown-none-softfloat`. Two other
documents cover the machinery that makes it usable:

* [`docs/02-boot-and-exception-levels.md`](02-boot-and-exception-levels.md) —
  ungating FP/SIMD (`CPACR_EL1.FPEN`) before any compiler-generated Rust runs.
* [`docs/07-exceptions-and-irq.md`](07-exceptions-and-irq.md) — saving the whole
  FP/SIMD register file, `FPSR` and `FPCR` on exception entry.

This document is about what you can actually *call*.

---

## 1. The gap in `core`

`core` and `std` do not expose the same `f32`/`f64` API, and the split is not
arbitrary: `core` stops where a general target would need a call into libm.

| Available in `core` | `std` only |
| --- | --- |
| arithmetic, comparison, `NaN`/`INFINITY`, `to_bits`/`from_bits` | `sqrt` |
| `abs`, `signum`, `copysign` | `floor`, `ceil`, `trunc`, `round`, `round_ties_even` |
| `min`, `max`, `clamp` | `mul_add` |
| `recip`, `to_degrees`, `to_radians` | `sin`, `cos`, `ln`, `exp`, `powf`, … |

On a general target the whole right-hand column is a libm call. On an A53
everything in the first two `std`-only rows is a **single instruction**. Only
the transcendentals genuinely need software.

That is the whole design of [`math`](../cogware-bcm2837/src/math.rs): expose
the instructions the hardware has, and nothing else.

---

## 2. Using it

`Float` is an extension trait, so the methods read exactly like their `std`
counterparts once it is in scope:

```rust
use cogware_bcm2837::math::Float;

let distance = (dx * dx + dy * dy).sqrt();
let rows     = (height / line_height).ceil();
let acc      = sample.mul_add(gain, acc);
```

It is implemented for `f32` and `f64` and sealed, so it cannot be implemented
for anything else. Every method is `#[inline]` and lowers to one instruction —
there is no call to hoist out of a loop.

---

## 3. Instruction mapping

| Method | `f64` | `f32` | Notes |
| --- | --- | --- | --- |
| `sqrt` | `fsqrt d` | `fsqrt s` | correctly rounded; negative → NaN; cannot fault |
| `floor` | `frintm d` | `frintm s` | toward −∞ |
| `ceil` | `frintp d` | `frintp s` | toward +∞ |
| `trunc` | `frintz d` | `frintz s` | toward zero |
| `round` | `frinta d` | `frinta s` | nearest, ties **away from zero** |
| `round_ties_even` | `frintn d` | `frintn s` | nearest, ties **to even** |
| `mul_add` | `fmadd d` | `fmadd s` | one rounding, not two |
| `fract` | — | — | `self - self.trunc()`, so one `frintz` and one `fsub` |

All of them match the `std` method of the same name, including the IEEE-754
special cases, so code written against `std` behaves identically here.

### `round` vs `round_ties_even`

They differ only at exact halves, and the difference matters when summing many
rounded values — `round` is biased away from zero, `round_ties_even` is not:

```text
x = -2.5
  floor            -3
  ceil             -2
  trunc            -2
  round            -3     ties away from zero
  round_ties_even  -2     ties to even
```

`round_ties_even` is the IEEE-754 default and the better choice for anything
accumulating. `round` exists because it is what `std::f64::round` does and
copied code expects it.

### Why `mul_add` is not just `a * b + c`

`fmadd` computes the product at full width and rounds **once**, at the end.
Written out, the product is rounded first and the sum again. On
`(1 + 2⁻²⁷)² − 1` the difference is visible:

```text
mul_add fused    : 1.4901161249358807e-8
naive a*a-1      : 1.4901161193847656e-8
```

The fused form is the correct one. Reach for it in dot products, polynomial
evaluation and anything iterative, where the error would otherwise compound.

---

## 4. What is deliberately absent

There is no `sin`, `ln`, `powf`, `atan2` or friends.

The A53 has no instruction for any of them, so providing them would mean
shipping a software implementation — and the accuracy, code size and table
storage trade-offs of that implementation belong to the application, not to a
board support package. A graphics routine wanting a fast approximate sine and a
control loop wanting a correctly rounded one want different code, and a BSP
that picks for them is in the way.

Rule 1.1 rules out depending on `libm`, so if you need transcendentals, write
the approximation you need in your own crate. `mul_add` makes Horner-form
polynomial evaluation both fast and accurate, which is what such an
implementation would be built on.

---

## 5. Verifying it works

The FPU self-test in `examples/shell` (`self_test_fpu`) runs on every core: it
multiplies by π, takes a square root and squares it back, asserting the round
trip. It uses `black_box` so the optimiser cannot fold the whole thing to a
constant — without that, a passing test would prove nothing about the hardware.

A downstream crate can check the rounding family against known-correct answers.
Use `-2.5`: it is the only value in a typical test set where `round` and
`round_ties_even` disagree.

## Confirmed on silicon

The hard-float decision is no longer QEMU-only. On a Pi Zero 2 W, at EL1 with
the MMU and both caches enabled, the `hello-uart` example reports:

```text
  FP/SIMD         : yes
  FPU check       : H(4) = 2.083333333333333
```

That is the fourth harmonic number, `1 + 1/2 + 1/3 + 1/4`, computed in hardware
double precision and formatted on the target. The `multicore` example gets the
same value on all four cores.

This matters because the older generation of Raspberry Pi kernels -- including
the reference tutorials this crate was written against -- use
`aarch64-unknown-none-softfloat`, and the folklore explanation is that the FPU
is somehow unavailable or unreliable during early boot. It is not. What is
fragile is the *ordering*: `CPACR_EL1.FPEN` must be ungated before any
compiler-generated code runs, because LLVM emits `ldp q0, q1` for ordinary
struct copies at `-O2`. Get that wrong and the first NEON instruction traps
with `ESR_EL1.EC == 0x07`, which is easy to misread as "the FPU does not work".

A boot hang that appeared to implicate the FPU was traced to a
`CPUECTLR_EL1` access at EL1 and had nothing to do with floating point; the
`boot-trace` marker for "first FP register access survived" was reached on
every run. See
[`02-boot-and-exception-levels.md`](02-boot-and-exception-levels.md).

---

## Related

| Document | Covers |
| --- | --- |
| [`docs/02-boot-and-exception-levels.md`](02-boot-and-exception-levels.md) | ungating FP/SIMD before Rust runs |
| [`docs/07-exceptions-and-irq.md`](07-exceptions-and-irq.md) | saving FP state across exceptions |
| [`docs/01-toolchain-and-build.md`](01-toolchain-and-build.md) | why the target is hard-float |
| [`docs/16-using-the-bsp.md`](16-using-the-bsp.md) | consuming the BSP from another crate |
