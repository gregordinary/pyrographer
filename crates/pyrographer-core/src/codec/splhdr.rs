//! The StarFive SPL image header (`spl_tool`, the `.normal.out` prefix).
//!
//! The first image the JH7110 BootROM loads over UART must begin with a
//! **1024-byte (`0x400`) StarFive header**, or the ROM does not recognize it. That
//! image is the recovery agent, or an SPL on the direct-boot path. `spl_tool`
//! produces the header from a raw `u-boot-spl.bin`, and the output carries the
//! `.normal.out` extension.
//!
//! This is the most brick-prone byte layout in the StarFive recovery flow, and it
//! is a pure transform. Like the Rockchip [`rkboot`](super::rkboot) container, it
//! is a sans-I/O codec, unit-tested with no board attached. Nothing here touches a
//! transport or a serial line.
//!
//! Two independent tools agree on the layout field for field:
//! `starfive-tech/Tools/spl_tool/spl_tool.c` (`struct ubootsplhdr`) and mainline
//! U-Boot `tools/sfspl.c` (`struct spl_hdr`). Every field is a **little-endian**
//! `u32`. `sfspl.c` writes each field through `cpu_to_le32`, which settles the byte
//! order:
//!
//! | Offset  | Field  | Size | Value / meaning |
//! | ------- | ------ | ---- | --------------- |
//! | `0x000` | `sofs` | 4    | header offset marker, `0x240` |
//! | `0x004` | `bofs` | 4    | backup-SPL address in flash, `0x20_0000` |
//! | `0x008` | (zero) | 636  | zero padding |
//! | `0x284` | `vers` | 4    | version marker, `0x0101_0101` |
//! | `0x288` | `fsiz` | 4    | size of the SPL body, in bytes |
//! | `0x28C` | `res1` | 4    | offset from the header start to the SPL body, `0x400` |
//! | `0x290` | `crcs` | 4    | CRC-32 of the SPL body |
//! | `0x294` | (zero) | 364  | zero padding |
//!
//! The header is exactly `0x400` bytes. The SPL body follows immediately at file
//! offset `0x400`, and `res1` records that offset.
//!
//! Two facts in this layout are brick hazards. Each is the StarFive counterpart of
//! a Rockchip pitfall that this crate also guards against:
//!
//! 1. **The `crcs` field is the *standard* CRC-32, polynomial `0x04C11DB7`**, the
//!    ordinary [`crc32`] that [`gpt`](super::gpt) uses. Its polynomial is one bit
//!    away from the non-standard Rockchip `parameter` CRC-32 (`0x04C10DB7`,
//!    [`crc32_rockchip`](super::crc::crc32_rockchip)), which this crate also
//!    carries. The two constants look almost identical. StarFive uses the standard
//!    one, and a test asserts that this codec does not call Rockchip's.
//! 2. **An eMMC target needs a different header from a flash target.** For eMMC,
//!    `spl_tool -i` writes the sentinel **`0x5A5A5A5A` into `crcs`**. The ROM's CRC
//!    check then fails on purpose, and the ROM falls through to the backup-SPL
//!    address in `bofs`. The `.normal.out` for eMMC therefore differs from the one
//!    for flash, even for identical SPL code. [`build`] produces the variant for the
//!    [`Target`] the caller names. This mirrors Rockchip's own eMMC offset fixup.
//!
//! # Provenance
//!
//! The layout, the little-endian byte order and the standard CRC-32 are
//! **\[DOC\]**, read from `spl_tool.c` and mainline `sfspl.c`, which agree. The
//! eMMC sentinel is a `spl_tool` feature, because mainline `sfspl.c` builds only the
//! flash-style header. The sentinel is therefore **\[DOC\]** from `spl_tool` alone.
//!
//! The tests pin [`build`] and [`parse_header`] against each other and against
//! constructed fixtures. The repository holds no `.normal.out` captured from a real
//! tool. The exact bytes a real tool emits are therefore **\[UNVERIFIED\]** until
//! one is added as a fixture.

use crate::codec::crc::crc32;
use crate::{Error, Result};

/// The header is exactly this many bytes, and the SPL body follows it.
pub const HEADER_LEN: usize = 0x400;

/// `sofs`: the header offset marker `spl_tool` writes, at offset `0x000`.
const SOFS: u32 = 0x240;
/// `bofs`: the default backup-SPL address, at offset `0x004`. On an eMMC target,
/// the ROM loads the backup copy from this address after the deliberate CRC
/// failure.
const BOFS: u32 = 0x20_0000;
/// `vers`: the version marker `spl_tool` writes, at offset `0x284`.
const VERS: u32 = 0x0101_0101;
/// `res1`: the offset from the header start to the SPL body, at offset `0x28C`.
/// It is [`HEADER_LEN`], because the body follows the header with nothing
/// between.
const RES1: u32 = HEADER_LEN as u32;

/// The sentinel `spl_tool -i` writes into `crcs` for an eMMC target.
///
/// It is a *deliberately wrong* CRC. On eMMC, the JH7110 ROM verifies the primary
/// SPL's CRC, finds this mismatch, and falls through to the backup-SPL address in
/// `bofs`. An eMMC header therefore stores this value in place of the body's true
/// CRC. **\[DOC\]**
pub const EMMC_CRC_SENTINEL: u32 = 0x5A5A_5A5A;

// Field offsets within the 1024-byte header.
const OFF_SOFS: usize = 0x000;
const OFF_BOFS: usize = 0x004;
const OFF_VERS: usize = 0x284;
const OFF_FSIZ: usize = 0x288;
const OFF_RES1: usize = 0x28C;
const OFF_CRCS: usize = 0x290;

/// Which boot medium the headered image is destined for.
///
/// The variant changes the bytes of the header. The ROM reaches the eMMC backup
/// copy through an intentional CRC mismatch, which a flash header must not carry.
/// Sending a flash image to an eMMC slot, or the reverse, can brick a board. The
/// caller therefore names the target, and nothing here infers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// QSPI NOR flash. `crcs` holds the body's true CRC-32. The ROM verifies it and
    /// runs the primary SPL.
    NorFlash,
    /// eMMC. `crcs` holds the [`EMMC_CRC_SENTINEL`]. The ROM's CRC check fails on
    /// purpose, and the ROM loads the SPL from the backup address in `bofs`.
    Emmc,
}

/// The header fields, parsed from a `.normal.out` for inspection and for the
/// round-trip test.
///
/// A caller given a pre-built `.normal.out` parses it to tell a flash header from
/// an eMMC one. The two differ only in `crcs`, and sending the wrong one to a slot
/// can brick a board. Parsing also lets the tests pin [`build`] against something
/// other than itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// `sofs`: the header offset marker.
    pub sofs: u32,
    /// `bofs`: the backup-SPL address in flash.
    pub bofs: u32,
    /// `vers`: the version marker.
    pub vers: u32,
    /// `fsiz`: the size of the SPL body, in bytes.
    pub fsiz: u32,
    /// `res1`: the offset from the header start to the SPL body.
    pub res1: u32,
    /// `crcs`: the stored checksum. For a flash image, this is the body's CRC-32.
    /// For an eMMC image, it is the [`EMMC_CRC_SENTINEL`].
    pub crcs: u32,
}

impl Header {
    /// Which target this header is built for, read from `crcs` against the body.
    ///
    /// A header carrying the sentinel is an eMMC header. A header whose `crcs`
    /// matches the body's CRC-32 is a flash header. The sentinel is unambiguous,
    /// because a true CRC-32 of the body equals `0x5A5A5A5A` only by an astronomical
    /// coincidence.
    ///
    /// This returns `None` for a `crcs` that matches neither. It does not guess which
    /// slot the header is for, because this module must never send an image to the
    /// wrong medium.
    pub fn target_for(&self, body: &[u8]) -> Option<Target> {
        if self.crcs == EMMC_CRC_SENTINEL {
            Some(Target::Emmc)
        } else if self.crcs == crc32(body) {
            Some(Target::NorFlash)
        } else {
            None
        }
    }
}

/// Build a `.normal.out`: the 1024-byte header for `target`, followed by
/// `spl_body`.
///
/// The header's fixed fields are written verbatim, and `fsiz` records the body
/// length. For a flash target, `crcs` is the body's standard CRC-32. For an eMMC
/// target, it is the deliberate [`EMMC_CRC_SENTINEL`]. The body is appended
/// unchanged at offset [`HEADER_LEN`].
///
/// The CRC is [`crc32`], the ordinary polynomial, and never Rockchip's
/// one-bit-different [`crc32_rockchip`](super::crc::crc32_rockchip). A test asserts
/// that `build` uses the standard one.
pub fn build(spl_body: &[u8], target: Target) -> Vec<u8> {
    let fsiz = spl_body.len() as u32;
    let crcs = match target {
        Target::NorFlash => crc32(spl_body),
        Target::Emmc => EMMC_CRC_SENTINEL,
    };

    let mut out = vec![0u8; HEADER_LEN + spl_body.len()];
    write_u32(&mut out, OFF_SOFS, SOFS);
    write_u32(&mut out, OFF_BOFS, BOFS);
    write_u32(&mut out, OFF_VERS, VERS);
    write_u32(&mut out, OFF_FSIZ, fsiz);
    write_u32(&mut out, OFF_RES1, RES1);
    write_u32(&mut out, OFF_CRCS, crcs);
    out[HEADER_LEN..].copy_from_slice(spl_body);
    out
}

/// Parse the 1024-byte header off the front of a `.normal.out`.
///
/// It reads the six `u32` fields at their fixed offsets. The input must be at least
/// [`HEADER_LEN`] bytes, because a shorter input is not a headered image. A caller
/// that wants the body takes `&image[HEADER_LEN..]`. A shorter input returns
/// [`Error::InvalidRequest`], because the file a person supplied is at fault, and
/// only that person can fix it.
pub fn parse_header(image: &[u8]) -> Result<Header> {
    if image.len() < HEADER_LEN {
        return Err(Error::InvalidRequest(format!(
            "a StarFive .normal.out is at least {HEADER_LEN} bytes of header, and this input is \
             {} bytes, too short to carry one",
            image.len()
        )));
    }
    Ok(Header {
        sofs: read_u32(image, OFF_SOFS),
        bofs: read_u32(image, OFF_BOFS),
        vers: read_u32(image, OFF_VERS),
        fsiz: read_u32(image, OFF_FSIZ),
        res1: read_u32(image, OFF_RES1),
        crcs: read_u32(image, OFF_CRCS),
    })
}

/// Whether `image` already carries a StarFive `.normal.out` header.
///
/// Every header carries the three fixed markers `sofs`, `bofs` and `vers`. Raw SPL
/// code would match all three in its first 1024 bytes only by astronomical
/// coincidence. [`plan_recover`](crate::recovery::plan_recover) uses this to tell a
/// pre-headered `u-boot-spl.bin.normal.out` from the raw `u-boot-spl.bin` it
/// expects, and refuses to add a second header to an already-headered image. A
/// double-headered SPL does not boot, and the recovery path cannot read back to
/// detect it.
pub fn looks_headered(image: &[u8]) -> bool {
    match parse_header(image) {
        Ok(header) => header.sofs == SOFS && header.bofs == BOFS && header.vers == VERS,
        // Too short to carry a header, so it is not one.
        Err(_) => false,
    }
}

/// Write a little-endian `u32` at `off`. The header is a packed C struct on a
/// little-endian host, so every integer in it is little-endian.
fn write_u32(buf: &mut [u8], off: usize, value: u32) {
    buf[off..off + 4].copy_from_slice(&value.to_le_bytes());
}

/// Read a little-endian `u32` at `off`. The offset is bounded by the caller's
/// length check.
fn read_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::crc::crc32_rockchip;

    /// A body of a few hundred bytes, distinct enough that a CRC over it is not a
    /// value a mistake would land on by accident.
    fn spl_body() -> Vec<u8> {
        (0..300u32).map(|i| (i * 7 + 3) as u8).collect()
    }

    /// A flash header, built and read back. Every fixed field holds what `spl_tool`
    /// writes, and `fsiz` records the body size. The body follows at `0x400`, and
    /// the checksum is the body's standard CRC-32.
    #[test]
    fn a_flash_image_round_trips_through_its_header() {
        let body = spl_body();
        let image = build(&body, Target::NorFlash);

        assert_eq!(image.len(), HEADER_LEN + body.len());
        assert_eq!(
            &image[HEADER_LEN..],
            &body[..],
            "the body follows the header"
        );

        let header = parse_header(&image).expect("a built image carries a header");
        assert_eq!(header.sofs, SOFS);
        assert_eq!(header.bofs, BOFS);
        assert_eq!(header.vers, VERS);
        assert_eq!(header.fsiz, body.len() as u32);
        assert_eq!(header.res1, HEADER_LEN as u32);
        assert_eq!(header.crcs, crc32(&body));
        assert_eq!(header.target_for(&body), Some(Target::NorFlash));
    }

    /// The eMMC header is the flash header with a deliberately wrong CRC: the
    /// sentinel that makes the ROM fall through to the backup copy. The two variants
    /// differ in exactly one field. The `Target` enum makes a caller choose that
    /// field's value by naming the medium.
    #[test]
    fn an_emmc_image_stores_the_sentinel_and_nothing_else_changes() {
        let body = spl_body();
        let flash = build(&body, Target::NorFlash);
        let emmc = build(&body, Target::Emmc);

        let flash_header = parse_header(&flash).unwrap();
        let emmc_header = parse_header(&emmc).unwrap();

        assert_eq!(emmc_header.crcs, EMMC_CRC_SENTINEL);
        assert_ne!(
            emmc_header.crcs,
            crc32(&body),
            "the eMMC CRC is wrong on purpose"
        );
        assert_eq!(emmc_header.target_for(&body), Some(Target::Emmc));

        // Only crcs differs: sofs, bofs, vers, fsiz, res1 are identical, and so
        // are the bodies.
        assert_eq!(
            Header {
                crcs: flash_header.crcs,
                ..emmc_header.clone()
            },
            flash_header,
            "flash and eMMC headers differ in crcs alone"
        );
        assert_eq!(&flash[HEADER_LEN..], &emmc[HEADER_LEN..]);
    }

    /// The counterpart of the CRC module's test that keeps the two CRC-32s apart.
    /// The header's checksum is the standard CRC-32, and the Rockchip parameter
    /// CRC-32 differs from it by one bit of one constant. If `build` called the wrong
    /// function, a flash header would carry a checksum the ROM rejects. The write
    /// would complete without error and leave a board that does not boot. This test
    /// asserts that the stored CRC is the standard one, and is not Rockchip's.
    #[test]
    fn the_header_crc_is_the_standard_one_not_rockchips() {
        let body = spl_body();
        let header = parse_header(&build(&body, Target::NorFlash)).unwrap();

        assert_eq!(header.crcs, crc32(&body), "the standard CRC-32");
        assert_ne!(
            header.crcs,
            crc32_rockchip(&body),
            "and never Rockchip's one-bit-different CRC-32"
        );
    }

    /// A `crcs` that is neither the body's true CRC nor the eMMC sentinel fails
    /// validation. `target_for` names no medium for it, because a guess can send the
    /// image to the wrong slot.
    #[test]
    fn a_header_that_matches_neither_target_names_no_target() {
        let body = spl_body();
        let mut image = build(&body, Target::NorFlash);
        // Corrupt one byte of the stored CRC.
        image[OFF_CRCS] ^= 0xff;

        let header = parse_header(&image).unwrap();
        assert_eq!(
            header.target_for(&body),
            None,
            "a checksum that fits no target is not silently assigned to one"
        );
    }

    /// An input too short to hold a header is refused, and is not read as a header
    /// of zeroes. The caller's file is at fault, so the error is `InvalidRequest`.
    #[test]
    fn an_input_shorter_than_the_header_is_refused() {
        let error = parse_header(&[0u8; HEADER_LEN - 1]).expect_err("too short for a header");
        assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
    }

    /// An empty body still produces a valid image. `fsiz` is zero, and `crcs` is the
    /// CRC-32 of no bytes, which is a real value a flash header can carry.
    #[test]
    fn an_empty_body_still_produces_a_well_formed_header() {
        let image = build(&[], Target::NorFlash);
        assert_eq!(image.len(), HEADER_LEN);
        let header = parse_header(&image).unwrap();
        assert_eq!(header.fsiz, 0);
        assert_eq!(header.crcs, crc32(&[]));
    }
}
