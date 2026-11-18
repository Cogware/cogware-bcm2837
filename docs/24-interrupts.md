# Interrupts

`cogware-bcm2837/src/drivers/interrupt.rs` delivers interrupts. Nothing in the
boot path turns them on: a kernel calls `interrupt::init()` once on core 0, then
on each core that wants them `start_tick`, `enable_ipi` and `unmask`. The
`interrupts` example does exactly that on all four cores.

## Three sources

| Source | Where | Reaches |
| --- | --- | --- |
| Virtual timer (`CNTV`) | per core, ARM-local bit 3 | the core it belongs to |
| Mailbox 0 | per core, ARM-local bit 4 | the core it belongs to |
| SoC peripherals, IRQ 0-63 | `0x3F00_B200`, ARM-local bit 8 | core 0 (`GPU_IRQ_ROUTING = 0`) |

The virtual timer, not the physical one, is used for the tick because EL1 can
always program it; boot sets `CNTVOFF_EL2 = 0`, so it reads the same as the
physical counter.

## Handlers never take a lock

A handler runs on top of whatever it interrupted. If that code held a
`SpinLock`, a handler that takes the same lock spins on a holder that cannot run
until the handler returns: one core, deadlocked, and if the lock was `UART` the
board goes silent. So a handler acknowledges its source, updates atomics and
wakes tasks. `TaskSet::wake_all` and the executor's `ready` bits are atomics,
which is why waking a task from a handler is safe.

The handler table is an array of `AtomicUsize` function pointers, as the vector
slot is. An enabled SoC interrupt with no handler is disabled on first arrival
and counted in `spurious_count`, because nothing would acknowledge it and it
would fire forever.

## The tick

`start_tick(hz)` programs `CNTV_CVAL_EL0` one period ahead and routes `CNTV` to
the core's IRQ. Each tick reloads from the previous deadline, not from now, so
the period does not drift by the interrupt latency; but it never schedules into
the past, so a long stall with interrupts masked does not replay the missed
ticks.

The tick needs no executor change. Taking an interrupt wakes the core from
`wfe`, and the idle loop promotes every `poll_again` task after each wake-up,
so a future polling hardware is re-checked once per tick. Before the tick
existed that cadence came only from the architectural event stream.

## IPIs

`send_ipi(core)` writes mailbox 0 of the target core. Its handler clears the
mailbox, counts it and issues `sev`. The executors already wake each other with
`sev`, so IPIs are not needed for that; they are for waking a core parked in
`wfi`, and for a future preemption tick.

## Verified

Under QEMU `raspi3b`, `cargo run --release --example interrupts` reports 20
ticks per core in 200 ms at 100 Hz, 20 system-timer channel-3 matches, one IPI
each to cores 1-3, a DMA completion interrupt waking the task that awaited it,
and no spurious interrupts. Not yet run on hardware.

## IRQ numbers used here

| IRQ | Source |
| --- | --- |
| 1, 3 | system timer channels 1 and 3 (`systimer::Channel::irq`) |
| 16-27 | DMA channels 0-11; 11-14 share 27 (`dma::irq`) |
| 38, 39 | Unicam `CAM0`, `CAM1` |
| 57 | PL011 |
