# Devices: sensors and panels

`src/devices/` is the generic half of driving an off-the-shelf camera or
display. It describes no device. A particular sensor or panel is a register
table and a mode, and lives in a crate of its own that implements the traits
here.

## Why device tables are not in this crate

This crate is GPL-3.0-only with a commercial licence beside it, which needs
every line relicensable by the copyright holder. In practice, sensor and panel
register tables come from vendor reference code or the Linux drivers, and the
Linux drivers are **GPL-2.0-only**. That code cannot be relicensed, and
GPL-2.0-only code cannot even be combined with GPL-3.0-only code in a binary
that is distributed. Building one for a board on your own desk triggers
neither licence; shipping one does. So the tables stay out, and a device crate
has to settle its own provenance: a datasheet, a vendor's permissive reference,
or private use.

## Cameras

* **`cci::Cci`**: the camera control interface — I2C with 16-bit (or, for
  older OmniVision parts, 8-bit) register addresses, big-endian values,
  `write_table` for the `(register, value)` lists vendors publish. Generic
  over the BSP's `I2cBus`, so hardware BSC and bit-banged buses both work.
* **`sensor::Sensor`**: `identify`, `configure` (returning a `SensorMode`),
  `start`, `stop`. A `SensorMode` produces the matching `unicam::Config`.
* **`sensor::capture`**: one frame, the SoC side done once for every sensor —
  receiver powered and listening before the sensor starts, both stopped after.
* **`bayer::render`**: a raw frame to a `Canvas` (or a `Canvas::sub` window of
  one), one pixel per 2x2 filter
  cell, decimated to fit, with white-balance gains. RAW8, RAW10 and RAW12
  packed. A preview: no black level, lens shading or colour matrix.

`camera-preview` renders a synthetic RAW10 frame through `bayer::render` and
checks the colours, so everything after the receiver is verified under QEMU.
With a sensor driver plugged in, it shows a live frame on HDMI.

## Displays

* **`panel::Step`** and **`panel::run`**: an initialisation table of DCS
  writes, generic writes, bridge register writes and delays, the shape
  vendors publish.
* **`panel::bridge_write`**: a 16-bit-address, 32-bit-value register write in
  one generic long packet, the form Toshiba's DSI bridges take.
* **`panel::Panel`**: the mode (`video`), `prepare` with the host up and no
  video yet, `enable` and `disable`. Board work such as a reset GPIO or an I2C
  power controller belongs in `prepare`; the trait's `Error` type, which only
  has to convert from `dsi::Error`, carries that work's failures too.
* **`panel::start`** and **`stop`**: the whole bring-up from
  [28-dsi-video.md](28-dsi-video.md), with the panel's steps in the right
  places.

## What a device crate looks like

For the Pi's official 7-inch display, say: a `Panel` whose `video` gives the
800x480 one-lane RGB888 mode; whose `prepare` powers the panel through its
board controller on I2C and sends the bridge's register writes as
`Step::Bridge`; and nothing else. For a camera: a `Sensor` holding a `Cci`,
whose `configure` writes the mode table and returns its `SensorMode`.
