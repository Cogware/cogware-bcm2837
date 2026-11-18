// SPDX-License-Identifier: GPL-3.0-only
//! The DMA controller: sixteen channels walking chains of control blocks.
//!
//! A transfer is described by a 32-byte [`ControlBlock`] in memory, which
//! names a source, a destination, a length and the next block. Channels 0-6
//! and 15 are full channels; 7-14 are "lite", with half the bandwidth, a 64
//! KiB length limit and no 2D mode. The VideoCore firmware keeps some for
//! itself, so channels are claimed from a mask rather than picked by number.
//!
//! The engine is not in the ARM coherency domain: every address it is given is
//! a bus address, and every buffer it touches needs cache maintenance either
//! side of the transfer. [`Channel::copy`] and [`Channel::fill`] do both;
//! [`Channel::start`] leaves them to the caller.
#![allow(dead_code)]

use core::{
    cell::UnsafeCell,
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicU16, Ordering},
    task::{Context, Poll},
    time::Duration,
};

use super::mmio::{ReadWrite, RegisterBlock};
use crate::{
    arch::{cpu, mmu},
    bsp::memory::{bus, map},
    task::{self, TaskSet},
    time,
};

/// Control block `TI` (transfer information) bits.
pub mod ti {
    /// Raise the channel's interrupt when this block completes.
    pub const INTEN: u32 = 1 << 0;
    /// 2D mode: `length` is `YLENGTH << 16 | XLENGTH`, and `stride` applies.
    /// Full channels only.
    pub const TDMODE: u32 = 1 << 1;
    /// Wait for each write to be acknowledged before the next.
    pub const WAIT_RESP: u32 = 1 << 3;
    /// Increment the destination address after each write.
    pub const DEST_INC: u32 = 1 << 4;
    /// 128-bit destination writes rather than 32-bit.
    pub const DEST_WIDTH_128: u32 = 1 << 5;
    /// Pace writes by the peripheral's data request, see [`super::Dreq`].
    pub const DEST_DREQ: u32 = 1 << 6;
    /// Do not write at all; for draining a peripheral.
    pub const DEST_IGNORE: u32 = 1 << 7;
    /// Increment the source address after each read.
    pub const SRC_INC: u32 = 1 << 8;
    /// 128-bit source reads rather than 32-bit.
    pub const SRC_WIDTH_128: u32 = 1 << 9;
    /// Pace reads by the peripheral's data request.
    pub const SRC_DREQ: u32 = 1 << 10;
    /// Do not read; the destination receives zeroes.
    pub const SRC_IGNORE: u32 = 1 << 11;

    /// The burst length field, in words beyond the first.
    pub const fn burst(words: u32) -> u32 {
        (words & 0xf) << 12
    }

    /// The peripheral whose data request paces this block.
    pub const fn permap(dreq: super::Dreq) -> u32 {
        (dreq as u32) << 16
    }

    /// Idle cycles inserted after each read or write, to throttle the channel.
    pub const fn waits(cycles: u32) -> u32 {
        (cycles & 0x1f) << 21
    }
}

/// Peripherals that can pace a transfer, as numbered in the `PERMAP` field.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum Dreq {
    /// No pacing: the request line is permanently asserted.
    None = 0,
    /// DSI0.
    Dsi0 = 1,
    /// PCM transmit.
    PcmTx = 2,
    /// PCM receive.
    PcmRx = 3,
    /// PWM FIFO.
    Pwm = 5,
    /// SPI0 transmit FIFO.
    Spi0Tx = 6,
    /// SPI0 receive FIFO.
    Spi0Rx = 7,
    /// The Arasan EMMC controller.
    Emmc = 11,
    /// DSI1.
    Dsi1 = 15,
}

const CS_ACTIVE: u32 = 1 << 0;
const CS_END: u32 = 1 << 1;
const CS_INT: u32 = 1 << 2;
const CS_ERROR: u32 = 1 << 8;
const CS_WAITING_FOR_WRITES: u32 = 1 << 6;
const CS_WAIT_FOR_OUTSTANDING_WRITES: u32 = 1 << 28;
const CS_ABORT: u32 = 1 << 30;
const CS_RESET: u32 = 1 << 31;

/// Mid-range priority for normal and panic traffic, as Linux uses.
const CS_PRIORITY: u32 = 8 << 16 | 8 << 20;

/// The `DEBUG` error flags: read-last-not-set, FIFO and read errors.
const DEBUG_ERRORS: u32 = 0b111;

/// Channels the ARM may use when the firmware has not been asked: what it
/// hands out on a Pi 3 with the default configuration.
const DEFAULT_CHANNELS: u16 = 0x7f35;

/// How long [`Channel::wait`] allows a transfer. Hundreds of megabytes at the
/// engine's speed.
const WAIT_LIMIT: Duration = Duration::from_secs(1);

/// The register stride between channels 0-14.
const CHANNEL_STRIDE: usize = 0x100;

/// Global interrupt status, one bit per channel, relative to [`map::DMA`].
const INT_STATUS: usize = 0xfe0;

/// Free channels the ARM may claim, one bit per channel.
static FREE: AtomicU16 = AtomicU16::new(DEFAULT_CHANNELS);

/// Tasks waiting on each channel, woken by [`handle_interrupt`].
static WAITERS: [TaskSet; 16] = [const { TaskSet::new() }; 16];

/// One control block per channel, backing [`Channel::copy`] and
/// [`Channel::fill`]. Out of line rather than in [`Channel`], whose alignment
/// would otherwise be 32 and too strict for a task's storage.
struct Scratch([UnsafeCell<ControlBlock>; 16]);

// SAFETY: entry `n` is only touched through the `Channel` that holds claim
// bit `n` in `FREE`, and there is at most one of those at a time.
unsafe impl Sync for Scratch {}

static SCRATCH: Scratch = Scratch([const { UnsafeCell::new(ControlBlock::new(0, 0, 0, 0)) }; 16]);

#[repr(C)]
struct Registers {
    cs: ReadWrite<u32>,        // 0x00
    conblk_ad: ReadWrite<u32>, // 0x04
    ti: ReadWrite<u32>,        // 0x08
    source_ad: ReadWrite<u32>, // 0x0c
    dest_ad: ReadWrite<u32>,   // 0x10
    txfr_len: ReadWrite<u32>,  // 0x14
    stride: ReadWrite<u32>,    // 0x18
    nextconbk: ReadWrite<u32>, // 0x1c
    debug: ReadWrite<u32>,     // 0x20
}

const _: () = assert!(core::mem::offset_of!(Registers, debug) == 0x20);

/// One step of a transfer, read by the engine from memory.
///
/// Addresses are bus addresses: [`bus::to_bus`] for DRAM, [`bus::peripheral`]
/// for a peripheral register.
#[derive(Clone, Copy, Debug, Default)]
#[repr(C, align(32))]
pub struct ControlBlock {
    /// Transfer information: the [`ti`] bits.
    pub info: u32,
    /// Bus address to read from.
    pub source: u32,
    /// Bus address to write to.
    pub dest: u32,
    /// Bytes to move, or `YLENGTH << 16 | XLENGTH` in 2D mode.
    pub length: u32,
    /// 2D mode only: destination stride in the high half, source in the low,
    /// both signed byte offsets added after each row.
    pub stride: u32,
    /// Bus address of the next block, or zero to stop after this one.
    pub next: u32,
    reserved: [u32; 2],
}

const _: () = assert!(core::mem::size_of::<ControlBlock>() == 32);

impl ControlBlock {
    /// A block with no successor.
    pub const fn new(info: u32, source: u32, dest: u32, length: u32) -> Self {
        Self {
            info,
            source,
            dest,
            length,
            stride: 0,
            next: 0,
            reserved: [0; 2],
        }
    }

    /// This block's bus address, for another block's `next` or a channel.
    pub fn bus_address(&self) -> u32 {
        bus::to_bus(self as *const Self as usize)
    }

    /// Writes this block back to memory so the engine sees it.
    ///
    /// [`Channel::start`] does this for the first block; every block chained
    /// after it needs it too.
    pub fn publish(&self) {
        // SAFETY: `self` is a live, mapped 32-byte object.
        unsafe { mmu::clean_invalidate_range(self as *const Self as usize, 32) };
    }
}

/// Which kind of channel to claim.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// A full channel: 2D mode, 1 GiB transfers, full bandwidth.
    Full,
    /// A lite channel, leaving the full ones free for work that needs them.
    Lite,
    /// Whichever is free, full first.
    Any,
}

/// Why a transfer failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// The channel is still running a previous transfer.
    Busy,
    /// The transfer did not finish within the limit; it has been aborted.
    Timeout,
    /// The engine flagged an error; the `DEBUG` register's error bits.
    Bus(u32),
    /// The source and destination lengths differ.
    LengthMismatch,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Busy => f.write_str("DMA channel busy"),
            Self::Timeout => f.write_str("DMA transfer timed out"),
            Self::Bus(bits) => write!(f, "DMA bus error (debug {bits:#x})"),
            Self::LengthMismatch => f.write_str("DMA buffers differ in length"),
        }
    }
}

const fn is_lite(index: usize) -> bool {
    matches!(index, 7..=14)
}

/// Asks the firmware which channels the ARM may use, and stops [`claim`]
/// handing out the rest.
///
/// Call before claiming anything. Without it the Pi 3 default mask stands.
pub fn probe_channels() -> Result<u16, super::mailbox::Error> {
    /// `GET_DMA_CHANNELS`, not one of the tags the mailbox driver names.
    const GET_DMA_CHANNELS: u32 = 0x0006_0001;
    let mut reply = [0u32; 1];
    super::MAILBOX
        .lock()
        .call_raw(GET_DMA_CHANNELS, &[0], &mut reply)?;
    let mask = reply[0] as u16;
    FREE.fetch_and(mask, Ordering::Relaxed);
    Ok(mask)
}

/// Claims a free channel of `kind`, or `None` if there is none.
///
/// The channel is reset and returned to the pool when dropped.
pub fn claim(kind: Kind) -> Option<Channel> {
    let wanted = |index: usize| match kind {
        Kind::Full => !is_lite(index),
        Kind::Lite => is_lite(index),
        Kind::Any => true,
    };
    let mut free = FREE.load(Ordering::Relaxed);
    loop {
        // Full channels first under `Any`: lite ones are the more plentiful.
        let index = (0..16)
            .filter(|&i| free & (1 << i) != 0 && wanted(i))
            .min_by_key(|&i| is_lite(i))?;
        // Acquire pairs with the Release in `Drop`, so the previous owner's
        // reset is complete before this one touches the channel.
        match FREE.compare_exchange_weak(
            free,
            free & !(1 << index),
            Ordering::Acquire,
            Ordering::Relaxed,
        ) {
            Ok(_) => {
                let base = if index == 15 {
                    map::DMA15
                } else {
                    map::DMA + index * CHANNEL_STRIDE
                };
                // SAFETY: the bit was ours to clear, so this is the only
                // handle to this channel's registers.
                let mut channel = Channel {
                    index: index as u8,
                    block: unsafe { RegisterBlock::new(base) },
                };
                channel.reset();
                return Some(channel);
            }
            Err(now) => free = now,
        }
    }
}

/// Acknowledges every pending DMA interrupt and wakes the tasks waiting on
/// those channels. Called from the interrupt dispatcher; takes no lock.
pub fn handle_interrupt() {
    // SAFETY: the global status register is mapped and read-only to software.
    let pending = unsafe { RegisterBlock::<()>::read_raw(map::DMA, INT_STATUS) };
    for index in (0..15).filter(|i| pending & (1 << i) != 0) {
        // SAFETY: `INT` is write-1-to-clear and touches nothing else, so this
        // cannot disturb the channel's owner.
        unsafe { RegisterBlock::<()>::write_raw(map::DMA, index * CHANNEL_STRIDE, CS_INT) };
        WAITERS[index].wake_all();
    }
}

/// The SoC interrupt number of `channel`. Channels 11-14 share one.
pub const fn irq(channel: usize) -> u32 {
    16 + if channel > 11 { 11 } else { channel as u32 }
}

/// An exclusively owned DMA channel.
pub struct Channel {
    index: u8,
    block: RegisterBlock<Registers>,
}

impl Channel {
    /// This channel's entry in [`SCRATCH`], which only this channel touches.
    fn scratch(&self) -> *mut ControlBlock {
        SCRATCH.0[self.index()].get()
    }

    /// The hardware channel number.
    pub fn index(&self) -> usize {
        self.index as usize
    }

    /// Whether this is a lite channel.
    pub fn is_lite(&self) -> bool {
        is_lite(self.index())
    }

    /// The SoC interrupt this channel raises.
    pub fn irq(&self) -> u32 {
        irq(self.index())
    }

    /// The longest single block this channel accepts, in bytes.
    pub fn max_length(&self) -> usize {
        if self.is_lite() { 0xfffc } else { 0x3fff_fffc }
    }

    /// Starts the chain beginning at `block`.
    ///
    /// # Safety
    ///
    /// Until [`is_done`](Self::is_done) reports completion or the channel is
    /// aborted, `block`, every block chained from it and every buffer they
    /// name must stay alive and untouched by the cores. Every chained block
    /// must have been [`publish`](ControlBlock::publish)ed, and DRAM buffers
    /// given the maintenance [`mmu::clean_invalidate_range`] describes.
    pub unsafe fn start(&mut self, block: &ControlBlock) -> Result<(), Error> {
        self.launch(block)
    }

    fn launch(&self, block: &ControlBlock) -> Result<(), Error> {
        let regs = self.block.regs();
        if regs.cs.read() & CS_ACTIVE != 0 {
            return Err(Error::Busy);
        }
        block.publish();
        regs.debug.write(DEBUG_ERRORS);
        regs.cs.write(CS_END | CS_INT);
        regs.conblk_ad.write(block.bus_address());
        regs.cs
            .write(CS_ACTIVE | CS_WAIT_FOR_OUTSTANDING_WRITES | CS_PRIORITY);
        Ok(())
    }

    /// Whether the last chain started has finished, or why it failed.
    pub fn is_done(&self) -> Option<Result<(), Error>> {
        let regs = self.block.regs();
        let cs = regs.cs.read();
        if cs & CS_ERROR != 0 {
            Some(Err(Error::Bus(regs.debug.read() & DEBUG_ERRORS)))
        } else if cs & CS_ACTIVE == 0 {
            Some(Ok(()))
        } else {
            None
        }
    }

    /// Waits for the running chain, aborting it after a second.
    pub fn wait(&mut self) -> Result<(), Error> {
        self.wait_inner()
    }

    fn wait_inner(&self) -> Result<(), Error> {
        let deadline = time::ticks() + time::frequency_hz() * WAIT_LIMIT.as_secs();
        loop {
            if let Some(result) = self.is_done() {
                return result;
            }
            if time::ticks() > deadline {
                self.halt();
                return Err(Error::Timeout);
            }
            cpu::spin_cycles(100);
        }
    }

    /// Awaits the running chain without blocking the core.
    ///
    /// Wakes on the channel's interrupt when the last block set
    /// [`ti::INTEN`] and the interrupt is enabled, and on the polling tick
    /// otherwise. Dropping the future does not stop the transfer.
    pub fn done(&mut self) -> Done<'_> {
        Done { channel: self }
    }

    /// Stops the channel wherever it is and returns it to idle.
    pub fn reset(&mut self) {
        self.halt();
    }

    fn halt(&self) {
        let regs = self.block.regs();
        // Pausing first and letting outstanding writes land keeps an abort
        // from leaving a write half-issued on the bus.
        regs.cs.write(0);
        for _ in 0..10_000 {
            if regs.cs.read() & CS_WAITING_FOR_WRITES == 0 {
                break;
            }
            cpu::spin_cycles(10);
        }
        regs.cs.write(CS_RESET);
        regs.cs.write(CS_END | CS_INT);
        regs.debug.write(DEBUG_ERRORS);
    }

    /// Copies `src` into `dst` by DMA, blocking until done.
    pub fn copy(&mut self, src: &[u32], dst: &mut [u32]) -> Result<(), Error> {
        if src.len() != dst.len() {
            return Err(Error::LengthMismatch);
        }
        self.run_words(
            ti::SRC_INC | ti::DEST_INC | ti::WAIT_RESP | ti::burst(4),
            Some(src.as_ptr() as usize),
            dst,
        )
    }

    /// Fills `dst` with `value` by DMA, blocking until done.
    pub fn fill(&mut self, dst: &mut [u32], value: u32) -> Result<(), Error> {
        // SAFETY: the entry is this channel's, and no transfer is running on it
        // while `&mut self` is held outside `run_words`.
        unsafe { (*self.scratch()).reserved[0] = value };
        self.run_words(ti::DEST_INC | ti::WAIT_RESP | ti::burst(4), None, dst)
    }

    /// Copies `block` into the channel's own storage and starts it, so the
    /// caller need not keep a block alive; only what it points at.
    ///
    /// The block's `next` must be zero, or name a chain that outlives the
    /// transfer. Its first reserved word is kept, since [`fill`](Self::fill)
    /// reads its value from there.
    ///
    /// # Safety
    ///
    /// As for [`start`](Self::start), for every buffer `block` names.
    pub unsafe fn submit(&mut self, block: ControlBlock) -> Result<(), Error> {
        if self.block.regs().cs.read() & CS_ACTIVE != 0 {
            return Err(Error::Busy);
        }
        let scratch = self.scratch();
        // SAFETY: the entry is this channel's, and the channel is idle, so the
        // engine is not reading it.
        let block = unsafe {
            *scratch = ControlBlock {
                reserved: (*scratch).reserved,
                ..block
            };
            &*scratch
        };
        self.launch(block)
    }

    /// Writes `value` to the register at bus address `dest`, through the
    /// engine rather than the core, blocking until it lands.
    ///
    /// For a peripheral whose bus slave ignores writes from the ARM but takes
    /// them from DMA; DSI1 is one.
    ///
    /// # Safety
    ///
    /// `dest` must be a register it is sound to write `value` to.
    pub unsafe fn write_word(&mut self, dest: u32, value: u32) -> Result<(), Error> {
        let scratch = self.scratch();
        // SAFETY: the entry is this channel's, and no transfer is running on
        // it while `&mut self` is held between transfers.
        let source = unsafe {
            (*scratch).reserved[0] = value;
            bus::to_bus(&raw const (*scratch).reserved[0] as usize)
        };
        // SAFETY: the source is the scratch block's own word, published with
        // it; the destination is the caller's.
        unsafe { self.submit(ControlBlock::new(ti::WAIT_RESP, source, dest, 4)) }?;
        self.wait_inner()
    }

    /// Runs a word transfer into `dst` in chunks of at most
    /// [`max_length`](Self::max_length), from `src` or, with `None`, from the
    /// scratch block's spare word.
    fn run_words(&mut self, info: u32, src: Option<usize>, dst: &mut [u32]) -> Result<(), Error> {
        let bytes = core::mem::size_of_val(dst);
        let dst = dst.as_mut_ptr() as usize;
        // SAFETY: both buffers are live borrows covering these ranges.
        unsafe {
            if let Some(src) = src {
                mmu::clean_invalidate_range(src, bytes);
            }
            mmu::clean_invalidate_range(dst, bytes);
        }

        let scratch = self.scratch();
        // SAFETY: only the address is taken.
        let fill_word = bus::to_bus(unsafe { &raw const (*scratch).reserved[0] } as usize);
        let chunk = self.max_length();
        let mut done = 0;
        let mut result = Ok(());
        while done < bytes && result.is_ok() {
            let length = chunk.min(bytes - done);
            let source = src.map_or(fill_word, |src| bus::to_bus(src + done));
            // SAFETY: the block names only the two buffers, borrowed for the
            // whole of this function, which waits for the transfer before
            // returning; the buffers were maintained above.
            result = unsafe {
                self.submit(ControlBlock::new(
                    info,
                    source,
                    bus::to_bus(dst + done),
                    length as u32,
                ))
            }
            .and_then(|()| self.wait_inner());
            done += length;
        }

        // SAFETY: as above. Drops anything prefetched into `dst` mid-transfer.
        unsafe { mmu::clean_invalidate_range(dst, bytes) };
        result
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        self.reset();
        // Release pairs with the Acquire in `claim`.
        FREE.fetch_or(1 << self.index, Ordering::Release);
    }
}

/// Future returned by [`Channel::done`].
#[must_use = "futures do nothing unless awaited"]
pub struct Done<'a> {
    channel: &'a mut Channel,
}

impl Future for Done<'_> {
    type Output = Result<(), Error>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Register before checking, so an interrupt landing in between still
        // finds this task to wake.
        WAITERS[self.channel.index()].insert_current();
        if let Some(result) = self.channel.is_done() {
            return Poll::Ready(result);
        }
        task::poll_again();
        Poll::Pending
    }
}
