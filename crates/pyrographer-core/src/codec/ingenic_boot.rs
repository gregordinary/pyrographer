//! The Ingenic XBurst USB boot ROM's `VR_*` vendor bootstrap command layer.
//!
//! An Ingenic SoC in USB boot runs its on-chip boot ROM and nothing else. DRAM is
//! uninitialized, and the boot ROM has no flash primitive of its own. Before flash
//! is reachable, the host must upload a DRAM-init stage and then a DFU-capable
//! U-Boot into memory, and jump to it. This module is the sans-I/O command layer
//! for that two-stage bootstrap. It builds the vendor control requests the boot ROM
//! answers as transport-neutral SETUP packets, and parses the one reply the boot
//! ROM sends back.
//!
//! Like the Rockchip maskrom download-boot, these requests are USB vendor control
//! transfers on endpoint 0, not a bulk pipe. They are stable across the XBurst
//! line, from the JZ4740 to the X2000 and T-series:
//!
//! | `bRequest` | Name | Direction | Carries |
//! | ---------- | ---- | --------- | ------- |
//! | `0x00` | `VR_GET_CPU_INFO` | device->host | an 8-byte CPU magic (read) |
//! | `0x01` | `VR_SET_DATA_ADDRESS` | host->device | the load address, split |
//! | `0x02` | `VR_SET_DATA_LENGTH` | host->device | the payload length, split |
//! | `0x03` | `VR_FLUSH_CACHES` | host->device | nothing |
//! | `0x04` | `VR_PROGRAM_START1` | host->device | the SRAM entry, split |
//! | `0x05` | `VR_PROGRAM_START2` | host->device | the DRAM entry, split |
//!
//! **The address and length travel in the SETUP itself, not in a data phase.** A
//! 32-bit value is split: the high 16 bits go in `wValue`, and the low 16 bits in
//! `wIndex`. None of the host-to-device requests has a data stage.
//!
//! The stage blobs travel over a separate bulk pipe: `VR_SET_DATA_ADDRESS`, then
//! the bulk write, then a `VR_PROGRAM_START*` jump. The bootstrap driver runs that
//! sequence, and this codec only frames each request. [`dfu`](super::dfu) divides
//! the work between framing and sequencing the same way.
//!
//! The bootstrap ends by jumping into the uploaded U-Boot. The board then tears
//! down its boot ROM USB identity, and re-enumerates as a standard DFU gadget,
//! which speaks the protocol in [`dfu`](super::dfu). The boot ROM's extended
//! requests (`VR_NOR_OPS` `0x06` and upward, and the X2000 `0x10` range) belong to
//! the host-driven "cloner" flash path. This module omits them, because it only
//! brings a board to a DFU-capable U-Boot, and that U-Boot owns the flash.
//!
//! The module is sans-I/O framing and touches no transport. The request builders
//! return a [`Setup`], the SETUP field values alone, and the driver turns that into
//! a [`Control`](crate::transport::Control). The tests therefore assert exact bytes
//! with no transport.
//!
//! # Confidence
//!
//! The request numbers, the endpoint-0 control shape and the `wValue`/`wIndex`
//! split are stable across the XBurst family. **\[COMMUNITY\]**
//!
//! What a real board's `VR_GET_CPU_INFO` answers is **\[UNVERIFIED\]** until a T31
//! is on the bench. This module parses the reply into its bytes and does not
//! interpret them, as `chipver` prints a Rockchip loader's reply without decoding
//! it. A pinned per-SoC value that arms the write gate belongs in
//! [`soc`](crate::soc), set from the real bytes.

use crate::{Error, Result};

/// The `bmRequestType` for a host-to-device `VR_*` request: vendor type, device
/// recipient, host-to-device direction.
pub const REQUEST_TYPE_OUT: u8 = 0x40;

/// The `bmRequestType` for a device-to-host `VR_*` request: vendor type, device
/// recipient, device-to-host direction. Only [`VR_GET_CPU_INFO`](Request::GetCpuInfo)
/// uses it.
pub const REQUEST_TYPE_IN: u8 = 0xc0;

/// The direction bit of `bmRequestType`: set for a device-to-host transfer.
///
/// The driver reads this bit from a [`Setup`] to choose between a control-IN and a
/// control-OUT. The direction is therefore stored once, in the request type byte,
/// and not tracked alongside it. [`dfu`](super::dfu) uses the same arrangement.
const DIR_IN: u8 = 0x80;

/// A `VR_*` vendor request (`bRequest`).
///
/// The enum covers the six requests of the stable bootstrap vocabulary, `0x00`
/// through `0x05`. The boot ROM's extended flash-op requests (`0x06` and up) belong
/// to the cloner path, which is not modeled.
///
/// **\[COMMUNITY\]** `xburst-tools` `ingenic_request.h`, unchanged JZ4740 -> X2000.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Request {
    /// `VR_GET_CPU_INFO`: read the CPU's 8-byte identifying magic. It is the one
    /// device-to-host request, and its reply is what pins a SoC into the write
    /// gate.
    GetCpuInfo = 0x00,
    /// `VR_SET_DATA_ADDRESS`: set the memory address the next bulk upload loads
    /// to.
    SetDataAddress = 0x01,
    /// `VR_SET_DATA_LENGTH`: set the length of the next bulk upload.
    SetDataLength = 0x02,
    /// `VR_FLUSH_CACHES`: flush the CPU's caches, so it sees freshly uploaded
    /// code rather than stale cache lines before it is jumped into.
    FlushCaches = 0x03,
    /// `VR_PROGRAM_START1`: jump to an entry point in SRAM, which runs the stage1
    /// DRAM-init blob.
    ProgramStart1 = 0x04,
    /// `VR_PROGRAM_START2`: jump to an entry point in DRAM, which runs the stage2
    /// U-Boot. The boot ROM's USB device is then torn down.
    ProgramStart2 = 0x05,
}

impl Request {
    /// The request's name, for a message a person reads. These are the reference
    /// tools' own `VR_*` identifiers.
    pub fn name(self) -> &'static str {
        match self {
            Request::GetCpuInfo => "VR_GET_CPU_INFO",
            Request::SetDataAddress => "VR_SET_DATA_ADDRESS",
            Request::SetDataLength => "VR_SET_DATA_LENGTH",
            Request::FlushCaches => "VR_FLUSH_CACHES",
            Request::ProgramStart1 => "VR_PROGRAM_START1",
            Request::ProgramStart2 => "VR_PROGRAM_START2",
        }
    }
}

/// The length of a `VR_GET_CPU_INFO` reply, in bytes.
///
/// **\[COMMUNITY\]** The reference tools read eight bytes of ASCII-ish magic. What
/// those bytes are for any given SoC is **\[UNVERIFIED\]** until pinned on
/// hardware.
pub const CPU_INFO_LEN: usize = 8;

/// A parsed `VR_GET_CPU_INFO` reply: the CPU's identifying magic, held as bytes.
///
/// The boot ROM answers with eight bytes of loosely ASCII text naming the part. The
/// reference tools show a `T31`-family string on the T-series. This type holds
/// those bytes and renders them for a person. It deliberately does not say which
/// SoC they mean. The write gate matches the exact bytes a board answers against a
/// value pinned on hardware, never an interpretation of them.
///
/// Pinned per-SoC values belong in [`soc`](crate::soc). This type is the wire
/// parse that is compared against them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuInfo {
    /// The eight raw magic bytes, exactly as the boot ROM sent them. A per-SoC gate
    /// entry is pinned against all eight.
    pub magic: [u8; CPU_INFO_LEN],
}

impl CpuInfo {
    /// Parse an 8-byte `VR_GET_CPU_INFO` reply.
    ///
    /// The reply must be exactly [`CPU_INFO_LEN`] bytes. A reply of any other
    /// length disagrees with the reference tools' account of the boot ROM. It
    /// returns [`Protocol`](Error::Protocol), because the fault is in the wire, not
    /// the caller's request. If a truncated magic were accepted, it could later be
    /// mis-pinned into the gate.
    pub fn parse(reply: &[u8]) -> Result<Self> {
        let magic: [u8; CPU_INFO_LEN] = reply.try_into().map_err(|_| {
            Error::Protocol(format!(
                "a VR_GET_CPU_INFO reply is {CPU_INFO_LEN} bytes, but the device answered with {}",
                reply.len()
            ))
        })?;
        Ok(CpuInfo { magic })
    }

    /// The magic rendered as text for a person: printable ASCII up to the first
    /// NUL, with any non-printable byte shown as `.`.
    ///
    /// It is a label for a log or an error, not a key. Every comparison, such as
    /// the write gate's, uses the bytes in [`magic`](CpuInfo::magic). The reply is
    /// often a short name NUL-padded to eight bytes, so the view stops at the first
    /// NUL, as a C string does.
    pub fn text(self) -> String {
        self.magic
            .iter()
            .take_while(|&&b| b != 0)
            .map(|&b| {
                if (0x20..0x7f).contains(&b) {
                    b as char
                } else {
                    '.'
                }
            })
            .collect()
    }
}

/// The SETUP fields of a `VR_*` control request.
///
/// It is all of a bootstrap request that crosses the wire. Every `VR_*` request is
/// a bare SETUP with no data phase, except [`get_cpu_info`], whose data phase is
/// the device's 8-byte reply. The codec builds this, and the driver turns it into a
/// [`Control`](crate::transport::Control), so the codec never names a transport.
/// [`request_type`](Setup::request_type) carries the direction, and
/// [`is_in`](Setup::is_in) reads it back. [`dfu::Setup`](super::dfu::Setup) has
/// the same shape, for the same reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Setup {
    /// `bmRequestType`: [`REQUEST_TYPE_IN`] or [`REQUEST_TYPE_OUT`].
    pub request_type: u8,
    /// `bRequest`: the [`Request`] code.
    pub request: u8,
    /// `wValue`: the high 16 bits of an address or length, or zero.
    pub value: u16,
    /// `wIndex`: the low 16 bits of an address or length, or zero.
    pub index: u16,
    /// `wLength`: the reply length for [`get_cpu_info`], and zero for every
    /// host-to-device request (their operands travel in `wValue`/`wIndex`, not a
    /// data phase).
    pub length: u16,
}

impl Setup {
    /// Whether this request's data phase is device-to-host (a control-IN).
    ///
    /// It is read from the direction bit of `bmRequestType`, so it cannot disagree
    /// with the request itself. Only [`get_cpu_info`] is an IN.
    pub fn is_in(self) -> bool {
        self.request_type & DIR_IN != 0
    }
}

/// Split a 32-bit word into the `(wValue, wIndex)` an Ingenic `VR_*` request
/// carries it in: the high 16 bits in `wValue`, the low 16 bits in `wIndex`.
///
/// **\[COMMUNITY\]** The `wValue`(MSB)/`wIndex`(LSB) split is the convention the
/// whole XBurst line shares for passing an address or length to the boot ROM.
fn split(word: u32) -> (u16, u16) {
    ((word >> 16) as u16, (word & 0xffff) as u16)
}

/// Build the SETUP for `VR_GET_CPU_INFO`: read the CPU's 8-byte magic.
///
/// It is the only device-to-host request. The driver issues a control-IN of
/// [`CPU_INFO_LEN`] bytes, and passes the reply to [`CpuInfo::parse`].
pub fn get_cpu_info() -> Setup {
    Setup {
        request_type: REQUEST_TYPE_IN,
        request: Request::GetCpuInfo as u8,
        value: 0,
        index: 0,
        length: CPU_INFO_LEN as u16,
    }
}

/// Build the SETUP for `VR_SET_DATA_ADDRESS`: set the memory address the next
/// bulk upload loads to.
///
/// `address` is split: its high half goes in `wValue`, and its low half in
/// `wIndex`.
pub fn set_data_address(address: u32) -> Setup {
    let (value, index) = split(address);
    Setup {
        request_type: REQUEST_TYPE_OUT,
        request: Request::SetDataAddress as u8,
        value,
        index,
        length: 0,
    }
}

/// Build the SETUP for `VR_SET_DATA_LENGTH`: set the length of the next bulk
/// upload.
///
/// `length` is split, its high half into `wValue` and its low half into `wIndex`.
/// That is the same split as an address, because the boot ROM takes both 32-bit
/// words through the SETUP rather than a data phase.
pub fn set_data_length(length: u32) -> Setup {
    let (value, index) = split(length);
    Setup {
        request_type: REQUEST_TYPE_OUT,
        request: Request::SetDataLength as u8,
        value,
        index,
        length: 0,
    }
}

/// Build the SETUP for `VR_FLUSH_CACHES`: flush the CPU's caches so it sees
/// freshly uploaded code before it is jumped into.
pub fn flush_caches() -> Setup {
    Setup {
        request_type: REQUEST_TYPE_OUT,
        request: Request::FlushCaches as u8,
        value: 0,
        index: 0,
        length: 0,
    }
}

/// Build the SETUP for `VR_PROGRAM_START1`: jump to `entry` in SRAM, running the
/// stage1 DRAM-init blob.
///
/// `entry` is split: its high half goes in `wValue`, and its low half in `wIndex`.
pub fn program_start1(entry: u32) -> Setup {
    let (value, index) = split(entry);
    Setup {
        request_type: REQUEST_TYPE_OUT,
        request: Request::ProgramStart1 as u8,
        value,
        index,
        length: 0,
    }
}

/// Build the SETUP for `VR_PROGRAM_START2`: jump to `entry` in DRAM, running the
/// stage2 U-Boot.
///
/// After this jump, the boot ROM's USB device is torn down, and the board
/// re-enumerates as a DFU gadget. This is therefore the last `VR_*` request of a
/// bootstrap. `entry` is split, its high half into `wValue` and its low half into
/// `wIndex`.
pub fn program_start2(entry: u32) -> Setup {
    let (value, index) = split(entry);
    Setup {
        request_type: REQUEST_TYPE_OUT,
        request: Request::ProgramStart2 as u8,
        value,
        index,
        length: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `VR_GET_CPU_INFO` is the one request that reads. Every other request has the
    /// device-to-host direction bit clear. The driver picks a control-IN or
    /// control-OUT from that bit alone. A request whose type disagreed with its
    /// direction would drive the data phase the wrong way.
    #[test]
    fn only_get_cpu_info_reads() {
        assert!(get_cpu_info().is_in());

        assert!(!set_data_address(0).is_in());
        assert!(!set_data_length(0).is_in());
        assert!(!flush_caches().is_in());
        assert!(!program_start1(0).is_in());
        assert!(!program_start2(0).is_in());
    }

    /// The SETUP of `VR_GET_CPU_INFO` pins the wire format of the one read. It has
    /// a vendor device-to-host type, request `0x00`, and no operands. It asks for
    /// exactly eight bytes, the length the reply is parsed at.
    #[test]
    fn get_cpu_info_reads_eight_bytes_with_no_operands() {
        let setup = get_cpu_info();
        assert_eq!(setup.request_type, REQUEST_TYPE_IN);
        assert_eq!(setup.request_type, 0xc0);
        assert_eq!(setup.request, 0x00);
        assert_eq!(setup.value, 0);
        assert_eq!(setup.index, 0);
        assert_eq!(setup.length, CPU_INFO_LEN as u16);
    }

    /// Every upload depends on the address split: the high half goes in `wValue`,
    /// and the low half in `wIndex`. An address with a distinct byte in every
    /// position catches a swapped half or a dropped byte. Either would land a
    /// stage blob at the wrong address and brick the bootstrap.
    #[test]
    fn set_data_address_splits_high_half_to_value_low_half_to_index() {
        let setup = set_data_address(0x1234_5678);
        assert_eq!(setup.request_type, REQUEST_TYPE_OUT);
        assert_eq!(setup.request, 0x01);
        assert_eq!(setup.value, 0x1234, "wValue is the high half");
        assert_eq!(setup.index, 0x5678, "wIndex is the low half");
        assert_eq!(
            setup.length, 0,
            "the address rides the SETUP, not a data phase"
        );

        // A high bit set must not be mishandled as a sign.
        let high = set_data_address(0x8000_0001);
        assert_eq!(high.value, 0x8000);
        assert_eq!(high.index, 0x0001);
    }

    /// The length uses the identical split, and is request `0x02` rather than
    /// `0x01`. Pinning both stops a copy-paste that left the wrong request code
    /// from passing.
    #[test]
    fn set_data_length_splits_the_same_way_under_its_own_request() {
        let setup = set_data_length(0x00ab_cdef);
        assert_eq!(setup.request, 0x02);
        assert_eq!(setup.value, 0x00ab);
        assert_eq!(setup.index, 0xcdef);
        assert_eq!(setup.length, 0);
    }

    /// The two program-start jumps split their entry address like an address. They
    /// differ only in the request byte: `0x04` for the SRAM stage1, and `0x05` for
    /// the DRAM stage2. That byte decides between running the DRAM-init blob and
    /// jumping into DRAM that is not yet initialized, so the test pins it.
    #[test]
    fn the_program_starts_differ_only_in_their_request_byte() {
        let start1 = program_start1(0x8010_0000);
        let start2 = program_start2(0x8010_0000);

        assert_eq!(start1.request, 0x04);
        assert_eq!(start2.request, 0x05);

        // Same address, so everything but the request byte matches.
        assert_eq!(start1.value, 0x8010);
        assert_eq!(start1.index, 0x0000);
        assert_eq!(start1.value, start2.value);
        assert_eq!(start1.index, start2.index);
        assert_eq!(start1.request_type, REQUEST_TYPE_OUT);
        assert_eq!(start2.request_type, REQUEST_TYPE_OUT);
    }

    /// `VR_FLUSH_CACHES` carries nothing: no operands and no data phase, request
    /// `0x03`.
    #[test]
    fn flush_caches_carries_no_operands() {
        let setup = flush_caches();
        assert_eq!(setup.request, 0x03);
        assert_eq!(setup.value, 0);
        assert_eq!(setup.index, 0);
        assert_eq!(setup.length, 0);
    }

    /// A CPU-info reply of the documented length parses to its eight raw bytes.
    /// The raw bytes survive unchanged, because a per-SoC gate entry is pinned
    /// against them.
    #[test]
    fn a_cpu_info_reply_keeps_its_bytes_verbatim() {
        // A NUL-padded name, the shape the reference tools show.
        let reply = *b"T31\0\0\0\0\0";
        let info = CpuInfo::parse(&reply).expect("eight bytes parse");
        assert_eq!(info.magic, reply);
        assert_eq!(info.text(), "T31");
    }

    /// `text` is a view for a person. It stops at the first NUL and shows any
    /// non-printable byte as `.`, so a log line stays readable. The raw bytes stay
    /// the key.
    #[test]
    fn cpu_info_text_is_a_readable_view_not_the_key() {
        let printable = CpuInfo::parse(b"X2000E\0\0").unwrap();
        assert_eq!(printable.text(), "X2000E");

        // A reply with no NUL and a control byte in it still renders.
        let raw = CpuInfo::parse(&[b'J', b'Z', 0x01, b'V', b'1', 0xff, b'!', b'?']).unwrap();
        assert_eq!(raw.text(), "JZ.V1.!?");
    }

    /// A reply of the wrong length disagrees with the reference tools' account of
    /// the boot ROM. The parse returns an error instead of reading past the end, or
    /// accepting a truncated magic that would later mis-pin the gate.
    #[test]
    fn a_cpu_info_reply_of_the_wrong_length_is_refused() {
        let short = CpuInfo::parse(b"T31");
        assert!(matches!(short, Err(Error::Protocol(_))), "{short:?}");
        let long = CpuInfo::parse(&[0u8; 9]);
        assert!(matches!(long, Err(Error::Protocol(_))), "{long:?}");
    }
}
