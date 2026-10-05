# pyrographer

> [!WARNING]
> pyrographer is under active development, and many of its features are untested on real
> hardware. Writing has run on one board so far, a raw image flashed to an RK3588S. A write that
> goes wrong can leave a board unable to boot, or destroy the data on a disk. Use pyrographer at
> your own risk, and only on a device you can afford to lose. Here and in the book,
> `[UNVERIFIED]` marks a feature that passes its tests against scripted devices and has not yet
> run on a real one.

pyrographer is a flashing and recovery toolkit for embedded devices, such as single-board
computers and IP cameras, written entirely in safe Rust. It comprises a command-line tool, a
desktop application and a Rust library, built on the same core.

pyrographer provides four core operations:

- `dump` reads a device's flash to an image file.
- `flash` writes an image to a device, then reads it back to check the result.
- `clone` copies one device's flash directly to another.
- `verify` compares a device's flash against an image.

A write permanently replaces the existing data on its target. Before each write, pyrographer
presents a plan of the changes and proceeds only on explicit confirmation.

## Supported devices

pyrographer supports three SoC families, and block devices on Linux:

- **Rockchip**, over USB. Reading is verified on a real RK3576, byte for byte against
  rkdeveloptool, Rockchip's own tool. Reading and writing are verified on a real RK3588S.
- **StarFive JH7110**, over serial. pyrographer writes the boot flash through the SoC's UART
  recovery mode. That mode cannot read flash, so the write is not read back. pyrographer can also
  boot U-Boot in RAM over the same line, which hands the eMMC to the block-device commands.
- **Ingenic XBurst**, over USB. These SoCs are found in IP cameras such as the Wyze Cam v3.
  pyrographer brings the boot ROM up to DFU, then reads the device's flash. Writing is built,
  and refused until an Ingenic SoC is pinned against real hardware.
- **Block devices**, on Linux, such as an SD card in a reader or a board that presents its
  storage as USB mass storage. pyrographer takes exclusive use of the disk. It refuses every
  disk the running system depends on, with no override.

The architecture leaves room for a Broadcom (CM4) backend, which is not yet built. The book's
[Hardware support](https://gregordinary.github.io/pyrographer/getting-started/hardware-support.html)
page lists what has run on which board.

## Commands

The four core operations run on Rockchip boards and on block devices. An Ingenic device in DFU
supports `dump` and `verify`. `list`, `info` and `partitions` find a device and describe it.
`--partition <name>` aims `dump`, `flash` and `verify` at a partition by name, and refuses an
image too large for it.

`--device` selects a board by `<bus>:<address>`, or a disk by its node path, such as
`/dev/sdb`. pyrographer never selects a disk by default, and `list` shows disks only with
`--blocks`.

Four commands maintain partition tables, in both the GPT and Rockchip's `parameter` format.
`repair-table` and `repair-param` rebuild a damaged copy from an intact one. `author-gpt` and
`author-param` write a fresh table from a layout you supply. All four use the same plan,
confirmation and read-back as `flash`.

Three commands handle what a Rockchip SDK ships. `firmware-info` checks a firmware package, an
`update.img`, with no device attached. `flash-firmware` writes a package as one plan: its
partition images, the GPT its parameter describes, and the ID block. `write-idb` writes the ID
block alone, the first stage the BootROM reads. Both writes use the same gate and read-back as
`flash`.

Four commands serve one vendor each:

- `db` uploads a loader to a Rockchip board in maskrom, the BootROM's USB download mode.
- `usbboot` uploads two stages to an Ingenic boot ROM, which brings the device up in DFU.
- `recover` writes a bootloader to a StarFive board's flash over serial.
- `uartboot` boots a StarFive board into U-Boot over serial, and writes nothing.

Two commands work with a board's bootloader prompt over a serial line:

- `console` watches a board's console for the text that marks success or failure.
- `uboot` drives a U-Boot prompt. It can present the board's storage over USB, boot from
  another source once, or send a single command.

`chipver`, `capability`, `storage` and `reset` query or reset a running Rockchip loader. The
[command-line reference](https://gregordinary.github.io/pyrographer/reference/cli/index.html)
documents every command and option.

## Design

- **Library first.** `pyrographer-core` holds every protocol and every safety check. The
  command-line tool, the desktop application and the web flasher call its Rust API, so all
  three behave the same way.
- **Safe Rust.** Every crate sets `#![forbid(unsafe_code)]`. The dependency set is small.
  `nusb` reaches USB and `serial2` reaches serial ports. `eframe`/`egui` draw the window, `rfd`
  opens file dialogs, and `pollster` runs the async operations.
- **Tested without hardware.** Every wire protocol and on-flash format is a sans-I/O codec, a
  pure function of bytes. The tests run each codec, operation and backend against scripted
  devices.
- **Gated writes.** Every write runs plan, confirmation, write and read-back, in that order.
  The plan names each partition the write touches. On a Rockchip board, the write is refused
  unless the loader identifies the SoC you named. In the library, a write without a
  confirmation does not compile.

The [pyrographer book](https://gregordinary.github.io/pyrographer/) covers installation, every
command, and the library's design. Its source is the mdBook under [`docs/`](docs/).

## Crates

| Crate | What it is |
|---|---|
| `pyrographer-core` | The library: transports, backends, codecs, operations and the write gate. Every public item is documented, and it compiles to `wasm32`, where the browser provides USB through WebUSB. |
| `pyrographer` | The command-line tool. |
| `pyrographer-gui` | The desktop application and the web flasher, built from one egui/eframe crate. |

The library's API is not yet stable.

## Building

The workspace uses Rust **edition 2024** and builds on **1.95 or newer**. The resolver is
MSRV-aware, so it holds each dependency to a version that compiles on that floor.

`cargo build` and `cargo test` cover the default members, the library and the command-line
tool, and pull in no GUI dependencies:

```sh
cargo build            # core + CLI
cargo test             # codecs, verbs, and backends against a scripted transport
cargo build -p pyrographer-gui   # the desktop window
```

The web flasher is a `wasm32-unknown-unknown` build of `pyrographer-gui`, served with
[Trunk](https://trunkrs.dev/). `.cargo/config.toml` sets the `--cfg=web_sys_unstable_apis`
flag that web-sys requires for its WebUSB and Web Serial bindings. The web flasher compiles,
and is `[UNVERIFIED]` in a browser.

[Installing](https://gregordinary.github.io/pyrographer/getting-started/installing.html) covers
each front end, and the permissions each kind of device needs.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
