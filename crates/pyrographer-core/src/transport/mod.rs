//! The transport seam: async byte movement to a device.
//!
//! The trait is async because the browser transports, WebUSB and Web Serial, are
//! async-only. The native transports are async as well. Backends are generic over
//! it, so a scripted mock stands in for hardware in tests, and the verbs are
//! written once.
//!
//! The seams are portable, and their implementations are not. There are four, one
//! per seam per platform:
//!
//! | Seam | Native | Browser |
//! |---|---|---|
//! | [`Transport`] | `UsbTransport`, over nusb | `WebUsbTransport`, over WebUSB |
//! | [`Serial`] | `SerialTransport`, over serial2 | `WebSerialTransport`, over Web Serial |
//!
//! nusb and serial2 call the operating system's own stacks, so they build on
//! native targets only, not on `wasm32`. The module exports exactly the pair that
//! runs where it is built. The names in the table are not links, because half of
//! them are absent from any one build's documentation.
//!
//! # Two seams
//!
//! [`Transport`] is USB-shaped: control transfers, and a bulk pipe pair with packet
//! boundaries. StarFive's JH7110 recovers over a serial line, which has no packets
//! and no endpoints, only a byte stream. [`Transport`]'s type cannot describe that
//! line accurately. [`Serial`] is therefore a second seam, and a backend names the
//! seam it uses. The browser implementation of [`Serial`] needs the same
//! `--cfg=web_sys_unstable_apis` that gates WebUSB.

#[cfg(not(target_arch = "wasm32"))]
mod usb;

#[cfg(not(target_arch = "wasm32"))]
pub use usb::UsbTransport;

#[cfg(not(target_arch = "wasm32"))]
mod serial;

#[cfg(not(target_arch = "wasm32"))]
pub use serial::SerialTransport;

// The chunk buffer is Web Serial's, but it is portable and it is tested, so it is
// compiled under `test` on every target as well as in the build that uses it. See
// the module's own docs for why that is worth the `cfg`.
#[cfg(any(target_arch = "wasm32", test))]
mod chunk;

#[cfg(target_arch = "wasm32")]
mod web;

#[cfg(target_arch = "wasm32")]
mod webusb;

#[cfg(target_arch = "wasm32")]
pub use webusb::{WebUsbTransport, list_permitted, request_device};

#[cfg(target_arch = "wasm32")]
mod webserial;

#[cfg(target_arch = "wasm32")]
pub use webserial::{WebSerialTransport, list_permitted_ports, request_port};

use crate::Result;

/// The default baud rate for a serial line.
///
/// It is the JH7110 recovery UART's rate. A board's console runs at whatever rate
/// its build sets, and an RK3576's console runs at 1500000. Both implementations
/// of [`Serial`] take a rate, and the console flow asks for one.
///
/// Both implementations read this one constant, so the browser and the native
/// build open a line at the same default rate.
pub const DEFAULT_BAUD: u32 = 115_200;

/// A USB control transfer: the SETUP packet, and the data that goes with it.
///
/// `request_type` is the raw `bmRequestType` byte, which keeps the seam
/// vendor-neutral. The caller passes the byte a vendor's protocol reference gives,
/// and the transport decodes it.
#[derive(Debug, Clone)]
pub enum Control<'a> {
    /// A device-to-host transfer, reading up to `length` bytes.
    In {
        /// The `bmRequestType` field.
        request_type: u8,
        /// The `bRequest` field.
        request: u8,
        /// The `wValue` field.
        value: u16,
        /// The `wIndex` field.
        index: u16,
        /// The `wLength` field: how many bytes to read.
        length: u16,
    },
    /// A host-to-device transfer, sending `data`.
    Out {
        /// The `bmRequestType` field.
        request_type: u8,
        /// The `bRequest` field.
        request: u8,
        /// The `wValue` field.
        value: u16,
        /// The `wIndex` field.
        index: u16,
        /// The bytes to send.
        data: &'a [u8],
    },
}

/// An async link to a device in a boot or recovery state.
///
/// A device in a boot state exposes exactly one bulk pipe pair. So the transport
/// owns its bulk endpoints, finding them as it opens the device, rather than taking
/// an endpoint address on every call. That matches the shape of every backend in
/// the set: rockusb and Ingenic stage2 each use one pair, and DFU uses control
/// transfers alone.
pub trait Transport {
    /// Issue a control transfer.
    ///
    /// Returns the data-phase bytes for an [`In`](Control::In) transfer, and no
    /// bytes for an [`Out`](Control::Out).
    async fn control(&mut self, req: Control<'_>) -> Result<Vec<u8>>;

    /// Write `data` to the bulk OUT endpoint.
    async fn write_bulk(&mut self, data: &[u8]) -> Result<()>;

    /// Read up to `len` bytes from the bulk IN endpoint.
    ///
    /// The device ends a transfer early by sending a short packet. A reply shorter
    /// than `len` is therefore normal, and the caller checks the length it got. A
    /// reply longer than `len` means the device and the host are desynchronized.
    /// An implementation reports it as an error rather than trimming the surplus.
    async fn read_bulk(&mut self, len: usize) -> Result<Vec<u8>>;

    /// Select an alt-setting on an interface, so the next transfer addresses
    /// what that alt-setting names.
    ///
    /// The request is a standard `SET_INTERFACE`, and the default body sends
    /// exactly that request. A transport that owns its wire, such as the native
    /// one, implements nothing here.
    ///
    /// WebUSB overrides it, because a browser does not let a host issue that
    /// request directly. The browser tracks which alt-setting an interface is on,
    /// and offers `selectAlternateInterface()` for changing it. A raw request sent
    /// without the browser's knowledge would leave the browser's record of the
    /// device disagreeing with the device.
    ///
    /// The only caller is the DFU agent, whose alt-settings are its partitions.
    /// DFU addresses a region by selecting its alt-setting, so this call aims each
    /// read and write. It is a [`Transport`] method rather than part of that agent,
    /// because the transport decides which request reaches the device.
    async fn select_alt_setting(&mut self, interface: u16, alt: u8) -> Result<()> {
        self.control(Control::Out {
            request_type: SET_INTERFACE_REQUEST_TYPE,
            request: SET_INTERFACE_REQUEST,
            value: u16::from(alt),
            index: interface,
            data: &[],
        })
        .await?;
        Ok(())
    }
}

/// The `bmRequestType` of a standard `SET_INTERFACE`: host-to-device, standard
/// type, interface recipient.
const SET_INTERFACE_REQUEST_TYPE: u8 = 0x01;

/// The `bRequest` of a standard `SET_INTERFACE`.
const SET_INTERFACE_REQUEST: u8 = 0x0b;

/// A DFU gadget opened for the flash path: the claimed transport, and what only
/// the descriptors carry.
///
/// A DFU [`FlashAgent`](crate::agent::FlashAgent) needs more than a transport,
/// because DFU addresses named alt-settings rather than a device-wide LBA. It has
/// to know three things:
///
/// - The DFU [`interface`](DfuOpen::interface) it drives
/// - The [`functional`](DfuOpen::functional) descriptor's transfer size, which
///   caps a block
/// - The [`alts`](DfuOpen::alts), which are the board's partitions
///
/// Each build's `open_dfu` reads all three and returns them together, so the
/// caller has everything a [`DfuAgent`](crate::agent::DfuAgent) needs.
///
/// The type is generic over the transport, and both builds return it. The two
/// builds reach the same three facts by different routes. Natively, nusb parses
/// the configuration, and every descriptor is available. In a browser tab, the
/// interface and its alt-settings are browser objects. The browser does not expose
/// the functional descriptor, so it is read as bytes with a standard
/// `GET_DESCRIPTOR`. The three facts are the same either way.
pub struct DfuOpen<T> {
    /// The claimed transport, driving DFU control transfers on endpoint 0.
    pub transport: T,
    /// The DFU interface number, carried as `wIndex` on every DFU request.
    pub interface: u16,
    /// The device's DFU functional descriptor: the transfer size that caps a
    /// block, and the capability bits that decide what a write can promise.
    ///
    /// It is carried whole, because [`DfuAgent`](crate::agent::DfuAgent) needs
    /// `bitCanDnload` and `bitManifestTolerant` as well as the transfer size.
    pub functional: crate::codec::dfu::Functional,
    /// The alt-settings the DFU interface exposes, which are the board's partitions.
    pub alts: Vec<crate::codec::dfu_alt::AltSetting>,
}

/// A raw byte stream to a device over a serial line.
///
/// This is the second transport seam, alongside [`Transport`]. It is async for
/// the same reason [`Transport`] is: the browser's Web Serial is async-only. A
/// serial line is not framed. It has no packets and no endpoints, only bytes in
/// order. This seam therefore has two operations: read some bytes, and write all
/// of some bytes. The framing an XMODEM transfer needs is implemented by the
/// [`codec::xmodem`](crate::codec::xmodem) block layout and the
/// [`recovery`](crate::recovery) sender that drives it over this seam.
pub trait Serial {
    /// Read up to `buf.len()` bytes, returning how many arrived within the
    /// transport's deadline.
    ///
    /// A serial read is not framed, so a short read is normal and carries no
    /// meaning of its own. The caller reads again for more. A read that receives
    /// nothing before the deadline returns [`Timeout`](crate::Error::Timeout), not
    /// zero bytes. A wait on a wedged device therefore ends rather than hanging,
    /// and a caller in a read loop does not spin on empty reads.
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize>;

    /// Write all of `data` to the line.
    async fn write_all(&mut self, data: &[u8]) -> Result<()>;
}

/// A borrowed line is a line.
///
/// A driver that owns its line by value, such as [`UBoot`](crate::uboot::UBoot),
/// can then run over a line another driver holds and keeps. The StarFive RAM boot
/// hands its line to the U-Boot driver this way, to stop the autoboot of the U-Boot
/// it has just sent.
impl<S: Serial + ?Sized> Serial for &mut S {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        (**self).read(buf).await
    }

    async fn write_all(&mut self, data: &[u8]) -> Result<()> {
        (**self).write_all(data).await
    }
}

#[cfg(any(test, feature = "testing"))]
pub mod testing {
    //! A transport that replays a scripted conversation, so the backends and verbs
    //! that consume it are tested with no hardware attached.
    //!
    //! It is public behind the `testing` feature, which is off in every shipped
    //! build. The GUI enables it to pin its job lifecycle: a job that runs to
    //! completion, one canceled mid-window, and an agent that becomes
    //! desynchronized. The GUI is another crate, so a `cfg(test)` module is not
    //! visible to it.

    use super::{Control, Serial, Transport};
    use crate::{Error, Result};
    use std::collections::VecDeque;

    /// One turn of a scripted conversation.
    #[derive(Debug, Clone)]
    pub enum Step {
        /// The host is expected to write exactly these bytes.
        ExpectWrite(Vec<u8>),
        /// The host is expected to issue a control-OUT transfer with exactly
        /// this SETUP and data.
        ///
        /// The maskrom download-boot uploads a loader this way. Pinning the SETUP
        /// fields pins the wire format of that upload, as
        /// [`ExpectWrite`](Step::ExpectWrite) does for the bulk path.
        ExpectControlOut {
            /// The `bmRequestType` field.
            request_type: u8,
            /// The `bRequest` field.
            request: u8,
            /// The `wValue` field.
            value: u16,
            /// The `wIndex` field.
            index: u16,
            /// The data the host is expected to send.
            data: Vec<u8>,
        },
        /// The host is expected to issue a control-IN transfer with exactly this
        /// SETUP, and the device answers it with `reply`.
        ///
        /// It is the counterpart of [`ExpectControlOut`](Step::ExpectControlOut)
        /// for the IN direction. It models the reads a backend makes through a
        /// control transfer rather than a bulk pipe. They are the Ingenic boot
        /// ROM's `VR_GET_CPU_INFO`, and DFU's `GETSTATUS` and `UPLOAD`. Pinning the
        /// SETUP fields pins the wire format of each read in the same way, and
        /// `reply` stands in for the bytes the device returns.
        ExpectControlIn {
            /// The `bmRequestType` field.
            request_type: u8,
            /// The `bRequest` field.
            request: u8,
            /// The `wValue` field.
            value: u16,
            /// The `wIndex` field.
            index: u16,
            /// The `wLength` field: how many bytes the host asked to read.
            length: u16,
            /// The bytes the device answers with.
            ///
            /// A control-IN cannot return more than `length` bytes. A script that
            /// offers more is malformed, and the transport panics rather than
            /// model a device that cannot exist.
            reply: Vec<u8>,
        },
        /// The device answers a read with these bytes.
        Reply(Vec<u8>),
        /// The device stalls the next control-OUT, as a BootROM that refuses a
        /// download chunk outright does.
        ///
        /// It models the failure the maskrom download meets on a section the ROM
        /// does not accept. A test can then pin the upload driver's report of
        /// where it stopped, without a board.
        StallControlOut,
        /// The next transfer waits out its deadline and receives nothing, as on a
        /// wedged device.
        ///
        /// It applies to a control transfer or a bulk transfer, in either direction.
        /// It is the counterpart of [`SerialStep::Timeout`](SerialStep::Timeout) for
        /// the USB path. Real transports report this as
        /// [`Error::Timeout`](crate::Error::Timeout), which leaves an agent
        /// desynchronized. A layer that consumes the transport can then be tested
        /// against that state without a board.
        Timeout,
        /// The device fails the next control transfer or bulk read, as a device
        /// that has dropped off the bus does.
        ///
        /// The transport reports it as
        /// [`Error::Disconnected`](crate::Error::Disconnected). A bulk write that
        /// meets this step panics, as it does at any step but
        /// [`ExpectWrite`](Step::ExpectWrite) or [`Timeout`](Step::Timeout).
        Disconnect,
    }

    /// A [`Transport`] that asserts what is written to it and replays scripted
    /// device answers.
    ///
    /// It asserts the exact bytes of every CBW, so a test written against it pins
    /// the wire format as well as the control flow.
    pub struct ScriptedTransport {
        steps: VecDeque<Step>,
    }

    impl ScriptedTransport {
        /// Create a transport that plays out `steps` in order.
        pub fn new(steps: Vec<Step>) -> Self {
            Self {
                steps: steps.into(),
            }
        }

        /// Panics unless every scripted step was used.
        pub fn assert_drained(&self) {
            assert!(
                self.steps.is_empty(),
                "{} scripted steps were never reached: {:?}",
                self.steps.len(),
                self.steps
            );
        }

        fn next_step(&mut self) -> Step {
            self.steps
                .pop_front()
                .expect("the code under test ran more transfers than the script has steps")
        }
    }

    impl Transport for ScriptedTransport {
        async fn control(&mut self, req: Control<'_>) -> Result<Vec<u8>> {
            match req {
                Control::Out {
                    request_type,
                    request,
                    value,
                    index,
                    data,
                } => match self.next_step() {
                    Step::ExpectControlOut {
                        request_type: er,
                        request: erq,
                        value: ev,
                        index: ei,
                        data: ed,
                    } => {
                        assert_eq!(
                            (request_type, request, value, index),
                            (er, erq, ev, ei),
                            "the host issued a control-OUT with a SETUP the script did not expect"
                        );
                        assert_eq!(
                            data,
                            &ed[..],
                            "the host sent control-OUT bytes the script did not expect"
                        );
                        Ok(Vec::new())
                    }
                    // A stall the same shape the real transports raise it:
                    // `Protocol`, worded exactly as `usb.rs` and `webusb.rs`
                    // word a stalled control OUT, so a test sees what hardware
                    // would send.
                    Step::StallControlOut => Err(Error::Protocol(
                        "the device stalled control OUT: it refused the command outright"
                            .to_string(),
                    )),
                    // A device that leaves the bus as it acknowledges the
                    // command, which is what an Ingenic board does on the
                    // `VR_PROGRAM_START2` jump into U-Boot -- the same shape the
                    // rockusb reset raises, so the bootstrap driver's tolerance
                    // of it can be pinned without a board.
                    Step::Disconnect => Err(Error::Disconnected),
                    // A control OUT that waits out its deadline with no answer,
                    // the counterpart of the bulk-path timeout for the DFU write,
                    // whose blocks go out as control transfers.
                    Step::Timeout => Err(Error::Timeout {
                        what: "control OUT",
                        waited: std::time::Duration::from_secs(10),
                    }),
                    other => {
                        panic!("the host issued a control-OUT when the script expected {other:?}")
                    }
                },
                Control::In {
                    request_type,
                    request,
                    value,
                    index,
                    length,
                } => match self.next_step() {
                    Step::ExpectControlIn {
                        request_type: er,
                        request: erq,
                        value: ev,
                        index: ei,
                        length: el,
                        reply,
                    } => {
                        assert_eq!(
                            (request_type, request, value, index, length),
                            (er, erq, ev, ei, el),
                            "the host issued a control-IN with a SETUP the script did not expect"
                        );
                        assert!(
                            reply.len() <= length as usize,
                            "a control-IN reply cannot exceed the {length} bytes it asked for; the \
                             script offered {}",
                            reply.len()
                        );
                        Ok(reply)
                    }
                    // A control IN that waits out its deadline, or a device that
                    // drops off the bus mid-read: the DFU status poll reads through
                    // control IN, so both are how that poll can tear -- and both
                    // leave a DFU agent desynchronized, which is what lets that be
                    // pinned without a board.
                    Step::Timeout => Err(Error::Timeout {
                        what: "control IN",
                        waited: std::time::Duration::from_secs(10),
                    }),
                    Step::Disconnect => Err(Error::Disconnected),
                    other => {
                        panic!("the host issued a control-IN when the script expected {other:?}")
                    }
                },
            }
        }

        async fn write_bulk(&mut self, data: &[u8]) -> Result<()> {
            match self.next_step() {
                Step::ExpectWrite(expected) => {
                    assert_eq!(
                        data, expected,
                        "the host wrote bytes the script did not expect"
                    );
                    Ok(())
                }
                Step::Timeout => Err(Error::Timeout {
                    what: "bulk OUT",
                    waited: std::time::Duration::from_secs(10),
                }),
                other => panic!("the host wrote when the script expected {other:?}"),
            }
        }

        async fn read_bulk(&mut self, len: usize) -> Result<Vec<u8>> {
            match self.next_step() {
                Step::Reply(bytes) => {
                    // The real transport reports a device that answers with more
                    // than was asked for rather than trimming the surplus away,
                    // so the mock holds itself to the same contract. A script that
                    // over-answers is modeling a desynchronized device, and the
                    // code under test must see it as one.
                    if bytes.len() > len {
                        return Err(Error::Protocol(format!(
                            "bulk IN answered a {len}-byte request with {} bytes. The device and \
                             the host are no longer synchronized",
                            bytes.len()
                        )));
                    }
                    Ok(bytes)
                }
                Step::Timeout => Err(Error::Timeout {
                    what: "bulk IN",
                    waited: std::time::Duration::from_secs(10),
                }),
                Step::Disconnect => Err(Error::Disconnected),
                other => panic!("the host read when the script expected {other:?}"),
            }
        }
    }

    /// One turn of a scripted serial conversation.
    ///
    /// It is the serial counterpart of [`Step`]. A serial line has no framing, so
    /// the device's turns are bytes made available to read, not framed replies. An
    /// [`Rx`](SerialStep::Rx) is drained across as many reads as the code under
    /// test makes. This models the `C` storm (many bytes read a bufferful at a
    /// time) and a short read (fewer bytes than were asked for).
    #[derive(Debug, Clone)]
    pub enum SerialStep {
        /// Bytes the device puts on the line, such as the `C` storm, a `NAK`
        /// burst, or the recovery agent's menu text.
        ///
        /// They are consumed across reads. A `read` takes up to the buffer's
        /// length and leaves the rest for the next one.
        Rx(Vec<u8>),
        /// The host is expected to write exactly these bytes in one `write_all`.
        ///
        /// The step asserts the exact bytes, so a test pins the wire format, as
        /// [`ExpectWrite`](Step::ExpectWrite) pins a CBW. Here the format is a
        /// whole XMODEM block, header and CRC included.
        ExpectTx(Vec<u8>),
        /// The next read waits out its deadline and receives nothing.
        ///
        /// This models a board that is strapped but not yet streaming, with the
        /// line quiet for a moment before the `C` storm begins. A sender must wait
        /// through that, rather than treat it as a failure.
        Timeout,
        /// The device drops off the line, and the next read fails with
        /// [`Error::Disconnected`](crate::Error::Disconnected).
        Disconnect,
    }

    /// A [`Serial`] that asserts what is written to it and replays scripted bytes
    /// for the host to read.
    ///
    /// It is the serial counterpart of [`ScriptedTransport`]. It pins the whole
    /// StarFive recovery sequence against a modeled ROM with no board, as
    /// [`ScriptedTransport`] models a rockusb device. The sequence covers the `C`
    /// handshake and per-block `ACK`, a scripted `NAK` burst, and the agent's menu
    /// prompts.
    pub struct ScriptedSerial {
        steps: VecDeque<SerialStep>,
    }

    impl ScriptedSerial {
        /// Create a serial that plays out `steps` in order.
        pub fn new(steps: Vec<SerialStep>) -> Self {
            Self {
                steps: steps.into(),
            }
        }

        /// Panics unless every scripted step was used.
        ///
        /// A leftover [`Rx`](SerialStep::Rx) with bytes still in it counts as
        /// unreached: the code under test stopped reading before it consumed what
        /// the device offered.
        pub fn assert_drained(&self) {
            assert!(
                self.steps.is_empty(),
                "{} scripted serial steps were never reached: {:?}",
                self.steps.len(),
                self.steps
            );
        }
    }

    impl Serial for ScriptedSerial {
        async fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
            match self.steps.front_mut() {
                Some(SerialStep::Rx(bytes)) => {
                    // A serial read takes up to the buffer's length. Draining the
                    // Rx across reads is what models the C storm and a short read:
                    // the front step shrinks until it is empty, then it is popped.
                    let n = buf.len().min(bytes.len());
                    buf[..n].copy_from_slice(&bytes[..n]);
                    bytes.drain(..n);
                    if bytes.is_empty() {
                        self.steps.pop_front();
                    }
                    Ok(n)
                }
                Some(SerialStep::Timeout) => {
                    self.steps.pop_front();
                    Err(Error::Timeout {
                        what: "serial read",
                        waited: std::time::Duration::from_secs(1),
                    })
                }
                Some(SerialStep::Disconnect) => {
                    self.steps.pop_front();
                    Err(Error::Disconnected)
                }
                Some(other) => {
                    panic!("the host read when the script expected {other:?}")
                }
                None => panic!("the code under test read past the end of the serial script"),
            }
        }

        async fn write_all(&mut self, data: &[u8]) -> Result<()> {
            match self.steps.pop_front() {
                Some(SerialStep::ExpectTx(expected)) => {
                    assert_eq!(
                        data,
                        &expected[..],
                        "the host wrote serial bytes the script did not expect"
                    );
                    Ok(())
                }
                Some(other) => panic!("the host wrote when the script expected {other:?}"),
                None => panic!("the code under test wrote past the end of the serial script"),
            }
        }
    }
}
