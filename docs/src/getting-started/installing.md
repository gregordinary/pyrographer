# Installing

pyrographer builds from source with Cargo. The command-line tool and the window are two
binaries, and the web flasher is a page you serve yourself. Every build needs Rust 1.95 or
newer, which [rustup](https://rustup.rs/) installs.

## The command-line tool

Clone the repository and install the tool from it:

```sh
git clone https://github.com/gregordinary/pyrographer
cd pyrographer
cargo install --locked --path crates/pyrographer
```

`cargo install` builds a release binary and puts `pyrographer` in `~/.cargo/bin`, which rustup
adds to your `PATH`. `--locked` builds the dependency versions in `Cargo.lock`, which are the
versions every quality gate ran against.

## The window

On Debian or Ubuntu, install the X11, Wayland and OpenGL development packages first. These are
the packages CI installs before it builds the window:

```sh
sudo apt-get install --no-install-recommends libgl1-mesa-dev libwayland-dev libx11-dev \
  libxcursor-dev libxi-dev libxkbcommon-dev libxrandr-dev
```

Then install the window from the repository's root:

```sh
cargo install --locked --path crates/pyrographer-gui
```

That puts `pyrographer-gui` in `~/.cargo/bin`. [The window](../reference/gui.md) describes what
it draws.

## The web flasher

The web flasher is the window compiled to WebAssembly. It needs the `wasm32-unknown-unknown`
target and [Trunk](https://trunkrs.dev/):

```sh
rustup target add wasm32-unknown-unknown
cargo install --locked trunk
```

To try it, run Trunk from the GUI crate's directory. It builds the page, serves it on
`localhost`, and opens it in your browser:

```sh
cd crates/pyrographer-gui
trunk serve --open
```

To host it, build the page into `crates/pyrographer-gui/dist`, and serve that directory over
HTTPS:

```sh
cd crates/pyrographer-gui
trunk build --release
```

The page needs a Chromium browser and a secure origin, which is HTTPS or `localhost`. The web
flasher compiles, and is `[UNVERIFIED]` in a browser.
[The web flasher](../reference/gui.md#the-web-flasher) covers what differs in a tab.

## Permissions

`list` reads descriptors the operating system has already cached, so it needs no permission.
The commands that open a device can be refused, and each kind of device takes its own grant.

### USB boards on Linux

Grant access with a udev rule for each vendor ID pyrographer scans. Write these lines to
`/etc/udev/rules.d/99-pyrographer.rules`:

```text
# Rockchip
SUBSYSTEM=="usb", ATTR{idVendor}=="2207", MODE="0660", TAG+="uaccess"
# Ingenic
SUBSYSTEM=="usb", ATTR{idVendor}=="a108", MODE="0660", TAG+="uaccess"
SUBSYSTEM=="usb", ATTR{idVendor}=="601a", MODE="0660", TAG+="uaccess"
```

Then reload the rules, and replug the board:

```sh
sudo udevadm control --reload-rules && sudo udevadm trigger
```

A rule on the vendor ID also covers the vendor's newer SoCs, which a rule on a product ID does
not.

If a board will not open, pyrographer prints the rule for that board's vendor.

### Serial ports on Linux

A USB-serial adapter's node, such as `/dev/ttyUSB0`, belongs to the `dialout` group. On some
distributions, such as Arch Linux, the group is `uucp`. Add yourself to the group, then log out
and back in:

```sh
sudo usermod -aG dialout "$USER"
```

### Block devices on Linux

Listing disks needs no privilege. Every other command on a disk opens it first, including
`flash --dry-run`, and opening a disk needs root or membership of the `disk` group. Run those
commands under `sudo`. Membership of `disk` grants
raw access to every disk on the machine, including the one the system runs from.
[Privilege](../reference/cli/block-devices.md#privilege) says which steps open the disk.

### USB boards on Windows

On Windows, pyrographer reaches a board through the WinUSB driver, so that driver must be bound
to the board. Running pyrographer on Windows is `[UNVERIFIED]`.
