//! The StarFive SPL image header (`spl_tool`, the `.normal.out` prefix).
//!
//! The first image the JH7110 BootROM loads must begin with a **1024-byte
//! (`0x400`) StarFive header**, or the ROM does not recognize it. That image is the
//! recovery agent, an SPL sent to the ROM over UART, or the SPL the ROM reads from
//! flash at boot. `spl_tool` produces the header from a raw `u-boot-spl.bin`, and
//! the output carries the `.normal.out` extension.
//!
//! This is the most brick-prone byte layout in the StarFive flow, and it is a pure
//! transform. Like the Rockchip [`rkboot`](super::rkboot) container, it is a
//! sans-I/O codec, unit-tested with no board attached. Nothing here touches a
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
//! | `0x004` | `bofs` | 4    | where the backup copy sits on the boot medium, `0x20_0000` by default |
//! | `0x008` | (zero) | 636  | zero padding |
//! | `0x284` | `vers` | 4    | version marker, `0x0101_0101` by default |
//! | `0x288` | `fsiz` | 4    | size of the SPL body, in bytes |
//! | `0x28C` | `res1` | 4    | offset from the header start to the SPL body, `0x400` |
//! | `0x290` | `crcs` | 4    | CRC-32 of the SPL body |
//! | `0x294` | (zero) | 364  | zero padding |
//!
//! The header is exactly `0x400` bytes. The SPL body follows immediately at file
//! offset `0x400`, and `res1` records that offset.
//!
//! # One header for every medium
//!
//! The header is the same for QSPI NOR flash, an SD card and eMMC. Its `crcs`
//! always holds the body's true CRC. The `0x5A5A5A5A` that `spl_tool -i` writes
//! belongs to a whole **disk image**, at disk offset `0x290`, in sector 1 past the
//! GPT header. It makes the ROM in eMMC mode fall through to the backup address at
//! disk offset `0x4`. No SPL carries it. **\[DOC\]**
//!
//! # The checksum
//!
//! **`crcs` is the *standard* CRC-32, polynomial `0x04C11DB7`**, the ordinary
//! [`crc32`] that [`gpt`](super::gpt) uses. Its polynomial is one bit away from the
//! non-standard Rockchip `parameter` CRC-32 (`0x04C10DB7`,
//! [`crc32_rockchip`](super::crc::crc32_rockchip)), which this crate also carries.
//! The two constants look almost identical. StarFive uses the standard one, and a
//! test asserts that this codec does not call Rockchip's.
//!
//! # Recognizing a headered image
//!
//! `spl_tool` takes `bofs` and `vers` as options, and StarFive's own binaries carry
//! a `vers` of `0x0101_0001`, where `spl_tool` and `sfspl.c` write `0x0101_0101`.
//! Neither field therefore marks a headered image. [`looks_headered`] reads the two
//! fields no tool varies, `sofs` and `res1`, and [`check`] confirms the rest
//! against the body's CRC.
//!
//! # Provenance
//!
//! The layout, the little-endian byte order and the standard CRC-32 are
//! **\[DOC\]**, read from `spl_tool.c` and mainline `sfspl.c`, which agree. The
//! tests pin [`build`] and [`parse_header`] against each other and against
//! constructed fixtures. They also pin them against the four StarFive binaries, the
//! three recovery agents and Milk-V's `usbprog`, when those are present beside the
//! repository.
//! Whether the ROM checks `vers` is **\[UNVERIFIED\]**. It runs images carrying
//! either value.

use crate::codec::crc::crc32;
use crate::{Error, Result};

/// The header is exactly this many bytes, and the SPL body follows it.
pub const HEADER_LEN: usize = 0x400;

/// `sofs`: the header offset marker every tool writes, at offset `0x000`.
const SOFS: u32 = 0x240;
/// `bofs`: the default backup address, at offset `0x004`. It is a byte offset on
/// the boot medium, where the ROM looks when the copy at the start fails its check.
pub const DEFAULT_BOFS: u32 = 0x20_0000;
/// `vers`: the version marker `spl_tool` and `sfspl.c` write, at offset `0x284`.
const VERS: u32 = 0x0101_0101;
/// `res1`: the offset from the header start to the SPL body, at offset `0x28C`.
/// It is [`HEADER_LEN`], because the body follows the header with nothing
/// between.
const RES1: u32 = HEADER_LEN as u32;

// Field offsets within the 1024-byte header.
const OFF_SOFS: usize = 0x000;
const OFF_BOFS: usize = 0x004;
const OFF_VERS: usize = 0x284;
const OFF_FSIZ: usize = 0x288;
const OFF_RES1: usize = 0x28C;
const OFF_CRCS: usize = 0x290;

/// The header fields, parsed from a `.normal.out`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// `sofs`: the header offset marker.
    pub sofs: u32,
    /// `bofs`: where the backup copy sits on the boot medium.
    pub bofs: u32,
    /// `vers`: the version marker.
    pub vers: u32,
    /// `fsiz`: the size of the SPL body, in bytes.
    pub fsiz: u32,
    /// `res1`: the offset from the header start to the SPL body.
    pub res1: u32,
    /// `crcs`: the CRC-32 of the SPL body.
    pub crcs: u32,
}

/// Where a [`Prepared`] image came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The file already carried a header, which [`check`] confirmed. It is sent
    /// as it is.
    Headered,
    /// The file was a raw `u-boot-spl.bin`, and [`build`] put the header on it.
    HeaderedHere,
}

/// An SPL ready for the ROM: a `.normal.out`, whichever form it was supplied in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    /// The headered image.
    pub image: Vec<u8>,
    /// Whether the header came with the file or was built here.
    pub origin: Origin,
}

/// Build a `.normal.out`: the 1024-byte header, followed by `spl_body`.
///
/// The header's fixed fields are written as `spl_tool` writes them by default, and
/// `fsiz` records the body length. `crcs` is the body's standard CRC-32. The body is
/// appended unchanged at offset [`HEADER_LEN`].
///
/// The CRC is [`crc32`], the ordinary polynomial, and never Rockchip's
/// one-bit-different [`crc32_rockchip`](super::crc::crc32_rockchip). A test asserts
/// that `build` uses the standard one.
pub fn build(spl_body: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; HEADER_LEN + spl_body.len()];
    write_u32(&mut out, OFF_SOFS, SOFS);
    write_u32(&mut out, OFF_BOFS, DEFAULT_BOFS);
    write_u32(&mut out, OFF_VERS, VERS);
    write_u32(&mut out, OFF_FSIZ, spl_body.len() as u32);
    write_u32(&mut out, OFF_RES1, RES1);
    write_u32(&mut out, OFF_CRCS, crc32(spl_body));
    out[HEADER_LEN..].copy_from_slice(spl_body);
    out
}

/// Parse the 1024-byte header off the front of a `.normal.out`.
///
/// It reads the six `u32` fields at their fixed offsets, and judges none of them.
/// [`check`] does that. The input must be at least [`HEADER_LEN`] bytes, because a
/// shorter input is not a headered image. A shorter input returns
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

/// Whether `image` begins with a StarFive header.
///
/// It reads the two fields that every tool writes the same way, `sofs` and `res1`.
/// Raw SPL code would match both only by astronomical coincidence. It does not
/// confirm the header: [`check`] does, against the body.
pub fn looks_headered(image: &[u8]) -> bool {
    match parse_header(image) {
        Ok(header) => header.sofs == SOFS && header.res1 == RES1,
        // Too short to carry a header, so it is not one.
        Err(_) => false,
    }
}

/// Confirm that `image` is a whole, intact `.normal.out`, and return its header.
///
/// It refuses three kinds of image:
///
/// - One whose fixed fields are wrong
/// - One whose `fsiz` runs past the end of the file
/// - One whose `crcs` is not the CRC-32 of the body it names
///
/// The ROM makes the same check, so an image that fails here is one the ROM
/// refuses. A refusal is [`Error::InvalidRequest`], because the file is at
/// fault.
pub fn check(image: &[u8]) -> Result<Header> {
    let header = parse_header(image)?;
    if header.sofs != SOFS || header.res1 != RES1 {
        return Err(Error::InvalidRequest(format!(
            "this file does not begin with a StarFive header: the fields that mark one hold \
             {:#x} and {:#x}, where a header holds {SOFS:#x} and {RES1:#x}",
            header.sofs, header.res1
        )));
    }
    let body = &image[HEADER_LEN..];
    let Some(body) = body.get(..header.fsiz as usize) else {
        return Err(Error::InvalidRequest(format!(
            "this file's StarFive header says the image is {} bytes, and only {} bytes follow \
             the header. The file is cut short",
            header.fsiz,
            body.len()
        )));
    };
    let crc = crc32(body);
    if crc != header.crcs {
        return Err(Error::InvalidRequest(format!(
            "this file's StarFive header carries the CRC {:#010x}, and the image it describes \
             has the CRC {crc:#010x}. The ROM checks the same value and refuses an image that \
             fails it",
            header.crcs
        )));
    }
    Ok(header)
}

/// Make an SPL ready for the ROM, from either form a person can have.
///
/// A file that already carries a header is confirmed with [`check`] and kept as it
/// is, so a `u-boot-spl.bin.normal.out` from a release goes through unchanged. A
/// raw `u-boot-spl.bin` gets the header from [`build`]. A file that carries a
/// header is never headered a second time. A double-headered SPL does not boot, and
/// nothing on the serial line reads it back to notice.
pub fn prepare(file: &[u8]) -> Result<Prepared> {
    if looks_headered(file) {
        check(file)?;
        Ok(Prepared {
            image: file.to_vec(),
            origin: Origin::Headered,
        })
    } else {
        Ok(Prepared {
            image: build(file),
            origin: Origin::HeaderedHere,
        })
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

    /// A header, built and read back. Every fixed field holds what `spl_tool`
    /// writes by default, and `fsiz` records the body size. The body follows at
    /// `0x400`, and the checksum is the body's standard CRC-32.
    #[test]
    fn an_image_round_trips_through_its_header() {
        let body = spl_body();
        let image = build(&body);

        assert_eq!(image.len(), HEADER_LEN + body.len());
        assert_eq!(
            &image[HEADER_LEN..],
            &body[..],
            "the body follows the header"
        );

        let header = check(&image).expect("a built image passes its own check");
        assert_eq!(header.sofs, SOFS);
        assert_eq!(header.bofs, DEFAULT_BOFS);
        assert_eq!(header.vers, VERS);
        assert_eq!(header.fsiz, body.len() as u32);
        assert_eq!(header.res1, HEADER_LEN as u32);
        assert_eq!(header.crcs, crc32(&body));
    }

    /// The counterpart of the CRC module's test that keeps the two CRC-32s apart.
    /// The header's checksum is the standard CRC-32, and the Rockchip parameter
    /// CRC-32 differs from it by one bit of one constant. If `build` called the wrong
    /// function, the header would carry a checksum the ROM rejects. The write
    /// would complete without error and leave a board that does not boot.
    #[test]
    fn the_header_crc_is_the_standard_one_not_rockchips() {
        let body = spl_body();
        let header = parse_header(&build(&body)).unwrap();

        assert_eq!(header.crcs, crc32(&body), "the standard CRC-32");
        assert_ne!(
            header.crcs,
            crc32_rockchip(&body),
            "and never Rockchip's one-bit-different CRC-32"
        );
    }

    /// A header whose `crcs` is not the body's CRC is refused. That covers a
    /// damaged file, and a file carrying the `0x5A5A5A5A` disk sentinel in its own
    /// header, which the ROM refuses at every address it reads.
    #[test]
    fn a_header_whose_crc_does_not_match_its_body_is_refused() {
        let body = spl_body();
        let mut image = build(&body);
        image[OFF_CRCS..OFF_CRCS + 4].copy_from_slice(&0x5A5A_5A5Au32.to_le_bytes());

        assert!(looks_headered(&image), "it still looks headered");
        let error = check(&image).expect_err("the CRC does not match");
        assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
        assert!(
            prepare(&image).is_err(),
            "and it is refused rather than headered again"
        );
    }

    /// A header that names more body than the file holds is refused rather than
    /// read past the end. A download cut short looks like this.
    #[test]
    fn a_header_naming_more_body_than_the_file_holds_is_refused() {
        let image = build(&spl_body());
        let error = check(&image[..image.len() - 1]).expect_err("the file is cut short");
        assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
    }

    /// The header is recognized by `sofs` and `res1`, the fields no tool varies,
    /// and not by `vers` or `bofs`, which `spl_tool` takes as options and which
    /// StarFive's own binaries set differently. A header with another `vers` and
    /// another `bofs` still checks.
    #[test]
    fn a_header_is_recognized_whatever_its_version_and_backup_address() {
        let body = spl_body();
        let mut image = build(&body);
        write_u32(&mut image, OFF_VERS, 0x0101_0001);
        write_u32(&mut image, OFF_BOFS, 0x10_0000);

        assert!(looks_headered(&image));
        let header = check(&image).expect("vers and bofs do not decide what a header is");
        assert_eq!((header.vers, header.bofs), (0x0101_0001, 0x10_0000));
    }

    /// `prepare` takes either form. A raw SPL is headered, and a headered one is
    /// kept byte for byte, never headered a second time.
    #[test]
    fn prepare_headers_a_raw_spl_and_keeps_a_headered_one() {
        let body = spl_body();

        let raw = prepare(&body).expect("a raw SPL");
        assert_eq!(raw.origin, Origin::HeaderedHere);
        assert_eq!(raw.image, build(&body));

        let headered = prepare(&raw.image).expect("a .normal.out");
        assert_eq!(headered.origin, Origin::Headered);
        assert_eq!(headered.image, raw.image, "kept as it is");
    }

    /// An input too short to hold a header is refused, and is not read as a header
    /// of zeroes. The caller's file is at fault, so the error is `InvalidRequest`.
    #[test]
    fn an_input_shorter_than_the_header_is_refused() {
        let error = parse_header(&[0u8; HEADER_LEN - 1]).expect_err("too short for a header");
        assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
        assert!(!looks_headered(&[0u8; HEADER_LEN - 1]));
    }

    /// An empty body still produces a valid image. `fsiz` is zero, and `crcs` is the
    /// CRC-32 of no bytes, which is a real value a header can carry.
    #[test]
    fn an_empty_body_still_produces_a_well_formed_header() {
        let image = build(&[]);
        assert_eq!(image.len(), HEADER_LEN);
        let header = check(&image).unwrap();
        assert_eq!(header.fsiz, 0);
        assert_eq!(header.crcs, crc32(&[]));
    }

    /// The four StarFive binaries, as they ship.
    ///
    /// They are not committed: they are StarFive's and Milk-V's, not ours to
    /// publish. The test reads them from the `reference/` directory beside the
    /// repository, and skips when they are absent, as the Rockchip codecs do with
    /// the RK3576 loader.
    ///
    /// Each one passes [`check`] and is kept by [`prepare`] byte for byte. Each
    /// carries `vers` `0x0101_0001` and `bofs` `0x20_0000`. [`build`] over the same
    /// body reproduces the file except in the three places StarFive's own tool
    /// differs from `spl_tool`: `vers`, and the two words at `0x2D4` (a copy of
    /// `fsiz`) and `0x2D8` (`1`).
    #[test]
    fn the_vendor_binaries_check_and_differ_from_build_only_where_their_tool_does() {
        let base = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../reference/mars-files");
        let files = [
            "Tools/recovery/jh7110-recovery-20221205.bin",
            "Tools/recovery/jh7110-recovery-20230322.bin",
            "Tools/recovery/jh7110-devkits-recovery-20230918.bin",
            "Mars-UsbFlashTool-v2.4-Windows/Mars-UsbFlashTool-v2.4-Windows/update/\
             usbprog-mars-230510.out",
        ];
        for name in files {
            let path = format!("{base}/{name}");
            let Ok(file) = std::fs::read(&path) else {
                eprintln!("skipping: vendor binary not present at {path}");
                continue;
            };

            let header = check(&file).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(header.vers, 0x0101_0001, "{name}");
            assert_eq!(header.bofs, DEFAULT_BOFS, "{name}");
            assert_eq!(header.fsiz as usize, file.len() - HEADER_LEN, "{name}");

            let prepared = prepare(&file).unwrap();
            assert_eq!(prepared.origin, Origin::Headered, "{name}");
            assert_eq!(prepared.image, file, "{name} is sent as it ships");

            let rebuilt = build(&file[HEADER_LEN..]);
            let differ: Vec<usize> = (0..HEADER_LEN)
                .filter(|&i| rebuilt[i] != file[i])
                .map(|i| i & !3)
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            assert_eq!(
                differ,
                vec![OFF_VERS, 0x2D4, 0x2D8],
                "{name}: build differs from StarFive's tool only in vers and the two extra words"
            );
            assert_eq!(read_u32(&file, 0x2D4), header.fsiz, "{name}");
            assert_eq!(read_u32(&file, 0x2D8), 1, "{name}");
            assert_eq!(&rebuilt[HEADER_LEN..], &file[HEADER_LEN..], "{name}");
        }
    }
}
