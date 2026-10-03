//! Web Serial: the serial line in a browser tab.
//!
//! This module is the browser's implementation of the [`Serial`] seam, as
//! [`WebUsbTransport`](super::WebUsbTransport) is of the USB seam. The code that
//! consumes it is the same code that runs against serial2 on a desktop. It includes
//! the codecs, the recovery sender, and the console and U-Boot drivers. None of
//! that code knows which transport it drives.
//!
//! # Differences from the native transport
//!
//! ## Port names
//!
//! Natively, a serial port is a path a person types, and
//! [`SerialTransport::open`](super::SerialTransport::open) opens it. A page cannot
//! open `/dev/ttyUSB0` by path. The browser gives it a port through
//! [`request_port`], in a chooser the page did not draw and cannot script. That
//! chooser opens only on a real user gesture. The two builds therefore differ as
//! the USB builds do, and for the same reason. The native build names a place, and
//! the browser build holds an object.
//!
//! ## The chooser
//!
//! The chooser is not filtered. [`request_device`](super::request_device) filters
//! by vendor ID, because a Rockchip board on the bus is itself a Rockchip device.
//! A serial line has no such identity to filter on. The device plugged into the
//! host is a USB-serial adapter, whose VID names FTDI, WCH or Silicon Labs, never
//! the board on the far end. A filter would hide whichever adapter a person has, so
//! every port is offered and the person picks one.
//!
//! ## Deadlines
//!
//! The host sets its own deadlines, as with WebUSB. The browser gives a stream
//! read no deadline. Every read and every write therefore races [`after`], and
//! whichever settles first is the result.
//!
//! ## Timed-out reads
//!
//! **A timed-out read keeps its place in the queue.** The two browser transports
//! differ here. A stream reader serves its `read()` calls in order. If one read is
//! abandoned and another issued, the next chunk that arrives goes to the abandoned
//! promise. Nothing awaits that promise, so those bytes are lost.
//!
//! On the USB seam this matters little. A bulk transfer that times out leaves the
//! agent [`Desynchronized`](crate::Error::Desynchronized), and every later command
//! is refused. On a serial line, a read timeout is routine. The recovery sender
//! reads a timeout as "the receiver has not started yet, retransmit". The console
//! spends a budget of timed-out reads while it waits for a prompt to appear.
//!
//! A transport that dropped a chunk per timeout would lose the first `C` of the
//! XMODEM handshake, and the first line a board prints. A read whose deadline
//! passes is therefore **kept**. The next [`read`](Serial::read) races the same
//! promise again rather than asking for a new one.
//!
//! ## Chunks
//!
//! The stream delivers whatever has arrived as one chunk, which can be more than
//! the caller asked for. The surplus is held and served first on the next read.
//! Natively, the operating system's buffer does this, and the seam never sees it.
//!
//! ## Closing the port
//!
//! **The port is given back on drop.** A WebUSB device that stays open can be
//! opened again. A `SerialPort` that stays open throws `InvalidStateError` on the
//! next `open()`. Without the release, a second recovery attempt in the same tab
//! would fail against a port the first attempt still held. The `Drop`
//! implementation at the end of this file releases the port.
//!
//! # The read deadline
//!
//! [`READ_DEADLINE`] is one second because the native
//! [`SerialTransport`](super::SerialTransport)'s `READ_TIMEOUT` is one second. The
//! drivers that consume this seam count their waits in reads, not seconds, because
//! `Instant::now()` panics on `wasm32`.
//! [`console::DEFAULT_READS`](crate::console::DEFAULT_READS) is 30 reads,
//! documented as ~30 seconds of silence. That holds only while the two transports
//! agree on the length of a read. A ten-second deadline here would silently turn
//! that budget into five minutes.
//!
//! # Browser support
//!
//! Web Serial needs the same three conditions WebUSB does:
//!
//! - A Chromium browser: Chrome, Edge or Chromium
//! - A secure context: HTTPS, or `localhost`
//! - A user gesture behind the chooser
//!
//! It is not available in Firefox or Safari. **\[DOC\]**
//!
//! The scripted-serial tests pin every byte layout this transport carries, which it
//! shares with the native path. This file itself is **\[UNVERIFIED\]** against a
//! browser and a board.

use std::time::Duration;

use js_sys::{Array, Promise, Uint8Array};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    FlowControlType, ParityType, ReadableStreamDefaultReader, ReadableStreamReadResult,
    SerialOptions, SerialPort, WritableStreamDefaultWriter,
};

use super::Serial;
use super::chunk::ChunkBuffer;
use super::web::{after, await_js, describe, timed_out};
use crate::{Error, Result};

/// How long one read waits for bytes before it is abandoned.
///
/// It is one second, and it must match [`SerialTransport`](super::SerialTransport)
/// exactly. The drivers that consume this seam spend a budget counted in reads
/// rather than a `Duration`, because `Instant::now()` panics on `wasm32`. The
/// wall-clock length of every wait in [`console`](crate::console) and
/// [`recovery`](crate::recovery) is therefore this constant multiplied by a count.
/// Changing it here alone would change those budgets in the browser and nowhere
/// else. Neither this value nor the native one is measured against a board.
/// **\[WEAK\]**
const READ_DEADLINE: Duration = Duration::from_secs(1);

/// How long one write waits to be accepted before it is abandoned.
///
/// It is two seconds, matching the native transport. A write hands bytes to the
/// browser's sink and returns, so this limit is a backstop against a wedged port,
/// not a throttle. **\[WEAK\]**
const WRITE_DEADLINE: Duration = Duration::from_secs(2);

/// How much of the line the browser holds between reads.
///
/// It is 4 KiB, which at 115200 baud is ~355 ms of a saturated line. That is
/// longer than the gap between one read returning and the next being issued. It is
/// also far more than the 512 bytes [`console`](crate::console) takes in one read.
/// The number bounds how far behind the host can fall before the browser stops
/// taking bytes off the line. It is not a performance setting.
///
/// The size is the high-water mark of the port's readable stream, and Web Serial's
/// default is 255 bytes. While that many bytes wait unread, the browser stops
/// reading from the port. An overrun the platform reports errors the stream with
/// `BufferOverrunError`. **\[DOC\]**
///
/// The line runs with no flow control, so nothing asks the board to pause. Bytes
/// the browser leaves unread wait in the operating system's receive buffer, and are
/// lost once it fills. How Chromium and each platform behave there is
/// **\[UNVERIFIED\]** in a browser.
const BUFFER_SIZE: u32 = 4096;

/// A serial line the browser handed to the page.
pub struct WebSerialTransport {
    port: SerialPort,
    reader: ReadableStreamDefaultReader,
    writer: WritableStreamDefaultWriter,
    /// Bytes a chunk delivered that the caller's buffer had no room for.
    ///
    /// They are served before another chunk is asked for, so no byte the line sent
    /// is lost to a chunk larger than a read. The buffer is sans-I/O and tested on
    /// every commit, in [`chunk`](super::chunk).
    held: ChunkBuffer,
    /// A read whose deadline passed while it was still pending.
    ///
    /// It is held rather than dropped, and the next [`read`](Serial::read) races
    /// it again. A reader serves its reads in order, so an abandoned promise would
    /// receive the next chunk that arrives, with nothing awaiting it. A serial read
    /// timeout is the ordinary case rather than a fault. The module docs give the
    /// detail.
    pending: Option<Promise>,
}

/// `navigator.serial`, or a refusal that says the browser has no Web Serial.
///
/// The property is checked rather than assumed, for the same reason as
/// [`usb`](super::WebUsbTransport). It is absent outside Chromium and in an
/// insecure context, and reaching through it there throws a JavaScript exception.
/// In a wasm build, that exception is a panic that takes the whole instance down,
/// not an error a caller can report.
fn serial() -> Result<web_sys::Serial> {
    let navigator = web_sys::window()
        .ok_or_else(|| {
            Error::Transport("there is no window to reach a serial line through".to_string())
        })?
        .navigator();

    let found =
        js_sys::Reflect::get(navigator.as_ref(), &JsValue::from_str("serial")).map_err(|e| {
            Error::Transport(format!("cannot reach navigator.serial: {}", describe(&e)))
        })?;

    if found.is_undefined() || found.is_null() {
        return Err(Error::Transport(
            "this browser does not offer Web Serial. pyrographer's web flasher needs a \
             Chromium-based browser (Chrome, Edge, or Chromium) on a secure origin: HTTPS or \
             localhost."
                .to_string(),
        ));
    }

    Ok(navigator.serial())
}

/// Ask the person to pick a serial port, and return the port they picked.
///
/// This is the serial acquisition seam, and the counterpart of
/// [`request_device`](super::request_device). The browser does not enumerate
/// serial ports for a page, and `requestPort` is the only way to obtain one. It
/// opens a dialog the page did not draw and cannot script. It fails unless it is
/// called from a real user gesture.
///
/// `Ok(None)` means the person closed the chooser without picking a port. That is
/// a result, not a failure.
///
/// No filter is passed. The device plugged into the host is a USB-serial adapter,
/// whose identity is the adapter's, not the board's. A filter could only hide the
/// port the person meant to pick.
pub async fn request_port() -> Result<Option<SerialPort>> {
    let serial = serial()?;

    match JsFuture::from(serial.request_port()).await {
        Ok(port) => Ok(Some(port)),
        // The chooser rejects when it is dismissed, and it rejects when the page
        // has no permission to open one. They are not the same thing, and only
        // the first is a person deciding not to.
        Err(error) => {
            if describe(&error).contains("NotFoundError") {
                Ok(None)
            } else {
                Err(Error::Transport(format!(
                    "the browser would not open a serial-port chooser: {}",
                    describe(&error)
                )))
            }
        }
    }
}

/// List the serial ports this origin has already been given permission for.
///
/// **This is not an enumeration of connected hardware**, as with
/// [`list_permitted`](super::list_permitted). `getPorts` returns only the ports a
/// person has already granted this origin through the chooser. An adapter that was
/// never picked is absent from the list, even while it is plugged in. A port
/// granted once returns on the next page load, with no second trip through the
/// chooser and its user gesture.
///
/// The permission is per origin and outlives the tab. It lasts until the person
/// revokes the grant in the browser's own site settings, the only place a grant can
/// be revoked.
///
/// **\[UNVERIFIED\]** against a browser. The specification says a grant persists
/// across a reload, and that has not been observed here.
pub async fn list_permitted_ports() -> Result<Vec<SerialPort>> {
    let serial = serial()?;

    let ports = JsFuture::from(serial.get_ports()).await.map_err(|e| {
        Error::Transport(format!(
            "the browser would not list the serial ports it has permission for: {}",
            describe(&e)
        ))
    })?;

    Ok(ports.into_iter().collect())
}

impl WebSerialTransport {
    /// Open a port the browser handed over, at [`DEFAULT_BAUD`].
    ///
    /// [`DEFAULT_BAUD`]: super::DEFAULT_BAUD
    pub async fn open(port: SerialPort) -> Result<Self> {
        Self::open_at(port, super::DEFAULT_BAUD).await
    }

    /// Open a port the browser handed over, at `baud`.
    ///
    /// The line is configured at 8N1 (8 data bits, no parity, one stop bit) with no
    /// flow control, the same line the native transport opens. Every field is set
    /// explicitly rather than left to the specification's defaults, so both
    /// implementations of the seam state the same configuration in code.
    ///
    /// The reader and the writer are taken here and held for the transport's life.
    /// A `SerialPort`'s streams can be locked by one reader at a time. A reader
    /// taken per read would have to release its lock while a chunk was still in
    /// flight.
    pub async fn open_at(port: SerialPort, baud: u32) -> Result<Self> {
        let options = SerialOptions::new(baud);
        options.set_data_bits(8);
        options.set_stop_bits(1);
        options.set_parity(ParityType::None);
        options.set_flow_control(FlowControlType::None);
        options.set_buffer_size(BUFFER_SIZE);

        await_js(port.open(&options), "opening the serial port")
            .await
            .map_err(open_error)?;

        // From here the port is **open**, and every failure below has to give it
        // back before it reports. There is no transport yet for `Drop` to run
        // against, and a port left open cannot be opened again -- so a reader the
        // browser would not hand over would otherwise cost this tab the port
        // permanently, for the rest of the page's life.
        let reader = match port
            .readable()
            .get_reader()
            .dyn_into::<ReadableStreamDefaultReader>()
        {
            Ok(reader) => reader,
            Err(_) => {
                give_back(&port);
                return Err(Error::Transport(
                    "the browser gave the serial port a reader that does not read byte chunks"
                        .to_string(),
                ));
            }
        };

        let writer = match port.writable().get_writer() {
            Ok(writer) => writer,
            Err(e) => {
                // The reader holds a lock and `close()` rejects while either
                // stream is locked, so it goes first. Releasing it bare is safe
                // here, unlike in the `Drop` teardown: nothing has read through it
                // yet, so there is no pending read for the release to reject.
                reader.release_lock();
                give_back(&port);
                return Err(Error::Transport(format!(
                    "cannot write to the serial port: {}",
                    describe(&e)
                )));
            }
        };

        Ok(Self {
            port,
            reader,
            writer,
            held: ChunkBuffer::default(),
            pending: None,
        })
    }

    /// The `SerialPort` this transport drives, so a caller can tell one line from
    /// another.
    ///
    /// The web build's recovery confirmation depends on this method, as the USB
    /// confirmation depends on
    /// [`WebUsbTransport::device`](super::WebUsbTransport::device). Natively, a
    /// person types the port path. In a browser there is no path to type, so the
    /// person picks the port again from the chooser. The re-pick is a confirmation
    /// because the picked port is compared with the port the plan was made for.
    pub fn port(&self) -> &SerialPort {
        &self.port
    }
}

/// Close a port an `open_at` could not finish setting up.
///
/// The close is fire-and-forget, as the [`Drop`](WebSerialTransport::drop)
/// teardown is. `close()` is a promise, and the caller is about to return an error
/// that says what went wrong. The caller needs the close to be under way, not
/// finished.
fn give_back(port: &SerialPort) {
    let port = port.clone();
    wasm_bindgen_futures::spawn_local(async move {
        let _ = JsFuture::from(port.close()).await;
    });
}

/// Turn a failed port open into an error that names the remedy.
///
/// Two of the ways `open()` fails have the same remedy for a person.
/// `InvalidStateError` is a port this page already opened and did not give back.
/// `NetworkError` is a port something else holds, usually a terminal emulator left
/// running. That is the most common reason a serial line is unavailable on any
/// host. Both become [`Error::Busy`] with the remedy attached.
///
/// Any other error is passed through unchanged. A remedy that names the wrong
/// cause misleads a person more than no remedy does.
fn open_error(error: Error) -> Error {
    let said = error.to_string();
    if said.contains("InvalidStateError") || said.contains("NetworkError") {
        Error::Busy(format!(
            "{error}. A serial port can be open in only one place at a time. Close any terminal \
             or flashing tool that holds it, and reload the page if this tab opened it earlier."
        ))
    } else {
        error
    }
}

/// Turn a rejected stream operation into the crate's [`Error`].
///
/// It is the counterpart of the native transport's `io_error`, and makes the same
/// distinction. A port that has gone away is
/// [`Disconnected`](Error::Disconnected), not a generic transport failure. The
/// browser rejects a read or a write on a lost device with a `NetworkError`
/// DOMException. That is the adapter unplugged mid-recovery, the one failure here
/// whose cause a person can see at the bench. Every other error keeps the
/// browser's own words.
///
/// A deadline never reaches this function. [`after`] resolves rather than
/// rejects, so a timed-out transfer is a settled value.
fn stream_error(error: &JsValue, what: &str) -> Error {
    let said = describe(error);
    if said.contains("NetworkError") {
        Error::Disconnected
    } else {
        Error::Transport(format!("{what} failed: {said}"))
    }
}

impl Serial for WebSerialTransport {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        // A zero-length read is answered the way the native transport answers
        // one -- serial2 reports it as a timeout -- rather than with a silent
        // `Ok(0)` a driver in a read loop would spin on. No caller asks for one;
        // what matters is that both implementations of the seam behave alike.
        if buf.is_empty() {
            return Err(Error::Timeout {
                what: "serial read",
                waited: READ_DEADLINE,
            });
        }

        // What the last chunk was too big to hand over comes first. Asking the
        // stream for more while bytes are still held here would put them out of
        // order, which on a line carrying XMODEM is a corrupt block.
        if self.held.has_bytes() {
            return Ok(self.held.take(buf));
        }

        // **The timer is armed before the read is taken out of the slot**, so that
        // the one fallible step here cannot be the step that loses a read. `after`
        // returns an error where there is no window to set a timer on; taking the
        // pending read first and then returning through `?` would drop a promise
        // still queued on the reader, and the next read would be served the chunk
        // that one was owed. That is the exact reordering this module is built to
        // prevent, so the order is the fix rather than an unwind.
        let timer = after(READ_DEADLINE)?;

        // A read left pending by an earlier deadline is raced again rather than
        // replaced. The module docs say why at length; the short of it is that a
        // reader serves its reads in order, so replacing this one would hand it
        // the next chunk with nobody waiting.
        let read = match self.pending.take() {
            Some(pending) => pending,
            None => self.reader.read(),
        };
        let race = Promise::race(Array::of2(read.as_ref(), timer.as_ref()).as_ref());

        let settled = JsFuture::from(race)
            .await
            .map_err(|e| stream_error(&e, "serial read"))?;

        if timed_out(&settled) {
            // The timer won. Keep the read: it is still in the reader's queue and
            // the chunk it is owed has not arrived yet.
            self.pending = Some(read);
            return Err(Error::Timeout {
                what: "serial read",
                waited: READ_DEADLINE,
            });
        }

        let result: ReadableStreamReadResult = settled.unchecked_into();

        // `done` is the stream closing under us, which for a serial port is the
        // line going away: the adapter unplugged, or the port closed elsewhere.
        // There is nothing left to read and there never will be, so this is
        // `Disconnected` rather than a short read the caller would retry forever.
        if result.get_done().unwrap_or(false) {
            return Err(Error::Disconnected);
        }

        // Checked, not assumed. Web Serial's readable is a stream of
        // `Uint8Array`, but the cast off a `JsValue` is unchecked, and reading a
        // length and a slice off something that is not a typed array throws --
        // which in a wasm build is a panic that takes the instance down rather
        // than an error anything can report.
        let chunk = result
            .get_value()
            .dyn_into::<Uint8Array>()
            .map_err(|_| {
                Error::Protocol(
                    "the serial port delivered a chunk that is not a byte array".to_string(),
                )
            })?
            .to_vec();

        // A chunk with no bytes in it is legal and says nothing, so it is reported
        // the way silence is: the drivers above treat a timeout as "still quiet"
        // and read again, and handing back `Ok(0)` instead would spin them.
        if chunk.is_empty() {
            return Err(Error::Timeout {
                what: "serial read",
                waited: READ_DEADLINE,
            });
        }

        Ok(self.held.fill(&chunk, buf))
    }

    async fn write_all(&mut self, data: &[u8]) -> Result<()> {
        // A timed-out write is *not* retained the way a timed-out read is, and the
        // asymmetry is the callers'. A read timeout is routine and the next read
        // wants the same bytes; a write timeout is an error every driver here
        // propagates with `?`, ending the operation -- so there is no next write
        // for a retained promise to be raced by, and the bytes may or may not have
        // reached the line. That is the same indeterminacy a native partial write
        // leaves behind, reported the same way.
        let chunk = Uint8Array::from(data);
        let write = self.writer.write_with_chunk(chunk.as_ref());

        let timer = after(WRITE_DEADLINE)?;
        let race = Promise::race(Array::of2(write.as_ref(), timer.as_ref()).as_ref());

        let settled = JsFuture::from(race)
            .await
            .map_err(|e| stream_error(&e, "serial write"))?;

        // The marker, not `undefined`: a stream write resolves with `undefined`
        // when it *succeeds*, so a deadline inferred from that value would report
        // every completed write as a timeout. See `web::DEADLINE_MARKER`.
        if timed_out(&settled) {
            return Err(Error::Timeout {
                what: "serial write",
                waited: WRITE_DEADLINE,
            });
        }

        Ok(())
    }
}

impl Drop for WebSerialTransport {
    /// Give the port back.
    ///
    /// **A `SerialPort` left open cannot be opened again.** `open()` on one throws
    /// `InvalidStateError`. Without this teardown, a recovery that ended
    /// (succeeded, failed, or canceled) would leave the next attempt in the same
    /// tab facing a port the earlier attempt still holds. The GUI's recovery and
    /// console jobs each drop their transport as the job ends, which is where the
    /// release must happen. `WebUsbTransport` needs no counterpart, because opening
    /// an already-open USB device is a no-op.
    ///
    /// The teardown steps run in a fixed order. The reader is **canceled** rather
    /// than only released, because releasing a lock with a read still pending
    /// rejects that read. A rejected promise that nothing awaits is an unhandled
    /// rejection in the console on every job. Canceling resolves the read with
    /// `done` instead. The locks are released next, and `close()` runs last,
    /// because it rejects while either stream is still locked.
    ///
    /// The teardown runs on the browser's event loop, because every step is a
    /// promise and `Drop` cannot await. No caller needs the result, because the
    /// port is being given up, so the task is spawned and its outcome discarded.
    /// The port is therefore released shortly after the transport is dropped, and
    /// no code here waits for the release.
    fn drop(&mut self) {
        let reader = self.reader.clone();
        let writer = self.writer.clone();
        let port = self.port.clone();

        wasm_bindgen_futures::spawn_local(async move {
            let _ = JsFuture::from(reader.cancel()).await;
            reader.release_lock();

            let _ = JsFuture::from(writer.abort()).await;
            writer.release_lock();

            let _ = JsFuture::from(port.close()).await;
        });
    }
}
