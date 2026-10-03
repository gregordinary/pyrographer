//! The USB Device Firmware Upgrade (DFU) 1.1 class protocol.
//!
//! An Ingenic board speaks DFU once a DFU-capable U-Boot runs on it. pyrographer
//! prefers that post-bootstrap flash path over the vendor "cloner" protocol. The
//! U-Boot that answers DFU handles DRAM init and exposes flash as named
//! alt-settings, so the host does not drive raw reads and writes.
//!
//! Unlike rockusb, DFU uses control transfers to an interface, not a bulk pipe.
//! Every request is a SETUP packet whose `bmRequestType` names the DFU interface
//! as recipient. There are seven:
//!
//! | `bRequest` | Name | Direction | Data phase |
//! | ---------- | ---- | --------- | ---------- |
//! | `0` | `DFU_DETACH` | host->device | none |
//! | `1` | `DFU_DNLOAD` | host->device | a firmware block (write) |
//! | `2` | `DFU_UPLOAD` | device->host | a firmware block (read) |
//! | `3` | `DFU_GETSTATUS` | device->host | 6-byte status |
//! | `4` | `DFU_CLRSTATUS` | host->device | none |
//! | `5` | `DFU_GETSTATE` | device->host | 1-byte state |
//! | `6` | `DFU_ABORT` | host->device | none |
//!
//! For `DNLOAD` and `UPLOAD`, the SETUP's `wValue` is the *block number*, a
//! sequence counter the host increments per block, and `wIndex` is the interface.
//! `wTransferSize`, from the interface's [`Functional`] descriptor, caps the block
//! size.
//!
//! The device paces the write loop, not the host. After each `DNLOAD`, the host
//! issues `GETSTATUS`. The reply carries a `bwPollTimeout` the host must wait out
//! before the next request. A device that erases slowly uses it to keep the host
//! from running ahead.
//!
//! This module parses that reply, and classifies the [`State`] and [`Status`] it
//! carries. The agent runs the loop that walks the states: send, poll, wait,
//! repeat. [`rockusb`](super::rockusb) divides the work the same way: it builds and
//! parses commands, and the agent sequences them.
//!
//! The module is sans-I/O framing and touches no transport. The request builders
//! return a [`Setup`], the SETUP field values alone, and the agent turns that into
//! a [`Control`](crate::transport::Control). The tests therefore assert exact bytes
//! with no transport.
//!
//! Every request number, code and layout here comes from the USB DFU 1.1
//! specification, a USB-IF standard, and is **\[DOC\]**. What an Ingenic board
//! answers is **\[UNVERIFIED\]** until a board is on the bench. That covers which
//! alt-settings it exposes, and which `wTransferSize` it advertises.

use crate::{Error, Result};

/// The `bmRequestType` for a host-to-device DFU request: class type, interface
/// recipient, host-to-device direction.
pub const REQUEST_TYPE_OUT: u8 = 0x21;

/// The `bmRequestType` for a device-to-host DFU request: class type, interface
/// recipient, device-to-host direction.
pub const REQUEST_TYPE_IN: u8 = 0xA1;

/// The direction bit of `bmRequestType`: set for a device-to-host transfer.
///
/// The agent reads this bit from a [`Setup`] to choose between a control-IN and a
/// control-OUT. The direction is therefore stored once, in the request type byte
/// the DFU specification fixes, and not tracked alongside it.
const DIR_IN: u8 = 0x80;

/// A DFU class request (`bRequest`).
///
/// **\[DOC\]** USB DFU 1.1, Table 3.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Request {
    /// `DFU_DETACH`: ask a run-time (application-mode) device to drop into DFU
    /// mode.
    Detach = 0,
    /// `DFU_DNLOAD`: send one firmware block to the device (a write).
    Download = 1,
    /// `DFU_UPLOAD`: read one firmware block back from the device (a read).
    Upload = 2,
    /// `DFU_GETSTATUS`: read the 6-byte status, and with it the poll timeout the
    /// host must wait before its next request.
    GetStatus = 3,
    /// `DFU_CLRSTATUS`: clear an error and return the device to `dfuIDLE`.
    ClearStatus = 4,
    /// `DFU_GETSTATE`: read the 1-byte state without disturbing it.
    GetState = 5,
    /// `DFU_ABORT`: abandon the current transfer and return to `dfuIDLE`.
    Abort = 6,
}

impl Request {
    /// The request's name, for a message a person reads.
    pub fn name(self) -> &'static str {
        match self {
            Request::Detach => "DFU_DETACH",
            Request::Download => "DFU_DNLOAD",
            Request::Upload => "DFU_UPLOAD",
            Request::GetStatus => "DFU_GETSTATUS",
            Request::ClearStatus => "DFU_CLRSTATUS",
            Request::GetState => "DFU_GETSTATE",
            Request::Abort => "DFU_ABORT",
        }
    }
}

/// The length of a `DFU_GETSTATUS` reply, in bytes.
pub const GET_STATUS_LEN: usize = 6;

/// The length of a `DFU_GETSTATE` reply, in bytes.
pub const GET_STATE_LEN: usize = 1;

/// A device's status code (`bStatus`) in a `DFU_GETSTATUS` reply.
///
/// [`Ok`](Status::Ok) is the only non-error value. Every other code names a way the
/// last request failed. A write loop stops on any of them, and sends no further
/// block to a device that has faulted.
///
/// **\[DOC\]** USB DFU 1.1, Table 6.1. The enum carries the full defined set, so a
/// device's own reason reaches the person verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Status {
    /// `OK`: no error.
    Ok = 0x00,
    /// `errTARGET`: the file is not targeted for use by this device.
    Target = 0x01,
    /// `errFILE`: the file fails a vendor verification test.
    File = 0x02,
    /// `errWRITE`: the device failed to write memory.
    Write = 0x03,
    /// `errERASE`: a memory erase failed.
    Erase = 0x04,
    /// `errCHECK_ERASED`: a memory erase check failed.
    CheckErased = 0x05,
    /// `errPROG`: the device failed to program memory.
    Prog = 0x06,
    /// `errVERIFY`: the programmed memory failed verification.
    Verify = 0x07,
    /// `errADDRESS`: an address was out of range.
    Address = 0x08,
    /// `errNOTDONE`: a `DNLOAD` of zero length was received but the device does
    /// not think it has all the data yet.
    NotDone = 0x09,
    /// `errFIRMWARE`: the device's firmware is corrupt and cannot return to
    /// run-time operation.
    Firmware = 0x0a,
    /// `errVENDOR`: a vendor-specific error.
    Vendor = 0x0b,
    /// `errUSBR`: an unexpected USB reset.
    UsbReset = 0x0c,
    /// `errPOR`: an unexpected power-on reset.
    PowerOnReset = 0x0d,
    /// `errUNKNOWN`: something went wrong, but the device does not know what.
    Unknown = 0x0e,
    /// `errSTALLEDPKT`: the device stalled an unexpected request.
    StalledPacket = 0x0f,
}

impl Status {
    /// Map the raw `bStatus` byte to a code, or `None` for a value the DFU 1.1
    /// specification does not define.
    ///
    /// An undefined status from a device is out of spec, and pyrographer does not
    /// guess its meaning. [`GetStatus::parse`] turns a `None` here into a
    /// [`Protocol`](Error::Protocol) error and does not proceed.
    pub fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            0x00 => Status::Ok,
            0x01 => Status::Target,
            0x02 => Status::File,
            0x03 => Status::Write,
            0x04 => Status::Erase,
            0x05 => Status::CheckErased,
            0x06 => Status::Prog,
            0x07 => Status::Verify,
            0x08 => Status::Address,
            0x09 => Status::NotDone,
            0x0a => Status::Firmware,
            0x0b => Status::Vendor,
            0x0c => Status::UsbReset,
            0x0d => Status::PowerOnReset,
            0x0e => Status::Unknown,
            0x0f => Status::StalledPacket,
            _ => return None,
        })
    }

    /// Whether this status reports an error, meaning anything other than
    /// [`Ok`](Status::Ok).
    pub fn is_error(self) -> bool {
        self != Status::Ok
    }

    /// The status's name, for a message a person reads. These are the DFU
    /// specification's own `errXXX` identifiers.
    pub fn name(self) -> &'static str {
        match self {
            Status::Ok => "OK",
            Status::Target => "errTARGET",
            Status::File => "errFILE",
            Status::Write => "errWRITE",
            Status::Erase => "errERASE",
            Status::CheckErased => "errCHECK_ERASED",
            Status::Prog => "errPROG",
            Status::Verify => "errVERIFY",
            Status::Address => "errADDRESS",
            Status::NotDone => "errNOTDONE",
            Status::Firmware => "errFIRMWARE",
            Status::Vendor => "errVENDOR",
            Status::UsbReset => "errUSBR",
            Status::PowerOnReset => "errPOR",
            Status::Unknown => "errUNKNOWN",
            Status::StalledPacket => "errSTALLEDPKT",
        }
    }
}

/// A device's state (`bState`) in a `DFU_GETSTATUS` or `DFU_GETSTATE` reply.
///
/// The states split into a run-time (`app*`) group, which a device is in before it
/// enters DFU mode, and a DFU-mode group. pyrographer finds an Ingenic board
/// already in DFU mode, with its U-Boot enumerated as a DFU gadget. The states the
/// write loop handles are therefore [`DfuIdle`](State::DfuIdle) through
/// [`DfuError`](State::DfuError).
///
/// **\[DOC\]** USB DFU 1.1, Table A.2.4 (the state-transition diagram).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum State {
    /// `appIDLE`: run-time, idle. The device is running its application and has
    /// not been asked to detach.
    AppIdle = 0,
    /// `appDETACH`: run-time, detaching. `DFU_DETACH` was received, and the device
    /// is waiting for a USB reset to enter DFU mode.
    AppDetach = 1,
    /// `dfuIDLE`: DFU mode, idle and ready for a transfer.
    DfuIdle = 2,
    /// `dfuDNLOAD-SYNC`: a `DNLOAD` block was received, and the device is waiting
    /// for the host's `GETSTATUS`.
    DownloadSync = 3,
    /// `dfuDNBUSY`: the device is writing the last block and is busy. The host
    /// must wait out the poll timeout before its next request.
    DownloadBusy = 4,
    /// `dfuDNLOAD-IDLE`: a block has been written and the device is ready for the
    /// next `DNLOAD`.
    DownloadIdle = 5,
    /// `dfuMANIFEST-SYNC`: the last block was sent (a zero-length `DNLOAD`), and
    /// the device is waiting for `GETSTATUS` before manifesting.
    ManifestSync = 6,
    /// `dfuMANIFEST`: the device is committing the firmware.
    Manifest = 7,
    /// `dfuMANIFEST-WAIT-RESET`: manifestation is complete and the device is
    /// waiting for a USB reset.
    ManifestWaitReset = 8,
    /// `dfuUPLOAD-IDLE`: an `UPLOAD` block was sent and the device is ready for
    /// the next.
    UploadIdle = 9,
    /// `dfuERROR`: an error occurred, and the device stays in this state until
    /// `CLRSTATUS`.
    DfuError = 10,
}

impl State {
    /// Map the raw `bState` byte to a state, or `None` for a value the DFU 1.1
    /// specification does not define.
    ///
    /// As with [`Status::from_u8`], an undefined state is out of spec and is not
    /// interpreted. The parse turns it into a [`Protocol`](Error::Protocol) error.
    pub fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            0 => State::AppIdle,
            1 => State::AppDetach,
            2 => State::DfuIdle,
            3 => State::DownloadSync,
            4 => State::DownloadBusy,
            5 => State::DownloadIdle,
            6 => State::ManifestSync,
            7 => State::Manifest,
            8 => State::ManifestWaitReset,
            9 => State::UploadIdle,
            10 => State::DfuError,
            _ => return None,
        })
    }

    /// Whether the device is busy and the host must wait out the poll timeout
    /// before its next request. True only in [`DownloadBusy`](State::DownloadBusy).
    pub fn is_busy(self) -> bool {
        self == State::DownloadBusy
    }

    /// Whether the device is in its error state, [`DfuError`](State::DfuError),
    /// and will refuse transfers until a `CLRSTATUS`.
    pub fn is_error(self) -> bool {
        self == State::DfuError
    }

    /// The state's name, in the DFU specification's own `dfuXXX`/`appXXX`
    /// identifiers.
    pub fn name(self) -> &'static str {
        match self {
            State::AppIdle => "appIDLE",
            State::AppDetach => "appDETACH",
            State::DfuIdle => "dfuIDLE",
            State::DownloadSync => "dfuDNLOAD-SYNC",
            State::DownloadBusy => "dfuDNBUSY",
            State::DownloadIdle => "dfuDNLOAD-IDLE",
            State::ManifestSync => "dfuMANIFEST-SYNC",
            State::Manifest => "dfuMANIFEST",
            State::ManifestWaitReset => "dfuMANIFEST-WAIT-RESET",
            State::UploadIdle => "dfuUPLOAD-IDLE",
            State::DfuError => "dfuERROR",
        }
    }
}

/// A parsed `DFU_GETSTATUS` reply.
///
/// It reports the outcome of the last request and the state it left the device in.
/// It also carries the `bwPollTimeout` the host must wait before its next request.
/// The write loop reads all three. It stops on an error
/// [`status`](GetStatus::status). It waits
/// [`poll_timeout_ms`](GetStatus::poll_timeout_ms) while the
/// [`state`](GetStatus::state) is busy, and sends the next block otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GetStatus {
    /// `bStatus`: the outcome of the last request.
    pub status: Status,
    /// `bwPollTimeout`: the minimum time in milliseconds the host must wait
    /// before its next request. It is three bytes on the wire, so its maximum is
    /// `0xFFFFFF` ms.
    pub poll_timeout_ms: u32,
    /// `bState`: the state the device is now in.
    pub state: State,
    /// `iString`: the index of a string descriptor describing the status, or
    /// zero for none. pyrographer does not fetch it, but carries it so the reply
    /// round-trips.
    pub string_index: u8,
}

impl GetStatus {
    /// Parse a 6-byte `DFU_GETSTATUS` reply.
    ///
    /// The reply must be exactly [`GET_STATUS_LEN`] bytes, and its status and state
    /// bytes must both be values the DFU specification defines. A reply of the
    /// wrong length, or one carrying a reserved status or state, comes from a device
    /// out of spec, and returns [`Protocol`](Error::Protocol). The fault is in the
    /// wire, not a caller's file.
    pub fn parse(reply: &[u8]) -> Result<Self> {
        if reply.len() != GET_STATUS_LEN {
            return Err(Error::Protocol(format!(
                "a DFU GETSTATUS reply is {GET_STATUS_LEN} bytes, but the device answered with {}",
                reply.len()
            )));
        }
        let status = Status::from_u8(reply[0]).ok_or_else(|| {
            Error::Protocol(format!(
                "the device reported DFU status {:#04x}, which the DFU 1.1 spec does not define",
                reply[0]
            ))
        })?;
        let poll_timeout_ms =
            u32::from(reply[1]) | (u32::from(reply[2]) << 8) | (u32::from(reply[3]) << 16);
        let state = State::from_u8(reply[4]).ok_or_else(|| {
            Error::Protocol(format!(
                "the device reported DFU state {:#04x}, which the DFU 1.1 spec does not define",
                reply[4]
            ))
        })?;
        Ok(GetStatus {
            status,
            poll_timeout_ms,
            state,
            string_index: reply[5],
        })
    }

    /// Serialize back to the 6 wire bytes.
    ///
    /// It is the inverse of [`parse`](GetStatus::parse). A scripted device can
    /// therefore be given a reply built from a [`GetStatus`], and the tests pin the
    /// two against each other. The poll timeout is three bytes on the wire. A value
    /// above `0xFFFFFF` cannot be represented, and is truncated to its low 24 bits.
    /// The tests stay below that value.
    pub fn to_bytes(self) -> [u8; GET_STATUS_LEN] {
        let poll = self.poll_timeout_ms.to_le_bytes();
        [
            self.status as u8,
            poll[0],
            poll[1],
            poll[2],
            self.state as u8,
            self.string_index,
        ]
    }
}

/// The DFU functional descriptor, `bDescriptorType` `0x21`.
pub const FUNCTIONAL_DESCRIPTOR_TYPE: u8 = 0x21;

/// The length of the DFU functional descriptor, in bytes.
pub const FUNCTIONAL_DESCRIPTOR_LEN: usize = 9;

// The `bmAttributes` bits of the functional descriptor.
const ATTR_CAN_DOWNLOAD: u8 = 1 << 0;
const ATTR_CAN_UPLOAD: u8 = 1 << 1;
const ATTR_MANIFESTATION_TOLERANT: u8 = 1 << 2;
const ATTR_WILL_DETACH: u8 = 1 << 3;

/// A parsed DFU functional descriptor.
///
/// It travels in the device's configuration descriptor, after the DFU interface.
/// It tells the host how to drive the device, including whether it can be read
/// ([`can_upload`](Functional::can_upload)) and written
/// ([`can_download`](Functional::can_download)) at all. It also carries the
/// [`transfer_size`](Functional::transfer_size) that caps every `DNLOAD` and
/// `UPLOAD` block. A write loop that sent larger blocks would have them rejected,
/// so the agent reads the cap from here and never assumes one.
///
/// **\[DOC\]** USB DFU 1.1, Table 4.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Functional {
    /// `bmAttributes`: the capability bits. Read them through the accessors
    /// rather than by hand.
    pub attributes: u8,
    /// `wDetachTimeOut`: the time in milliseconds the device waits for a USB reset
    /// after `DFU_DETACH` before giving up.
    pub detach_timeout_ms: u16,
    /// `wTransferSize`: the maximum number of bytes the device accepts in one
    /// `DNLOAD`, and returns in one `UPLOAD`. It caps the write and read block
    /// size.
    pub transfer_size: u16,
    /// `bcdDFUVersion`: the DFU specification revision in BCD. It is `0x0110` for
    /// DFU 1.1, `0x0100` for 1.0, and `0x011a` for the STMicro DfuSe extension.
    pub dfu_version: u16,
}

impl Functional {
    /// Parse the 9-byte functional descriptor.
    ///
    /// The input must be at least [`FUNCTIONAL_DESCRIPTOR_LEN`] bytes, and its
    /// `bDescriptorType`, the second byte, must be [`FUNCTIONAL_DESCRIPTOR_TYPE`].
    /// Anything else is not this descriptor, and returns
    /// [`Protocol`](Error::Protocol), because the descriptor came from the wire. A
    /// descriptor longer than nine bytes is read as its first nine. The `bLength` in
    /// the first byte is the device's own claim, and the fixed fields are all within
    /// the first nine.
    ///
    /// A `wTransferSize` of zero is refused here too. Every layer that consumes the
    /// descriptor measures in this block size, and the agent reports it as its
    /// sector size. A zero would reach the verbs as a division by zero, instead of
    /// as a malformed descriptor.
    pub fn parse(descriptor: &[u8]) -> Result<Self> {
        if descriptor.len() < FUNCTIONAL_DESCRIPTOR_LEN {
            return Err(Error::Protocol(format!(
                "a DFU functional descriptor is {FUNCTIONAL_DESCRIPTOR_LEN} bytes, but this one \
                 is {}",
                descriptor.len()
            )));
        }
        if descriptor[1] != FUNCTIONAL_DESCRIPTOR_TYPE {
            return Err(Error::Protocol(format!(
                "expected a DFU functional descriptor (type {FUNCTIONAL_DESCRIPTOR_TYPE:#04x}), \
                 but got descriptor type {:#04x}",
                descriptor[1]
            )));
        }
        let transfer_size = u16::from_le_bytes([descriptor[5], descriptor[6]]);
        // The transfer size is what the agent above reports as its sector size, so
        // a zero here becomes a division by zero several layers up. Refused where
        // it is a fact about a descriptor rather than a fact about one call site:
        // every construction of a `Functional` comes through here, so no caller
        // can hold one that names a size no block can be measured in.
        if transfer_size == 0 {
            return Err(Error::Protocol(
                "the DFU functional descriptor advertises a transfer size of zero, which is not \
                 a usable block size"
                    .to_string(),
            ));
        }

        Ok(Functional {
            attributes: descriptor[2],
            detach_timeout_ms: u16::from_le_bytes([descriptor[3], descriptor[4]]),
            transfer_size,
            dfu_version: u16::from_le_bytes([descriptor[7], descriptor[8]]),
        })
    }

    /// `bitCanDnload`: whether the device accepts `DFU_DNLOAD`, and so whether it
    /// can be written at all.
    pub fn can_download(self) -> bool {
        self.attributes & ATTR_CAN_DOWNLOAD != 0
    }

    /// `bitCanUpload`: whether the device serves `DFU_UPLOAD`, and so whether it can
    /// be read back. This bit makes an Ingenic DFU board verifiable, unlike the
    /// StarFive serial path.
    pub fn can_upload(self) -> bool {
        self.attributes & ATTR_CAN_UPLOAD != 0
    }

    /// `bitManifestTolerant`: whether the device stays on the bus after
    /// manifesting. If it does, the host can issue `GETSTATUS` once more instead of
    /// losing the device to a reset.
    pub fn manifestation_tolerant(self) -> bool {
        self.attributes & ATTR_MANIFESTATION_TOLERANT != 0
    }

    /// `bitWillDetach`: whether the device detaches itself after `DFU_DETACH`
    /// rather than waiting for the host to issue a USB reset.
    pub fn will_detach(self) -> bool {
        self.attributes & ATTR_WILL_DETACH != 0
    }
}

/// The standard interface descriptor, `bDescriptorType` `0x04`.
///
/// [`functional_in_configuration`] matches on this type to find, in a raw
/// configuration descriptor, the interface record a functional descriptor follows.
pub const INTERFACE_DESCRIPTOR_TYPE: u8 = 0x04;

/// Find the DFU functional descriptor an interface carries, in a raw
/// configuration descriptor.
///
/// WebUSB does not expose class-specific descriptors, and this function fills that
/// gap. Natively, nusb parses the configuration, and the functional descriptor is
/// available among the DFU interface's own. A browser exposes interfaces,
/// alt-settings and endpoints as objects, and no property on `USBDevice` carries
/// `wTransferSize`. The web transport therefore fetches the configuration
/// descriptor as bytes with a standard `GET_DESCRIPTOR`, and walks it here. The
/// walk is sans-I/O, accepts bytes from either transport, and is unit-tested with
/// constructed ones.
///
/// A configuration descriptor is a chain of `[bLength, bDescriptorType, ...]`
/// records. Class-specific descriptors belong to the interface record that most
/// recently preceded them. The walk therefore tracks the current interface number,
/// and returns the first [`FUNCTIONAL_DESCRIPTOR_TYPE`] record found for
/// `interface_number`. A functional descriptor placed before its interface
/// descriptor, or missing entirely, returns [`Protocol`](Error::Protocol) with that
/// reason. The transfer size is never guessed, and the native path refuses the
/// same way.
///
/// A record whose `bLength` is under two, or longer than the bytes that remain,
/// ends the walk. The walk does not try to re-synchronize inside a malformed chain.
/// Stopping reports the descriptor as absent.
pub fn functional_in_configuration(config: &[u8], interface_number: u8) -> Result<Functional> {
    let mut at = 0usize;
    let mut current: Option<u8> = None;

    while at + 2 <= config.len() {
        let length = usize::from(config[at]);
        let kind = config[at + 1];
        if length < 2 || at + length > config.len() {
            break;
        }
        let record = &config[at..at + length];

        match kind {
            // bInterfaceNumber is the third byte of an interface descriptor.
            INTERFACE_DESCRIPTOR_TYPE if length >= 3 => current = Some(record[2]),
            FUNCTIONAL_DESCRIPTOR_TYPE if current == Some(interface_number) => {
                return Functional::parse(record);
            }
            _ => {}
        }
        at += length;
    }

    Err(Error::Protocol(format!(
        "interface {interface_number} carries no DFU functional descriptor (type \
         {FUNCTIONAL_DESCRIPTOR_TYPE:#04x}) in the configuration descriptor, so its transfer size \
         is unknown"
    )))
}

/// The SETUP fields of a DFU control request.
///
/// It is all of a DFU request that crosses the wire, except the data phase. The
/// codec builds this, and the agent turns it into a
/// [`Control`](crate::transport::Control), so the codec never names a transport.
/// [`request_type`](Setup::request_type) carries the direction, and
/// [`is_in`](Setup::is_in) reads it back. The agent chooses a control-IN or
/// control-OUT from that one bit, and tracks no separate direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Setup {
    /// `bmRequestType`: [`REQUEST_TYPE_IN`] or [`REQUEST_TYPE_OUT`].
    pub request_type: u8,
    /// `bRequest`: the [`Request`] code.
    pub request: u8,
    /// `wValue`: a block number for `DNLOAD`/`UPLOAD`, the detach timeout for
    /// `DETACH`, zero otherwise.
    pub value: u16,
    /// `wIndex`: the DFU interface number.
    pub index: u16,
    /// `wLength`: for a device-to-host request, how many bytes to read. For a
    /// host-to-device one, how many bytes of data the agent sends.
    pub length: u16,
}

impl Setup {
    /// Whether this request's data phase is device-to-host (a control-IN).
    ///
    /// It is read from the direction bit of `bmRequestType`, which the DFU
    /// specification fixes per request. It therefore cannot disagree with the
    /// request itself.
    pub fn is_in(self) -> bool {
        self.request_type & DIR_IN != 0
    }
}

/// Build the SETUP for `DFU_DETACH`: ask a run-time device to enter DFU mode,
/// giving it `timeout_ms` to see the USB reset that follows.
pub fn detach(interface: u16, timeout_ms: u16) -> Setup {
    Setup {
        request_type: REQUEST_TYPE_OUT,
        request: Request::Detach as u8,
        value: timeout_ms,
        index: interface,
        length: 0,
    }
}

/// Build the SETUP for a `DFU_DNLOAD` of `data_len` bytes as block `block_num`.
///
/// The agent sends `data_len` bytes of data with it. A `data_len` of zero is the
/// end-of-download signal that moves the device toward manifestation. `data_len`
/// must not exceed the device's [`Functional::transfer_size`], which the agent
/// enforces.
pub fn download(interface: u16, block_num: u16, data_len: u16) -> Setup {
    Setup {
        request_type: REQUEST_TYPE_OUT,
        request: Request::Download as u8,
        value: block_num,
        index: interface,
        length: data_len,
    }
}

/// Build the SETUP for a `DFU_UPLOAD` of up to `len` bytes as block `block_num`.
///
/// The device returns up to `len` bytes. A short reply (fewer than `len` bytes)
/// marks the end of the flash region, and tells a read loop it is done.
pub fn upload(interface: u16, block_num: u16, len: u16) -> Setup {
    Setup {
        request_type: REQUEST_TYPE_IN,
        request: Request::Upload as u8,
        value: block_num,
        index: interface,
        length: len,
    }
}

/// Build the SETUP for `DFU_GETSTATUS`: read the 6-byte status.
pub fn get_status(interface: u16) -> Setup {
    Setup {
        request_type: REQUEST_TYPE_IN,
        request: Request::GetStatus as u8,
        value: 0,
        index: interface,
        length: GET_STATUS_LEN as u16,
    }
}

/// Build the SETUP for `DFU_CLRSTATUS`: clear an error and return to `dfuIDLE`.
pub fn clear_status(interface: u16) -> Setup {
    Setup {
        request_type: REQUEST_TYPE_OUT,
        request: Request::ClearStatus as u8,
        value: 0,
        index: interface,
        length: 0,
    }
}

/// Build the SETUP for `DFU_GETSTATE`: read the 1-byte state.
pub fn get_state(interface: u16) -> Setup {
    Setup {
        request_type: REQUEST_TYPE_IN,
        request: Request::GetState as u8,
        value: 0,
        index: interface,
        length: GET_STATE_LEN as u16,
    }
}

/// Build the SETUP for `DFU_ABORT`: abandon the current transfer and return to
/// `dfuIDLE`.
pub fn abort(interface: u16) -> Setup {
    Setup {
        request_type: REQUEST_TYPE_OUT,
        request: Request::Abort as u8,
        value: 0,
        index: interface,
        length: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The IN requests carry the device-to-host direction bit and the OUT ones do
    /// not, and [`Setup::is_in`] reads exactly that bit. The agent picks a
    /// control-IN or control-OUT from that bit. A request whose type disagreed with
    /// its direction would send the data phase the wrong way.
    #[test]
    fn the_in_requests_are_the_ones_that_read() {
        assert!(get_status(0).is_in());
        assert!(get_state(0).is_in());
        assert!(upload(0, 0, 64).is_in());

        assert!(!detach(0, 0).is_in());
        assert!(!download(0, 0, 64).is_in());
        assert!(!clear_status(0).is_in());
        assert!(!abort(0).is_in());
    }

    /// A download's SETUP pins the wire format: the request byte, the block number
    /// in `wValue`, the interface in `wIndex`, and the data length in `wLength`.
    /// These are the fields a device matches a write against.
    #[test]
    fn a_download_setup_carries_the_block_number_and_interface() {
        let setup = download(3, 7, 4096);
        assert_eq!(setup.request_type, REQUEST_TYPE_OUT);
        assert_eq!(setup.request, 1); // DFU_DNLOAD
        assert_eq!(setup.value, 7); // block number
        assert_eq!(setup.index, 3); // interface
        assert_eq!(setup.length, 4096);
    }

    /// GETSTATUS reads six bytes device-to-host, and GETSTATE reads one. The agent
    /// asks the transport for these lengths, so they are pinned here.
    #[test]
    fn the_status_requests_ask_for_the_right_lengths() {
        assert_eq!(get_status(1).length, 6);
        assert_eq!(get_status(1).request, 3); // DFU_GETSTATUS
        assert_eq!(get_state(1).length, 1);
        assert_eq!(get_state(1).request, 5); // DFU_GETSTATE
    }

    /// A GETSTATUS reply round-trips, and the poll timeout uses all three of its
    /// bytes. A value with a distinct byte in each position catches a parse that
    /// dropped or misordered one. Such a parse would make the host wait the wrong
    /// time between blocks.
    #[test]
    fn a_get_status_reply_round_trips_with_a_three_byte_poll_timeout() {
        let status = GetStatus {
            status: Status::Ok,
            poll_timeout_ms: 0x030201, // one distinct byte per position
            state: State::DownloadIdle,
            string_index: 0,
        };
        let bytes = status.to_bytes();
        // bStatus, then the poll timeout little-endian, then bState, then iString.
        assert_eq!(bytes, [0x00, 0x01, 0x02, 0x03, 0x05, 0x00]);
        assert_eq!(GetStatus::parse(&bytes).unwrap(), status);
    }

    /// The busy state carries a real poll timeout. The host must wait it out
    /// before the next request.
    #[test]
    fn a_busy_reply_is_read_as_busy_with_its_wait() {
        let bytes = [0x00, 0xf4, 0x01, 0x00, 0x04, 0x00]; // OK, 500ms, dfuDNBUSY
        let status = GetStatus::parse(&bytes).unwrap();
        assert_eq!(status.status, Status::Ok);
        assert!(!status.status.is_error());
        assert_eq!(status.poll_timeout_ms, 500);
        assert_eq!(status.state, State::DownloadBusy);
        assert!(status.state.is_busy());
    }

    /// An error reply is read as an error, and the status code keeps the device's
    /// own reason. A write loop stops here and sends no further block to a faulted
    /// device.
    #[test]
    fn an_error_reply_names_the_devices_own_reason() {
        let bytes = [0x03, 0x00, 0x00, 0x00, 0x0a, 0x00]; // errWRITE, dfuERROR
        let status = GetStatus::parse(&bytes).unwrap();
        assert_eq!(status.status, Status::Write);
        assert!(status.status.is_error());
        assert_eq!(status.status.name(), "errWRITE");
        assert_eq!(status.state, State::DfuError);
        assert!(status.state.is_error());
    }

    /// A reply of the wrong length is malformed. The parse returns an error instead
    /// of reading past the end or padding the reply.
    #[test]
    fn a_status_reply_of_the_wrong_length_is_refused() {
        let short = GetStatus::parse(&[0x00, 0x00, 0x00]);
        assert!(matches!(short, Err(Error::Protocol(_))), "{short:?}");
        let long = GetStatus::parse(&[0u8; 7]);
        assert!(matches!(long, Err(Error::Protocol(_))), "{long:?}");
    }

    /// A status or state byte the DFU specification does not define is refused,
    /// not guessed at. A write that proceeded on an unknown state would act on a
    /// guess.
    #[test]
    fn a_reserved_status_or_state_is_refused_not_guessed() {
        // 0x10 is past the last defined status.
        let bad_status = GetStatus::parse(&[0x10, 0x00, 0x00, 0x00, 0x02, 0x00]);
        assert!(
            matches!(bad_status, Err(Error::Protocol(_))),
            "{bad_status:?}"
        );
        // 11 is past the last defined state.
        let bad_state = GetStatus::parse(&[0x00, 0x00, 0x00, 0x00, 11, 0x00]);
        assert!(
            matches!(bad_state, Err(Error::Protocol(_))),
            "{bad_state:?}"
        );
    }

    /// A functional descriptor is decoded field for field, and the attribute bits
    /// come apart into the four capabilities. `wTransferSize` caps each block and
    /// the write loop depends on it, so it is checked explicitly.
    #[test]
    fn a_functional_descriptor_decodes_its_fields_and_attribute_bits() {
        // bLength=9, bDescriptorType=0x21, bmAttributes=0b1011 (download, upload,
        // will-detach; not manifestation-tolerant), wDetachTimeOut=0x00fa (250),
        // wTransferSize=0x1000 (4096), bcdDFUVersion=0x0110 (DFU 1.1).
        let descriptor = [0x09, 0x21, 0x0b, 0xfa, 0x00, 0x00, 0x10, 0x10, 0x01];
        let functional = Functional::parse(&descriptor).unwrap();

        assert!(functional.can_download());
        assert!(functional.can_upload());
        assert!(functional.will_detach());
        assert!(!functional.manifestation_tolerant());
        assert_eq!(functional.detach_timeout_ms, 250);
        assert_eq!(functional.transfer_size, 4096);
        assert_eq!(functional.dfu_version, 0x0110);
    }

    /// Every consuming layer measures in the transfer size, so a size of zero is
    /// refused at the descriptor. Otherwise it would reach the agent as a sector
    /// size of zero. That is a division by zero in the verbs and, in `author-gpt`,
    /// a sector no table can be laid out in.
    #[test]
    fn a_functional_descriptor_advertising_no_transfer_size_is_refused() {
        let descriptor = [
            FUNCTIONAL_DESCRIPTOR_LEN as u8,
            FUNCTIONAL_DESCRIPTOR_TYPE,
            0x0b, // attributes: download, upload, manifestation-tolerant
            0x00,
            0x00, // detach timeout
            0x00,
            0x00, // wTransferSize: zero
            0x00,
            0x01, // bcdDFUVersion 1.0
        ];
        let error = Functional::parse(&descriptor)
            .expect_err("a transfer size of zero is no block size at all");
        assert!(matches!(error, Error::Protocol(_)), "{error:?}");
    }

    /// A descriptor with the wrong type byte is not the DFU functional descriptor,
    /// and a short one cannot carry it. Both come from the wire, so both are
    /// protocol errors rather than caller errors.
    #[test]
    fn a_wrong_or_short_functional_descriptor_is_refused() {
        // Right length, wrong bDescriptorType (0x02 is an endpoint descriptor).
        let wrong_type = Functional::parse(&[0x09, 0x02, 0x0b, 0xfa, 0x00, 0x00, 0x10, 0x10, 0x01]);
        assert!(
            matches!(wrong_type, Err(Error::Protocol(_))),
            "{wrong_type:?}"
        );

        let too_short = Functional::parse(&[0x09, 0x21, 0x0b]);
        assert!(
            matches!(too_short, Err(Error::Protocol(_))),
            "{too_short:?}"
        );
    }

    /// A configuration descriptor as a device lays one out: the configuration
    /// header, then each interface with whatever descriptors belong to it.
    ///
    /// `interfaces` gives, per interface, its number and the records that follow
    /// it. The configuration is built here rather than pasted as a hex blob, so a
    /// test can state which interface a functional descriptor follows. That
    /// association is what the walk decides.
    fn configuration(interfaces: &[(u8, &[&[u8]])]) -> Vec<u8> {
        // bLength, bDescriptorType (0x02 = configuration), wTotalLength, and the
        // rest of the nine-byte header, which the walk steps over by length.
        let mut out = vec![0x09, 0x02, 0x00, 0x00, 0x01, 0x01, 0x00, 0x80, 0x32];
        for (number, records) in interfaces {
            // A nine-byte interface descriptor; bInterfaceNumber is the third
            // byte and is the only field the walk reads.
            out.extend_from_slice(&[
                0x09,
                INTERFACE_DESCRIPTOR_TYPE,
                *number,
                0x00,
                0x00,
                0xfe,
                0x01,
                0x02,
                0x00,
            ]);
            for record in *records {
                out.extend_from_slice(record);
            }
        }
        let total = u16::try_from(out.len()).expect("a test configuration is small");
        out[2..4].copy_from_slice(&total.to_le_bytes());
        out
    }

    /// A functional descriptor belongs to the interface record that precedes it.
    /// A configuration carrying two returns the one that follows the requested
    /// interface. The web transport depends on this, because a browser shows it
    /// interface numbers and classes but not this descriptor. The interface number
    /// is all it has to select with.
    #[test]
    fn a_functional_descriptor_is_found_under_the_interface_it_follows() {
        let first: &[u8] = &[0x09, 0x21, 0x0b, 0xff, 0x00, 0x00, 0x04, 0x1a, 0x01];
        let second: &[u8] = &[0x09, 0x21, 0x0b, 0xfa, 0x00, 0x00, 0x10, 0x10, 0x01];
        let config = configuration(&[(0, &[first]), (1, &[second])]);

        // Interface 0's, whose transfer size is 0x0400.
        let found = functional_in_configuration(&config, 0).expect("interface 0 carries one");
        assert_eq!(found.transfer_size, 0x0400);

        // And interface 1's, 0x1000, which is the one a mistaken walk would
        // return for both.
        let found = functional_in_configuration(&config, 1).expect("interface 1 carries one");
        assert_eq!(found.transfer_size, 0x1000);
        assert!(found.can_upload(), "attributes ride through the walk");
    }

    /// An interface with no functional descriptor gives the host no transfer
    /// size. The walk refuses with an error naming the interface, and supplies no
    /// default size.
    #[test]
    fn an_interface_with_no_functional_descriptor_is_refused() {
        // An endpoint descriptor under interface 0, and nothing else.
        let endpoint: &[u8] = &[0x07, 0x05, 0x81, 0x02, 0x40, 0x00, 0x00];
        let config = configuration(&[(0, &[endpoint])]);

        let error = functional_in_configuration(&config, 0)
            .expect_err("there is no functional descriptor to find");
        assert!(matches!(error, Error::Protocol(_)), "{error:?}");

        // And an interface that is not in the configuration at all.
        let functional: &[u8] = &[0x09, 0x21, 0x0b, 0xfa, 0x00, 0x00, 0x10, 0x10, 0x01];
        let config = configuration(&[(0, &[functional])]);
        let error = functional_in_configuration(&config, 3)
            .expect_err("interface 3 is not in this configuration");
        assert!(matches!(error, Error::Protocol(_)), "{error:?}");
    }

    /// A malformed chain ends the walk, with no attempt to re-synchronize inside
    /// it. A zero `bLength` would loop forever, and a length running past the
    /// buffer would read bytes that are not there. Either way the walk reports the
    /// descriptor as absent.
    #[test]
    fn a_configuration_that_stops_tying_out_ends_the_walk() {
        let mut config = configuration(&[(0, &[])]);
        // A record claiming no length at all, before any functional descriptor.
        config.extend_from_slice(&[0x00, 0x21, 0x0b, 0xfa, 0x00, 0x00, 0x10, 0x10, 0x01]);
        let error =
            functional_in_configuration(&config, 0).expect_err("the walk stops at the bad record");
        assert!(matches!(error, Error::Protocol(_)), "{error:?}");

        // A record claiming more bytes than remain.
        let mut config = configuration(&[(0, &[])]);
        config.extend_from_slice(&[0x40, 0x21, 0x0b]);
        let error =
            functional_in_configuration(&config, 0).expect_err("the walk stops at the bad record");
        assert!(matches!(error, Error::Protocol(_)), "{error:?}");
    }
}
