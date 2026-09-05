# SD card (EMMC)

The BCM2837 has an Arasan SDHCI controller wired to the SD slot. This driver
brings a card out of reset, identifies it, negotiates a 4-bit bus, and does
programmed I/O block reads and writes. It is enough to load files off a card;
it is not a filesystem, and it is deliberately not a DMA engine.

The register block lives at `MMIO_BASE + 0x0030_0000` — `0x3F30_0000` on real
hardware and under QEMU's `raspi3b` — and is mapped Device-nGnRnE by
[03-memory-and-mmu.md](03-memory-and-mmu.md), so every access follows the MMIO
rules in [05-drivers-and-mmio.md](05-drivers-and-mmio.md).

## Why PIO and not DMA

The card is not on the BSP boot path. The shell example initialises it on
demand, and the `i`/`r`/`x` console commands run long after the shell is up,
so the cost of a
transfer is not the cost of a boot.

DMA would mean a descriptor ring in uncached memory, a second MMIO block, an
interrupt path, and a cache-maintenance story for every buffer handed in.
Programmed I/O is a loop over a 32-bit data register. At roughly 5 ms per
512-byte block under QEMU that is slow, but it is slow in a way that is
completely predictable and has no shared state to get wrong.

If a later stage needs throughput, the place to add it is `transfer_blocks`,
which is already the only function that touches the data register.

## Command layer

Everything the driver sends goes through one path:

```text
send_command(SdCommand)                       no argument
send_command_arg(SdCommand, u32)              with an argument
        |
        v
  wait for CMD_INHIBIT / DAT_INHIBIT to clear
  clear the interrupt latch
  write ARG1, then CMDTM (the write to CMDTM starts the command)
  wait for CMD_DONE, checking ERROR_MASK
  latch RESP0..RESP3
```

`SdCommand` is an enum rather than raw indices so the response type travels
with the command. Getting `RSP_136` wrong on `SendCsd` does not fail loudly —
it silently returns the wrong 32 bits — so the encoding is written once, next
to the command, and never at the call site.

Application commands (`ACMD*`) are `CMD55` followed by the command proper. The
driver sends the pair itself and checks `ST_APP_CMD` in the `CMD55` response,
because a card that ignored `CMD55` will happily misinterpret the `ACMD` that
follows as an ordinary command with the same index.

Every wait is bounded, per rule 10.2 in [`instructions.md`](../instructions.md).
A card that never answers produces `Error::Timeout`, not a hung core.

## Bring-up sequence

```text
 1. CONTROL0 = 0, CONTROL1 |= SRST_HC       full controller reset
 2. wait for SRST_HC to self-clear
 3. CONTROL1 = CLK_INTLEN | divider(400 kHz) | DATA_TOUNIT_MAX
 4. wait for CLK_STABLE, then set CLK_EN    identification clock
 5. enable all interrupt status bits (polled, not routed to the GIC)
 6. CMD0   GoIdleState                      card to idle
 7. CMD8   SendIfCond, 0x1AA                voltage + check pattern echo
 8. ACMD41 SdSendOpCond, loop               power-up, HCS if CMD8 answered
 9. CMD2   AllSendCid                       CID -> manufacturer, product, serial
10. CMD3   SendRelativeAddr                 card assigns its RCA
11. CMD9   SendCsd, RCA                     CSD -> capacity, CSD version
12. CMD7   SelectCard, RCA                  idle -> transfer state
13. raise the clock to 20 MHz
14. ACMD6  SetBusWidth, RCA|2 + HCTL_DWIDTH 4-bit data bus
15. CMD16  SetBlocklen, 512
```

Step 7 is what decides everything after it. A card that answers `CMD8` with the
check pattern intact is SD v2 or later and may be high capacity, so `ACMD41`
gets `HCS` set and the card may come back with `OCR_CARD_CAPACITY`. A card that
times out on `CMD8` is SD v1, `ACMD41` is sent without `HCS`, and the capacity
must come from the CSD.

Step 8 is the only unbounded-looking loop in the sequence. It is bounded: the
card has a spec-mandated one-second power-up window, and the driver gives up
after that.

Step 14 changes the bus width on both sides. The order matters — `ACMD6` tells
the card first, `HCTL_DWIDTH` tells the controller second. Doing it the other
way round leaves the controller listening on four lines to a card driving one.

## Capacity, and the CSD bit numbering trap

`CMD9` returns a 136-bit R2 response. The controller strips the CRC7 and the
stop bit, so `RESP0..RESP3` hold CSD bits `[127:8]` — but not in the order a
first reading of the datasheet suggests. The driver assembles the four
registers little-endian into a 16-byte buffer:

```text
buffer[0..4]  = RESP3.to_le_bytes()
buffer[4..8]  = RESP2.to_le_bytes()
buffer[8..12] = RESP1.to_le_bytes()
buffer[12..16]= RESP0.to_le_bytes()
```

which produces this mapping, written out in full in `unpack_csd` because
deriving it a second time is how bugs get reintroduced:

```text
buffer[0]  = CSD[111:104]     buffer[8]  = CSD[47:40]
buffer[1]  = CSD[119:112]     buffer[9]  = CSD[55:48]
buffer[2]  = CSD[127:120]     buffer[10] = CSD[63:56]
buffer[3]  = unused           buffer[11] = CSD[71:64]
buffer[4]  = CSD[79:72]       buffer[12] = CSD[15:8]
buffer[5]  = CSD[87:80]       buffer[13] = CSD[23:16]
buffer[6]  = CSD[95:88]       buffer[14] = CSD[31:24]
buffer[7]  = CSD[103:96]      buffer[15] = CSD[39:32]
```

`CSD_STRUCTURE` is `CSD[127:126]`, so it reads out of `buffer[2] & 0xC0`. That
one field is the check: if it yields a sensible 0 or 1, the mapping is right.

**CSD version 2** (high capacity) is easy. `C_SIZE` is `CSD[69:48]`, a plain
22-bit field, and capacity is `(C_SIZE + 1) * 512 KiB`.

**CSD version 1** is where this driver had a real bug. The fields are split and
the byte indices are not adjacent:

```text
C_SIZE      = CSD[73:62]  = (buffer[4] & 0x03) << 10
                          | (buffer[11] as u32) << 2
                          | (buffer[10] & 0xC0) >> 6
C_SIZE_MULT = CSD[49:47]  = (buffer[9] & 0x03) << 1
                          | (buffer[8] & 0x80) >> 7
READ_BL_LEN = CSD[83:80]  =  buffer[5] & 0x0F

capacity = (C_SIZE + 1) * 2^(C_SIZE_MULT + 2) * 2^READ_BL_LEN
```

An earlier version read those fields from bytes 6, 7, 8 and 10, which is what
you get if you assume the buffer is in CSD bit order. It reported QEMU's 64 MiB
card as 819 MiB. Since `block_count()` gates `read_blocks`/`write_blocks`
against `Error::OutOfRange`, a wrong capacity is not cosmetic: it either
rejects valid blocks or waves through reads that run off the end of the card.

The lesson is in the code as a comment table rather than in a commit message.

## Transfers

`read_blocks` and `write_blocks` both validate before touching hardware:

* the buffer must be non-empty and a whole multiple of `BLOCK_SIZE` (512),
  otherwise `Error::InvalidBuffer`;
* `start_block + count` must be within `block_count()`, otherwise
  `Error::OutOfRange`.

The API always takes **512-byte sector numbers**. Before issuing the command,
both standard-capacity families (`V1` and `V2Sc`) multiply that sector number
by 512; `V2Hc` passes it unchanged. The multiplication is checked and returns
`Error::OutOfRange` if the byte address does not fit the command's 32-bit
argument. A checked shift would only validate the shift count, not lost bits.
Treating V1 as block-addressed would silently send sector 512 to byte 512
(sector 1), corrupting the wrong block.

Compile-time regressions beside `CardType::block_address` cover all three
families at sectors 0, 1, 512, the last representable byte address, its successor
and `u32::MAX`. They run on every library build, including
`cargo check --release --lib`; no public testing API or writable card is needed.

Both then call `transfer_blocks`, which takes a direction-carrying enum:

```rust
enum Transfer<'a> {
    Read(&'a mut [u8]),
    Write(&'a [u8]),
}
```

This exists for a soundness reason. The obvious implementation shares one
`&mut [u8]` and casts on the way in for writes. That is undefined behaviour:
a caller may legitimately pass a `&'static [u8]` living in `.rodata`, and
fabricating a `&mut` to it is UB the moment it is created, whether or not the
driver ever writes through it. The enum costs nothing at runtime and needs no
`unsafe`.

Single-block transfers use `CMD17`/`CMD24`; multi-block use `CMD18`/`CMD25`
with `TM_MULTI_BLOCK | TM_BLKCNT_EN`, and the driver sends `CMD12` afterwards to
return the card to transfer state.

The inner loop waits for `READ_RDY` or `WRITE_RDY` per block, then moves 128
words through the `DATA` register. `BLKSIZECNT` is programmed with the block
size in the low half and the count in the high half.

## Console

Three commands, all after `i`:

| key | what it does |
| --- | --- |
| `i` | probes the controller if needed, then prints the card table |
| `r` | hexdumps block 0, collapsing runs of zeros, and checks for `0x55AA` |
| `x` | writes a pattern to block 1, reads it back, and verifies byte-for-byte |

`x` uses block 1 rather than block 0 on purpose: block 0 is the MBR on a
formatted card, and a self-test that destroys the partition table is not a
self-test. Block 1 is inside the reserved gap before the first partition on
every layout this kernel is likely to meet.

## What QEMU tells you, and what it does not

Against `-drive file=sd.img,if=sd,format=raw`, the emulated card reports:

```text
type          sd v2 sc
capacity      64 MiB (matches the image)
product       QEMU!  rev 0.1
manufacturer  0xaa (OEM XY)
serial        0xdeadbeef
manufactured  2006-02
bus width     4-bit
OCR           0x80ffff00, CSD v1
```

Init takes about 10 ms; a 512-byte read about 5 ms; a write about 4 ms. The RCA
is randomised on every boot, so never hard-code it.

What QEMU does **not** exercise:

* real card timing — `ACMD41` completes almost immediately, so a slow card's
  power-up window is untested;
* the `SRST_HC` / `CONTROL1` reset dance against a controller that is already
  in a bad state;
* voltage switching and any UHS speed mode, none of which this driver attempts;
* CSD v2, since QEMU's card is v1.

## On real hardware

A 16 GB SDHC card in a Raspberry Pi Zero 2 W reports:

```text
type          sd v2 hc
capacity      15238 MiB (31207424 blocks)
CSD           v2
partition     #1 FAT32 at LBA 8192, 16 sectors/cluster, 1948046 clusters
```

Two numbers differ sharply from emulation.

**Enumeration takes 403 ms, not 10 ms.** Nearly all of it is the `ACMD41`
power-up poll, which the emulated card completes immediately. Any bounded retry
count tuned against QEMU will be far too short for a real card.

**CSD v2 now runs.** The capacity decoded out of the CSD agrees exactly with
`block_count * 512`, which is the check to make: confusing the v1 and v2
layouts gives a wildly wrong size rather than a subtly wrong one, so agreement
to the byte is strong evidence.

Reading a 6691-byte file from the FAT32 boot partition took 4.3 ms. That was
done by DigiDash, a downstream consumer, which walked the MBR, the BPB, the root
directory and the cluster chain using nothing but `read_blocks`. The addressing
is therefore confirmed against structures the VideoCore firmware itself wrote
and reads.

## Filesystems

This driver stops at the block. It knows nothing of partitions or files, and
that is deliberate: the split between "move 512 bytes to and from this LBA" and
"find CONFIG.TXT" is a real one, and only the first half is board-specific.

The second half is available behind the **`embedded-sdmmc`** feature, off by
default. It supplies [`hal::sdmmc`](../cogware-bcm2837/src/hal/sdmmc.rs), which
implements that crate's `BlockDevice` and `TimeSource` over `EMMC`, after which
`embedded-sdmmc` mounts FAT16 or FAT32 and opens files by name. Two things to
know:

* `SdCard::read_only()` fails writes in `BlockDevice::write` rather than trusting
  callers to open files read-only. Prefer it for the card the board
  booted from.
* `NoClock` reports the epoch, because the BCM2837 has no battery-backed RTC and
  a visibly wrong timestamp beats a plausible invented one.

DigiDash used to parse FAT itself, against this driver, and now uses that
feature instead. Both were validated against the same QEMU images — FAT32,
FAT16, a multi-cluster file diffed byte for byte, and an empty slot.

What is still unexercised is what no successful boot can reach: the `SRST_HC` /
`CONTROL1` reset dance against a controller already wedged, and voltage
switching or any UHS mode, neither of which this driver attempts. Writes are
QEMU-only on purpose — the card in a working board is not expendable.

## Related

* [05-drivers-and-mmio.md](05-drivers-and-mmio.md) — the MMIO wrapper and the
  ordering rules every register access here obeys
* [06-synchronisation.md](06-synchronisation.md) — why `EMMC` is a
  `SpinLock<Emmc>` and what that means for calling it from several cores
* [13-heap.md](13-heap.md) — where the block buffers come from
