// SPDX-License-Identifier: GPL-3.0-only
//! VideoCore property mailbox.
//!
//! The BCM2837's ARM cores cannot configure most of the SoC; clocks, power
//! domains, the DRAM split and the framebuffer are owned by the VideoCore
//! firmware and requested over a mailbox. Uses:
//!
//! * `GET_ARM_MEMORY` / `GET_VC_MEMORY` give the GPU's private DRAM
//!   reservation, which [`crate::arch::mmu::memory_map`] maps non-cacheable;
//! * `SET_CLOCK_RATE` on [`Clock::Arm`] clocks the CPU up on real hardware;
//! * `SET_POWER_STATE` on [`Device::V3d`] and the framebuffer tags are for a
//!   future GPU driver.
//!
//! # The message format
//!
//! A property message is a 16-byte-aligned buffer of `u32`:
//!
//! ```text
//!   [0] total size in bytes, including this header
//!   [1] request/response code: 0 on the way in, 0x8000_0000 on success
//!   [2] tag id            <- first tag
//!   [3] value buffer size in bytes
//!   [4] request code (0) on the way in; 0x8000_0000 | length on the way out
//!   ... value words, padded to a 4-byte boundary
//!   ... further tags
//!   [n] 0  <- end tag
//! ```
//!
//! The VideoCore writes its reply back into the *same* buffer.
//!
//! # Cache coherency
//!
//! The GPU is not in the ARM's coherency domain and addresses DRAM through a
//! bus alias rather than the ARM physical address ([`memory::bus`]). Two
//! consequences:
//!
//! 1. The address written to the mailbox must be the *uncached* bus alias,
//!    `memory::bus::to_bus(physical)`.
//! 2. The ARM's view of the buffer is Normal cacheable, so it must be cleaned
//!    before the doorbell is rung and invalidated afterwards, or the VC reads
//!    stale data and the ARM reads a stale reply.
//!
//! Exception: before [`crate::arch::mmu::enable`] runs, every access is
//! Device-nGnRnE and therefore uncached, so a mailbox transaction during early
//! boot needs no maintenance. That is when [`probe_memory`] runs.

#![allow(dead_code)]

use core::sync::atomic::{Ordering, compiler_fence};

use crate::{
    arch::{cpu, mmu},
    bsp::memory::{
        self,
        map::mailbox::{self as regs, channel, status},
    },
};

/// Maximum property message length, in 32-bit words.
///
/// Large enough for the multi-tag framebuffer setup a GPU driver will need.
const BUFFER_WORDS: usize = 64;

/// Response code the VideoCore writes into word 1 on success.
const RESPONSE_SUCCESS: u32 = 0x8000_0000;

/// Bit the VideoCore sets in a tag's request/response word once it has filled
/// in that tag's value.
const TAG_RESPONSE: u32 = 0x8000_0000;

/// How many polls to make before giving up on a mailbox transaction.
const POLL_LIMIT: u32 = 4_000_000;

/// Property interface tag identifiers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum Tag {
    /// Firmware revision.
    GetFirmwareRevision = 0x0000_0001,
    /// Board model.
    GetBoardModel = 0x0001_0001,
    /// Board revision code, which encodes the model and RAM size.
    GetBoardRevision = 0x0001_0002,
    /// Board MAC address.
    GetBoardMac = 0x0001_0003,
    /// Board serial number.
    GetBoardSerial = 0x0001_0004,
    /// Base and size of the DRAM the ARM cores own.
    GetArmMemory = 0x0001_0005,
    /// Base and size of the DRAM the VideoCore owns.
    GetVcMemory = 0x0001_0006,
    /// Query a power domain.
    GetPowerState = 0x0002_0001,
    /// Turn a power domain on or off. Needed before touching the V3D block.
    SetPowerState = 0x0002_8001,
    /// Configured rate of a clock, in Hz.
    GetClockRate = 0x0003_0002,
    /// Highest rate a clock may be set to.
    GetMaxClockRate = 0x0003_0004,
    /// Lowest rate a clock may be set to.
    GetMinClockRate = 0x0003_0007,
    /// Set a clock rate. This is the CPU overclock path.
    SetClockRate = 0x0003_8002,
    /// Actual measured rate of a clock, in Hz.
    GetMeasuredClockRate = 0x0003_0047,
    /// SoC temperature in thousandths of a degree Celsius.
    GetTemperature = 0x0003_0006,
    /// Temperature at which the SoC throttles.
    GetMaxTemperature = 0x0003_000A,
    /// Allocate the framebuffer. Returns its bus address and size.
    AllocateFramebuffer = 0x0004_0001,
    /// Release the framebuffer.
    ReleaseFramebuffer = 0x0004_8001,
    /// Physical (display) width and height.
    SetPhysicalSize = 0x0004_8003,
    /// Virtual (framebuffer) width and height.
    SetVirtualSize = 0x0004_8004,
    /// Bits per pixel.
    SetDepth = 0x0004_8005,
    /// Pixel order (BGR or RGB).
    SetPixelOrder = 0x0004_8006,
    /// Which pixel of the virtual buffer appears at the top left of the
    /// display. Moving this is how a page flip is done without a copy.
    SetVirtualOffset = 0x0004_8009,
    /// Bytes per line of the allocated framebuffer.
    GetPitch = 0x0004_0008,
    /// Blocks until the display's next vertical blanking interval.
    ///
    /// Unlike every other tag here this one is a *wait*, not a setting: the
    /// firmware does not answer it until the beam reaches the end of a frame.
    /// It is the only vertical-sync signal the property interface offers, and
    /// without it a page flip lands wherever the beam happens to be and the
    /// panel shows two frames at once. See [`super::framebuffer::Framebuffer::wait_for_vsync`].
    ///
    /// Not every firmware and no emulator implements it, so a caller must be
    /// ready for it to go unanswered rather than treating that as fatal.
    SetVsync = 0x0004_800E,
    /// Blank or unblank the display.
    BlankScreen = 0x0004_0002,
    /// Power one of the firmware's fine-grained domains, see [`Domain`].
    SetDomainState = 0x0003_8030,
}

/// The firmware's fine-grained power domains, for [`Tag::SetDomainState`].
///
/// Finer than [`Device`]: these are the blocks behind the camera and display
/// interfaces, which read back nothing until powered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum Domain {
    /// Unicam receiver 0 (`CAM0`).
    Unicam0 = 12,
    /// Unicam receiver 1 (`CAM1`).
    Unicam1 = 13,
    /// The DSI0 host.
    Dsi0 = 17,
    /// The DSI1 host.
    Dsi1 = 18,
}

/// Clock identifiers accepted by the clock tags.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum Clock {
    /// SD card interface.
    Emmc = 1,
    /// UART reference clock.
    Uart = 2,
    /// The ARM cores; raise this to clock the CPU up.
    Arm = 3,
    /// VideoCore core clock, which also drives most peripherals.
    Core = 4,
    /// The 3D block.
    V3d = 5,
    /// H.264 codec.
    H264 = 6,
    /// Image sensor pipeline.
    Isp = 7,
    /// SDRAM.
    Sdram = 8,
    /// Pixel clock.
    Pixel = 9,
    /// PWM.
    Pwm = 10,
}

/// Power domain identifiers accepted by the power tags.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum Device {
    /// SD card.
    SdCard = 0,
    /// UART0, the PL011.
    Uart0 = 1,
    /// UART1, the mini UART.
    Uart1 = 2,
    /// USB host controller.
    UsbHcd = 3,
    /// I2C0.
    I2c0 = 4,
    /// I2C1.
    I2c1 = 5,
    /// I2C2.
    I2c2 = 6,
    /// SPI.
    Spi = 7,
    /// CCP2TX.
    Ccp2tx = 8,
    /// The 3D block. Must be powered on before its registers respond.
    V3d = 10,
}

/// Why a mailbox transaction failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// The mailbox stayed full or empty past `POLL_LIMIT`.
    Timeout,
    /// The VideoCore replied on a different channel than the one addressed.
    WrongChannel,
    /// The VideoCore did not set the success code in the message header.
    Rejected,
    /// The VideoCore did not fill in the requested tag.
    TagNotAnswered,
    /// The message would not fit in `BUFFER_WORDS`.
    MessageTooLong,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let text = match self {
            Self::Timeout => "mailbox timed out",
            Self::WrongChannel => "reply on the wrong channel",
            Self::Rejected => "firmware rejected the message",
            Self::TagNotAnswered => "firmware did not answer the tag",
            Self::MessageTooLong => "message too long",
        };
        f.write_str(text)
    }
}

/// The shared property message buffer.
///
/// Static rather than stack: the VideoCore needs a physical DRAM address, and
/// the alignment is easier to guarantee for a static.
///
/// # Why a whole cache line, not the sixteen bytes the mailbox asks for
///
/// The channel is carried in the low four bits of the address, so the
/// *mailbox* needs only 16-byte alignment. The cache maintenance needs more.
/// `transact` cleans and invalidates by cache line, and a line is 64 bytes,
/// so it rounds the buffer's address down to the start of the line it begins
/// in. At 16-byte alignment that line can hold up to 48 bytes of whatever
/// static the linker put before this one.
///
/// With one core that is harmless — nothing else is writing. With a second
/// core running it is not: that core can hold the shared line dirty in its
/// own cache, and its eventual write-back lands on top of a message the
/// VideoCore has not read yet. The symptom is not a hang but a rejected
/// message, because the firmware reads a header that no longer says what was
/// written. `BUFFER_WORDS` is a multiple of a line, so aligning to one gives
/// the buffer its lines to itself and no neighbour can share them.
#[repr(C, align(64))]
struct MessageBuffer([u32; BUFFER_WORDS]);

// The reasoning above only holds while the buffer is a whole number of cache
// lines; a partial last line could still be shared.
const _: () = assert!(core::mem::size_of::<MessageBuffer>().is_multiple_of(CACHE_LINE));

static mut MESSAGE: MessageBuffer = MessageBuffer([0; BUFFER_WORDS]);

/// One tag's slot in a property message.
///
/// Each slot carries a mutable borrow for the reply, since a multi-tag message
/// scatters replies back into the caller's buffers.
pub struct TagIo<'a> {
    /// Identifier of the property tag to send.
    ///
    /// A `u32` rather than a [`Tag`] so that a downstream kernel can reach a
    /// tag this BSP has not enumerated; use [`TagIo::new`] for the ones it has,
    /// and [`TagIo::raw`] for the rest.
    pub id: u32,
    /// Value words handed to the firmware.
    pub request: &'a [u32],
    /// Where the firmware's reply words are written.
    pub response: &'a mut [u32],
    /// How many words the firmware reported writing. Filled in by
    /// [`Mailbox::call_many`]; meaningless before that returns.
    pub answered: usize,
}

impl<'a> TagIo<'a> {
    /// A slot that sends `request` and accepts up to `response.len()` words back.
    pub fn new(tag: Tag, request: &'a [u32], response: &'a mut [u32]) -> Self {
        Self::raw(tag as u32, request, response)
    }

    /// The same, for a tag identifier this BSP does not name.
    ///
    /// The property interface is large and only partly documented, and [`Tag`]
    /// carries the parts the BSP itself uses. Without this a downstream kernel
    /// that wants, say, `GET_THROTTLED` has to fork the crate to add one
    /// discriminant — which is a poor reason to fork a BSP.
    pub fn raw(id: u32, request: &'a [u32], response: &'a mut [u32]) -> Self {
        Self {
            id,
            request,
            response,
            answered: 0,
        }
    }

    /// Size of this tag's value buffer, in words.
    ///
    /// The property interface uses one buffer for both directions, sized to the
    /// larger of request and response.
    fn value_words(&self) -> usize {
        self.request.len().max(self.response.len())
    }
}

/// The VideoCore property mailbox.
///
/// Held in a [`crate::sync::SpinLock`] in [`crate::drivers::MAILBOX`]: the
/// single shared message buffer forbids concurrent transactions.
pub struct Mailbox {
    _private: (),
}

impl Mailbox {
    /// Creates the mailbox interface.
    ///
    /// # Safety
    ///
    /// At most one may exist, and it must be the only user of the mailbox
    /// registers and of the static message buffer.
    pub const unsafe fn new() -> Self {
        Self { _private: () }
    }

    /// Performs a single-tag transaction and returns the response words.
    ///
    /// `request` supplies the tag's value buffer on the way in. The value
    /// buffer is sized to whichever is larger of `request` and `response`, as
    /// the property interface requires.
    pub fn call(
        &mut self,
        tag: Tag,
        request: &[u32],
        response: &mut [u32],
    ) -> Result<usize, Error> {
        self.call_raw(tag as u32, request, response)
    }

    /// The same, for a tag identifier this BSP does not name.
    ///
    /// See [`TagIo::raw`] for why this exists.
    pub fn call_raw(
        &mut self,
        id: u32,
        request: &[u32],
        response: &mut [u32],
    ) -> Result<usize, Error> {
        let mut tags = [TagIo::raw(id, request, response)];
        self.call_many(&mut tags)?;
        Ok(tags[0].answered)
    }

    /// Performs a transaction carrying several tags in one message.
    ///
    /// The VideoCore applies tags in order, so settings that must be applied
    /// together belong here — e.g. configuring the framebuffer mode and then
    /// allocating it, where the allocation must observe the preceding tags.
    pub fn call_many(&mut self, tags: &mut [TagIo<'_>]) -> Result<(), Error> {
        // header (2) + per tag (id, value size, request code, values) + end tag
        let mut total_words = 3;
        for slot in tags.iter() {
            total_words += 3 + slot.value_words();
        }
        if total_words > BUFFER_WORDS {
            return Err(Error::MessageTooLong);
        }

        // SAFETY: `Mailbox` is a singleton behind a lock, so this is the only
        // live reference to the buffer for the duration of the call.
        let message = unsafe { &mut *core::ptr::addr_of_mut!(MESSAGE) };
        let words = &mut message.0;

        words[0] = (total_words * 4) as u32;
        words[1] = 0;

        let mut at = 2;
        for slot in tags.iter() {
            let values = slot.value_words();
            words[at] = slot.id;
            words[at + 1] = (values * 4) as u32;
            words[at + 2] = 0;
            words[at + 3..at + 3 + values].fill(0);
            words[at + 3..at + 3 + slot.request.len()].copy_from_slice(slot.request);
            at += 3 + values;
        }
        words[at] = 0;

        let address = core::ptr::addr_of!(*words) as *const u32 as usize;
        self.transact(address, total_words)?;

        if words[1] != RESPONSE_SUCCESS {
            return Err(Error::Rejected);
        }

        let mut at = 2;
        for slot in tags.iter_mut() {
            let values = slot.value_words();
            if words[at + 2] & TAG_RESPONSE == 0 {
                return Err(Error::TagNotAnswered);
            }
            // The firmware reports the length it actually wrote, which can be
            // shorter than the buffer it was handed.
            let answered = ((words[at + 2] & !TAG_RESPONSE) as usize / 4).min(values);
            let copied = answered.min(slot.response.len());
            slot.response[..copied].copy_from_slice(&words[at + 3..at + 3 + copied]);
            slot.answered = answered;
            at += 3 + values;
        }
        Ok(())
    }

    /// Rings the doorbell and waits for the reply, handling cache maintenance.
    fn transact(&mut self, address: usize, total_words: usize) -> Result<(), Error> {
        let bus_address = memory::bus::to_bus(address);
        // The low four bits carry the channel, so the buffer must be 16-byte
        // aligned. `MessageBuffer` guarantees it.
        debug_assert!(bus_address & 0xF == 0);
        let payload = bus_address | channel::PROPERTY_ARM_TO_VC;

        // Publish the request. With the MMU off everything is already uncached,
        // so this is a no-op beyond the barrier.
        self.flush(address, total_words);

        Self::wait_until(regs::MB1, |value| value & status::FULL == 0)?;
        // SAFETY: MB1 DATA is a valid Device-mapped write-only register.
        unsafe { write_reg(regs::MB1 + regs::DATA, payload) };

        loop {
            Self::wait_until(regs::MB0, |value| value & status::EMPTY == 0)?;
            // SAFETY: MB0 DATA is a valid Device-mapped read-only register.
            let reply = unsafe { read_reg(regs::MB0 + regs::DATA) };

            if reply & 0xF != channel::PROPERTY_ARM_TO_VC {
                // Traffic for another channel; drop it and keep polling.
                continue;
            }
            if reply & !0xF != bus_address {
                return Err(Error::WrongChannel);
            }
            break;
        }

        // Drop any cached copy of the buffer so the reply is actually read.
        self.flush(address, total_words);
        Ok(())
    }

    /// Cleans and invalidates the message buffer, then orders it against the
    /// mailbox accesses.
    fn flush(&self, address: usize, total_words: usize) {
        if mmu::is_enabled() {
            let end = address + total_words * 4;
            let mut line = address & !(CACHE_LINE - 1);
            while line < end {
                // SAFETY: the buffer is a live static and the range is its own.
                unsafe { mmu::clean_invalidate_line(line) };
                line += CACHE_LINE;
            }
        }
        compiler_fence(Ordering::SeqCst);
        cpu::dsb_sy();
    }

    /// Polls a mailbox status register until `ready` is satisfied.
    fn wait_until(mailbox: usize, ready: impl Fn(u32) -> bool) -> Result<(), Error> {
        for _ in 0..POLL_LIMIT {
            // SAFETY: a valid Device-mapped status register.
            if ready(unsafe { read_reg(mailbox + regs::STATUS) }) {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err(Error::Timeout)
    }

    /// Base and size in bytes of the DRAM the ARM cores own.
    pub fn arm_memory(&mut self) -> Result<(usize, usize), Error> {
        let mut out = [0u32; 2];
        self.call(Tag::GetArmMemory, &[], &mut out)?;
        Ok((out[0] as usize, out[1] as usize))
    }

    /// Base and size in bytes of the DRAM the VideoCore owns.
    pub fn vc_memory(&mut self) -> Result<(usize, usize), Error> {
        let mut out = [0u32; 2];
        self.call(Tag::GetVcMemory, &[], &mut out)?;
        Ok((out[0] as usize, out[1] as usize))
    }

    /// Board revision code.
    pub fn board_revision(&mut self) -> Result<u32, Error> {
        let mut out = [0u32; 1];
        self.call(Tag::GetBoardRevision, &[], &mut out)?;
        Ok(out[0])
    }

    /// Board serial number.
    pub fn board_serial(&mut self) -> Result<u64, Error> {
        let mut out = [0u32; 2];
        self.call(Tag::GetBoardSerial, &[], &mut out)?;
        Ok(u64::from(out[1]) << 32 | u64::from(out[0]))
    }

    /// Configured rate of `clock`, in Hz. Zero means the clock does not exist.
    pub fn clock_rate(&mut self, clock: Clock) -> Result<u32, Error> {
        let mut out = [0u32; 2];
        self.call(Tag::GetClockRate, &[clock as u32], &mut out)?;
        Ok(out[1])
    }

    /// Highest rate `clock` may be set to, in Hz.
    pub fn max_clock_rate(&mut self, clock: Clock) -> Result<u32, Error> {
        let mut out = [0u32; 2];
        self.call(Tag::GetMaxClockRate, &[clock as u32], &mut out)?;
        Ok(out[1])
    }

    /// Lowest rate `clock` may be set to, in Hz.
    pub fn min_clock_rate(&mut self, clock: Clock) -> Result<u32, Error> {
        let mut out = [0u32; 2];
        self.call(Tag::GetMinClockRate, &[clock as u32], &mut out)?;
        Ok(out[1])
    }

    /// Requests `rate_hz` for `clock` and returns the rate actually granted.
    ///
    /// The firmware clamps the request to what `config.txt` and thermal and
    /// voltage headroom allow, so the returned value is authoritative and may
    /// be lower than requested. To clock the ARM cores up, request
    /// `max_clock_rate(Clock::Arm)`.
    ///
    /// `skip_turbo` of `false` applies the firmware's usual turbo behaviour.
    pub fn set_clock_rate(
        &mut self,
        clock: Clock,
        rate_hz: u32,
        skip_turbo: bool,
    ) -> Result<u32, Error> {
        let mut out = [0u32; 3];
        self.call(
            Tag::SetClockRate,
            &[clock as u32, rate_hz, u32::from(skip_turbo)],
            &mut out,
        )?;
        Ok(out[1])
    }

    /// SoC temperature in thousandths of a degree Celsius.
    pub fn temperature(&mut self) -> Result<u32, Error> {
        let mut out = [0u32; 2];
        self.call(Tag::GetTemperature, &[0], &mut out)?;
        Ok(out[1])
    }

    /// Powers a device domain on or off, waiting for the transition.
    ///
    /// A 3D driver must call this with [`Device::V3d`] before the V3D registers
    /// at [`memory::map::V3D`] read back anything but zero.
    pub fn set_power_state(&mut self, device: Device, on: bool) -> Result<u32, Error> {
        /// Bit 0 of the state word: requested power state.
        const ON: u32 = 1 << 0;
        /// Bit 1: block until the transition has completed.
        const WAIT: u32 = 1 << 1;

        let state = if on { ON | WAIT } else { WAIT };
        let mut out = [0u32; 2];
        self.call(Tag::SetPowerState, &[device as u32, state], &mut out)?;
        Ok(out[1])
    }
}

impl Mailbox {
    /// Powers a [`Domain`] on or off.
    ///
    /// The firmware echoes the domain and its new state; anything less means
    /// the tag was not understood, which QEMU's emulated firmware does.
    pub fn set_domain_state(&mut self, domain: Domain, on: bool) -> Result<(), Error> {
        let mut out = [0u32; 2];
        let words = self.call(
            Tag::SetDomainState,
            &[domain as u32, u32::from(on)],
            &mut out,
        )?;
        if words < 2 {
            return Err(Error::TagNotAnswered);
        }
        if out[1] != u32::from(on) {
            return Err(Error::Rejected);
        }
        Ok(())
    }
}

/// Cortex-A53 cache line size in bytes.
const CACHE_LINE: usize = 64;

/// # Safety
///
/// `address` must be a valid Device-mapped 32-bit register.
unsafe fn read_reg(address: usize) -> u32 {
    unsafe { core::ptr::read_volatile(address as *const u32) }
}

/// # Safety
///
/// `address` must be a valid Device-mapped 32-bit register.
unsafe fn write_reg(address: usize, value: u32) {
    unsafe { core::ptr::write_volatile(address as *mut u32, value) }
}

/// Asks the firmware what the ARM cores are clocked at, and what they could be.
///
/// Returns `(current, maximum)` in hertz. It only *reads*: the BSP does not
/// raise the clock, because a boot-time rate change is policy, and policy the
/// application cannot skip does not belong in the boot path. The firmware boots
/// the cores at their minimum — 600 MHz of a possible 1200 on a Pi 3 — and
/// leaves them there, so an application that never asks runs at half speed
/// forever. Reporting both numbers is what makes that visible; raising it is
/// [`Mailbox::set_clock_rate`], and it is the application's call.
///
/// # Safety
///
/// Core 0 only, with the MMU still off and no secondary core running: it uses
/// the mailbox's bypass path, and single-threaded access is what makes the
/// unlocked access sound.
pub unsafe fn probe_clock() -> Result<(u32, u32), Error> {
    // SAFETY: the caller guarantees single-threaded, exclusive access, which is
    // the same window `probe_memory` runs in.
    let mailbox = unsafe { crate::drivers::MAILBOX.bypass() };
    let current = mailbox.clock_rate(Clock::Arm)?;
    let maximum = mailbox.max_clock_rate(Clock::Arm)?;
    Ok((current, maximum))
}

/// Asks the firmware for the real DRAM split and records it.
///
/// Called by core 0 before [`crate::arch::mmu::init`], so that the memory map
/// reflects the actual `gpu_mem` setting rather than the compile-time guess.
/// Failure is not fatal: the default split stands and the map is still usable.
///
/// # Safety
///
/// Core 0 only, with the MMU still off and no secondary core running. The MMU
/// being off makes the transaction coherent without cache maintenance;
/// single-threaded access makes the unlocked mailbox access sound.
pub unsafe fn probe_memory() -> Result<(usize, usize), Error> {
    // SAFETY: the caller guarantees single-threaded, exclusive access.
    let mailbox = unsafe { crate::drivers::MAILBOX.bypass() };
    let (base, size) = mailbox.vc_memory()?;

    // SAFETY: same contract; this is the window in which the memory map may
    // still be adjusted.
    unsafe { memory::set_vc_memory(base, size) };
    Ok((base, size))
}
