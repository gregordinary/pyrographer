//! The native serial transport, backed by serial2.
//!
//! serial2 calls the operating system's serial stack, so this module is compiled
//! on native targets only. [`WebSerialTransport`](super::WebSerialTransport) is
//! the browser's implementation of the same [`Serial`] seam.
//!
//! Setting a baud rate is irreducible platform FFI on every OS: termios and ioctl
//! on Unix, and the Win32 equivalent on Windows. serial2 confines that unsafe code
//! to itself, through libc on Unix and windows-sys on Windows. This crate
//! therefore keeps `#![forbid(unsafe_code)]`.
//!
//! **This transport blocks the thread it is driven on**, as
//! [`UsbTransport`](super::UsbTransport) does. A read has a wall-clock deadline,
//! and serial2 enforces it by blocking until bytes arrive or the deadline passes.
//! The methods are `async` because the seam is async. Web Serial suspends a read,
//! and this transport blocks instead, which costs nothing under `pollster`.
//!
//! The line is opened at 8N1 (8 data bits, no parity, one stop bit) with no flow
//! control, at a baud rate the caller names. serial2 applies exactly that from a
//! bare baud rate: a `u32` selects raw mode, 8N1, and no flow control.
//!
//! [`READ_TIMEOUT`] and the browser transport's `READ_DEADLINE` are both one
//! second, and must stay equal. The drivers that consume this seam spend budgets
//! counted in reads rather than seconds, because `Instant::now()` panics on
//! `wasm32`. This constant therefore sets what a budget is worth in wall-clock
//! time. Changing one constant alone changes the length of every wait in
//! [`console`](crate::console) and [`recovery`](crate::recovery) on one platform
//! only.
//!
//! [`DEFAULT_BAUD`] is 115200, the JH7110 recovery UART's rate. A board's console
//! runs at whatever rate its build sets, and an RK3576's runs at 1500000.
//! [`SerialTransport::open`] uses the default, and [`SerialTransport::open_at`]
//! takes the rate as a parameter.

use serial2::SerialPort;
use std::io;
use std::time::Duration;

use super::{DEFAULT_BAUD, Serial};
use crate::{Error, Result};

/// The longest a single read waits for bytes before it is abandoned.
///
/// It bounds one read, not the whole recovery. The XMODEM sender makes many
/// reads, and each returns as soon as any bytes arrive. On a live line this limit
/// is never reached. It is reached only on a silent line, such as a board that is
/// not strapped into recovery or not powered. There it lets the sender give up
/// rather than hang.
///
/// The recovery driver treats a read timeout as a retransmit trigger while it
/// waits for the receiver, so the value is generous but finite. It is not
/// measured against hardware. **\[WEAK\]**
const READ_TIMEOUT: Duration = Duration::from_secs(1);

/// The longest a single write takes before it is abandoned.
///
/// A write goes into the OS serial buffer and returns quickly, so this limit is a
/// backstop against a wedged port, not a throttle. **\[WEAK\]**
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// The native serial transport, backed by serial2.
pub struct SerialTransport {
    port: SerialPort,
}

impl SerialTransport {
    /// Open the serial port at `path` at [`DEFAULT_BAUD`].
    ///
    /// A serial port is a path a person names (`/dev/ttyUSB0`, `COM3`), not a
    /// device on a bus with a USB identity. There is nothing to scan or classify.
    /// This is the serial counterpart of
    /// [`UsbTransport::open`](super::UsbTransport::open), with no discovery step
    /// before it. The read and write deadlines are set here, so every transfer a
    /// driver makes over this transport is bounded.
    pub fn open(path: &str) -> Result<Self> {
        Self::open_at(path, DEFAULT_BAUD)
    }

    /// Open the serial port at `path` at `baud`.
    ///
    /// This is [`open`](Self::open) with the rate named. A JH7110 recovery UART
    /// runs at 115200, the rate its ROM sets. A board's console runs at whatever
    /// rate its build sets, so the rate is a parameter here, and the console flow
    /// asks for it.
    pub fn open_at(path: &str, baud: u32) -> Result<Self> {
        let mut port = SerialPort::open(path, baud).map_err(|e| open_error(e, path))?;
        port.set_read_timeout(READ_TIMEOUT)
            .map_err(|e| open_error(e, path))?;
        port.set_write_timeout(WRITE_TIMEOUT)
            .map_err(|e| open_error(e, path))?;
        Ok(Self { port })
    }
}

/// Turn a serial-port open failure into an error a person can act on.
///
/// A missing port is [`Error::InvalidRequest`] **naming the path**, not the bare
/// [`Error::DeviceNotFound`] a missing USB device returns. A serial port is a path
/// a person typed, so the mistake is in what they typed. An error that does not
/// repeat the path back gives them nothing to correct.
///
/// A refused permission is [`Error::Transport`] naming the path and the remedy. On
/// Linux it is the common failure, from a user outside the `dialout` or `uucp`
/// group, so the message names both groups. Everything else is
/// [`Error::Transport`] with the system's own words and the path.
fn open_error(err: io::Error, path: &str) -> Error {
    match err.kind() {
        io::ErrorKind::NotFound => Error::InvalidRequest(format!(
            "there is no serial port at {path}. On Linux, a USB-serial adapter is usually \
             /dev/ttyUSB0 or /dev/ttyACM0, and `ls /dev/ttyUSB* /dev/ttyACM*` lists them. On \
             Windows, it is a COM port."
        )),
        io::ErrorKind::PermissionDenied => Error::Transport(format!(
            "cannot open serial port {path}: permission denied. On Linux, access usually \
             requires membership of the 'dialout' or 'uucp' group. Add your user to that group, \
             then log in again."
        )),
        _ => Error::Transport(format!("cannot open serial port {path}: {err}")),
    }
}

/// Turn a serial read or write failure into the crate's [`Error`].
///
/// A read that waits out its deadline is [`Error::Timeout`]. The recovery driver
/// reads that as "the receiver has gone quiet": a retransmit trigger while it
/// waits for the first `C`, not a fault. A port that has gone away (the USB-serial
/// adapter unplugged) is [`Error::Disconnected`]. Anything else is
/// [`Error::Transport`].
fn io_error(err: io::Error, what: &'static str, waited: Duration) -> Error {
    match err.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => Error::Timeout { what, waited },
        io::ErrorKind::BrokenPipe | io::ErrorKind::NotConnected | io::ErrorKind::UnexpectedEof => {
            Error::Disconnected
        }
        _ => Error::Transport(format!("{what}: {err}")),
    }
}

impl Serial for SerialTransport {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        match self.port.read(buf) {
            // A serial read returns as soon as any bytes arrive, so a short read
            // is normal and carries no meaning of its own. A read of zero,
            // though, is the deadline passing with nothing to show -- serial2
            // reports that as a timeout, but a driver in a read loop must never
            // be handed a silent zero to spin on, so it is made one here too.
            Ok(0) => Err(Error::Timeout {
                what: "serial read",
                waited: READ_TIMEOUT,
            }),
            Ok(n) => Ok(n),
            Err(e) => Err(io_error(e, "serial read", READ_TIMEOUT)),
        }
    }

    async fn write_all(&mut self, data: &[u8]) -> Result<()> {
        self.port
            .write_all(data)
            .map_err(|e| io_error(e, "serial write", WRITE_TIMEOUT))
    }
}
