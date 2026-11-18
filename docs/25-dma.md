# DMA

`cogware-bcm2837/src/drivers/dma.rs` drives the sixteen-channel DMA controller.
A transfer is a chain of 32-byte control blocks in memory; a channel is pointed
at the first and walks the chain until a block's `next` is zero.

## Claiming channels

The firmware keeps some channels. `dma::claim(Kind)` hands out channels from an
atomic mask initialised to `0x7f35`, which is what the Pi 3 firmware leaves the
ARM by default; `dma::probe_channels()` narrows it with the firmware's own
answer (`GET_DMA_CHANNELS`, tag `0x0006_0001`). A `Channel` is exclusively
owned, and dropping it resets the hardware and returns the bit.

Channels 0-6 and 15 are full; 7-14 are lite: half the bandwidth, no 2D mode,
and at most 65 535 bytes per block. `Kind::Any` prefers a full channel. The
safe helpers split long transfers into blocks of `max_length()`.

## Coherency

The engine is outside the ARM coherency domain. Every address it is given is a
bus address — `bus::to_bus` for DRAM, which selects the uncached `0xC000_0000`
alias, and `bus::peripheral` for a register — and every DRAM buffer needs
`mmu::clean_invalidate_range` on both sides of the transfer: before, so the
engine reads what the core wrote and no dirty line is evicted over its output;
after, so the core does not read a line it speculatively refilled meanwhile.
Clean-and-invalidate rather than invalidate, because a buffer's first and last
lines may hold a neighbour's unwritten data.

`copy`, `fill`, `write_word`, `Spi0::transfer_dma` and the EMMC DMA paths all do
this themselves. `Channel::start` and `Channel::submit` do not, and are
`unsafe` for that reason and because the engine keeps using the buffers after
they return.

## Why the scratch block is not in `Channel`

`ControlBlock` must be 32-byte aligned. Embedded in `Channel`, it made the
channel 32-byte aligned, and the executor's task storage is 16-byte aligned,
so any task that held a channel failed to spawn. Each channel's scratch block
lives in a static table instead, indexed by channel number and owned through
the claim bit.

## Paced transfers

A block naming a peripheral sets `SRC_DREQ` or `DEST_DREQ` and a `PERMAP`, so
the engine moves data only while the peripheral asks for it.

* **SPI0** (`Spi0::transfer_dma`): two channels, receive (`DREQ 7`) started
  before transmit (`DREQ 6`), with `DLEN` and `DMAEN` set. Only word-aligned
  buffers whose length is a multiple of four take this path; anything else
  falls back to programmed I/O, because the FIFO's 32-bit accesses would leave
  a partial word.
* **EMMC** (`Emmc::read_blocks_dma`, `write_blocks_dma`): one channel paced by
  `DREQ 11` on the `DATA` register, chunked on block boundaries. The ready
  flags the controller raises along the way are cleared afterwards; a read
  also waits for `DATA_DONE`.
* **PWM**: `Pwm::enable_dma` and `fifo_bus_address` are the pieces; the
  caller builds the chain.
* **DSI1** register writes go through `write_word`, see
  [27-camera-and-display.md](27-camera-and-display.md).

## QEMU ignores DREQ

QEMU's DMA model runs every transfer to completion the moment it is started.
The consequences differ by peripheral:

* EMMC DMA works, because QEMU's SD model refills its data port synchronously.
  `sdcard` writes four blocks by DMA, reads them back by DMA and by PIO, and
  all three agree. That proves the addresses, lengths and cache maintenance,
  not the pacing.
* SPI DMA does not: the receive channel drains an empty FIFO before a byte is
  sent. `buses` therefore runs its known-answer check (the flash's JEDEC ID by
  DMA) last and labels it as needing a board.

Memory-to-memory transfers need no pacing and are fully verified:
`peripherals` copies and fills 16 KiB through a full and a lite channel, and
`interrupts` awaits a completion interrupt.
