//! The Rockchip firmware package (`update.img`): an `RKFW` header around an `RKAF`
//! archive.
//!
//! A Rockchip SDK build ends in one file. It carries a loader container, a
//! `parameter` file, and an image for each partition. The file has two layers. The
//! outer `RKFW` header says where the loader and the archive sit. The `RKAF` archive
//! is a 2048-byte header listing up to sixteen entries, the entries' bytes, and a
//! trailing checksum over all of it.
//!
//! ```text
//! RKFW header    102 bytes: version, release time, chip, loader range, archive range
//! loader         an RKBOOT container, as `db` takes
//! RKAF archive   2048-byte header, then each entry's bytes
//!                then Rockchip's CRC-32 over everything before it, 4 bytes
//! trailer        32 bytes the reference tool reads as an MD5
//! ```
//!
//! This module parses the two headers, which are all a reader needs to know where
//! everything is. Reading the file is [`firmware`](crate::firmware)'s, in one
//! forward pass through the image seam, because a package is gigabytes and is never
//! held whole.
//!
//! The `RKFW` header is **\[DOC\]** (rkdeveloptool's `STRUCT_RKIMAGE_HEAD`). The
//! `RKAF` layout and its checksum are **\[COMMUNITY\]**: rkflashtool's `rkunpack`
//! and the packing tool's own header agree on every offset. No real package has been
//! read here, so the whole format is **\[UNVERIFIED\]** against one.

use crate::{Error, Result};

/// The four bytes a firmware package begins with.
pub const PACKAGE_MAGIC: &[u8; 4] = b"RKFW";

/// The four bytes an `RKAF` archive begins with.
pub const ARCHIVE_MAGIC: &[u8; 4] = b"RKAF";

/// The length of the `RKFW` header.
pub const HEADER_LEN: usize = 102;

/// The length of the `RKAF` archive's header.
pub const ARCHIVE_HEADER_LEN: usize = 2048;

/// The length of the checksum that follows an archive.
pub const ARCHIVE_CRC_LEN: usize = 4;

/// The length of the trailer after the archive that the reference tool reads as an
/// MD5.
pub const TRAILER_LEN: usize = 32;

// `RKFW` header fields.
const OFF_VERSION: usize = 6;
const OFF_RELEASE: usize = 14;
const OFF_CHIP: usize = 21;
const OFF_LOADER_OFFSET: usize = 25;
const OFF_LOADER_SIZE: usize = 29;
const OFF_ARCHIVE_OFFSET: usize = 33;
const OFF_ARCHIVE_SIZE: usize = 37;
/// Where a package past 4 GiB writes `HI`, in the header's reserved bytes.
const OFF_HIGH_MARK: usize = 55;
/// Where such a package keeps the high 32 bits of its archive size.
const OFF_HIGH_SIZE: usize = 57;

// `RKAF` header fields.
const OFF_LENGTH: usize = 4;
const OFF_MODEL: usize = 8;
const MODEL_LEN: usize = 34;
const OFF_MACHINE_ID: usize = 42;
const MACHINE_ID_LEN: usize = 30;
const OFF_MANUFACTURER: usize = 72;
const MANUFACTURER_LEN: usize = 56;
const OFF_ARCHIVE_VERSION: usize = 132;
const OFF_COUNT: usize = 136;
const OFF_ENTRIES: usize = 140;
const ENTRY_LEN: usize = 112;
const MAX_ENTRIES: usize = 16;
const E_NAME_LEN: usize = 32;
const E_PATH: usize = 32;
const E_PATH_LEN: usize = 60;
const E_NAND_SIZE: usize = 92;
const E_POSITION: usize = 96;
const E_NAND_ADDRESS: usize = 100;
const E_PADDED_SIZE: usize = 104;
const E_SIZE: usize = 108;

/// A range of a file: where it begins, and how many bytes it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    /// The offset of its first byte.
    pub offset: u64,
    /// How many bytes it holds.
    pub len: u64,
}

impl Range {
    /// The offset one past its last byte, or `None` where that overflows.
    pub fn end(self) -> Option<u64> {
        self.offset.checked_add(self.len)
    }
}

/// When the package was built, as its header records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReleaseTime {
    /// The year.
    pub year: u16,
    /// The month, from 1.
    pub month: u8,
    /// The day of the month, from 1.
    pub day: u8,
    /// The hour.
    pub hour: u8,
    /// The minute.
    pub minute: u8,
    /// The second.
    pub second: u8,
}

/// A firmware package's `RKFW` header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageHeader {
    /// The package's version, as stored. [`version_text`] renders it.
    pub version: u32,
    /// When the package was built.
    pub release: ReleaseTime,
    /// The chip field, as four raw bytes.
    ///
    /// It sits where an RKBOOT container names its SoC. The rkflashtool unpacker
    /// reads its first byte as a family code, such as `0x38` for RK35xx.
    /// **\[COMMUNITY\]**
    ///
    /// What an RK3576 package carries there is unmeasured, so it is reported and
    /// never compared. The loader container inside carries its own
    /// claim, and that is the one [`verbs::loader_blob_refusal`] judges.
    ///
    /// [`verbs::loader_blob_refusal`]: crate::verbs::loader_blob_refusal
    pub chip: [u8; 4],
    /// Where the loader container sits.
    pub loader: Range,
    /// Where the `RKAF` archive sits, its checksum included.
    pub archive: Range,
}

/// Render a stored version as `major.minor.build`, the way rkflashtool prints one:
/// the top byte, the next byte, and the low sixteen bits.
pub fn version_text(version: u32) -> String {
    format!(
        "{}.{}.{}",
        version >> 24,
        (version >> 16) & 0xff,
        version & 0xffff
    )
}

/// Parse a firmware package's `RKFW` header from its first [`HEADER_LEN`] bytes.
///
/// A file that is not a firmware package, or is too short to hold the header, is
/// an [`Error::InvalidRequest`]. The ranges are read and not yet checked against
/// the file. The reader that has the file's length does that.
pub fn parse_header(bytes: &[u8]) -> Result<PackageHeader> {
    let header = bytes.get(..HEADER_LEN).ok_or_else(|| {
        malformed(format!(
            "the file is {} bytes, too short for the {HEADER_LEN}-byte RKFW header",
            bytes.len()
        ))
    })?;
    if !header.starts_with(PACKAGE_MAGIC) {
        return Err(malformed(
            "not a firmware package: it does not begin with RKFW".to_string(),
        ));
    }

    // A package past 4 GiB marks its header and carries the archive size's high
    // half in the reserved bytes. **[DOC]** (rkdeveloptool `CRKImage`)
    let high = if header[OFF_HIGH_MARK..OFF_HIGH_MARK + 2] == *b"HI" {
        u64::from(le_u32(header, OFF_HIGH_SIZE))
    } else {
        0
    };

    Ok(PackageHeader {
        version: le_u32(header, OFF_VERSION),
        release: ReleaseTime {
            year: u16::from_le_bytes([header[OFF_RELEASE], header[OFF_RELEASE + 1]]),
            month: header[OFF_RELEASE + 2],
            day: header[OFF_RELEASE + 3],
            hour: header[OFF_RELEASE + 4],
            minute: header[OFF_RELEASE + 5],
            second: header[OFF_RELEASE + 6],
        },
        chip: [
            header[OFF_CHIP],
            header[OFF_CHIP + 1],
            header[OFF_CHIP + 2],
            header[OFF_CHIP + 3],
        ],
        loader: Range {
            offset: u64::from(le_u32(header, OFF_LOADER_OFFSET)),
            len: u64::from(le_u32(header, OFF_LOADER_SIZE)),
        },
        archive: Range {
            offset: u64::from(le_u32(header, OFF_ARCHIVE_OFFSET)),
            len: (high << 32) | u64::from(le_u32(header, OFF_ARCHIVE_SIZE)),
        },
    })
}

/// A firmware package's `RKAF` archive, as its header lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Archive {
    /// The archive's bytes before its trailing checksum: its header and every
    /// entry. The checksum covers exactly these.
    pub length: u64,
    /// The board model the package names.
    pub model: String,
    /// The machine ID the package names.
    pub machine_id: String,
    /// The manufacturer the package names.
    pub manufacturer: String,
    /// The archive's version, as stored. [`version_text`] renders it.
    pub version: u32,
    /// The entries, in header order.
    pub entries: Vec<ArchiveEntry>,
}

/// One entry of an `RKAF` archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveEntry {
    /// The entry's name. For a partition image, it is the partition's name. A few
    /// names are roles instead: `parameter`, `bootloader` and `package-file`.
    pub name: String,
    /// The path the packing tool took the entry from, such as `Image/boot.img`. A
    /// path of `SELF` marks the package itself.
    pub path: String,
    /// The NAND size field, as stored.
    pub nand_size: u32,
    /// Where the entry's bytes begin, counted from the start of the archive.
    pub position: u64,
    /// The NAND address field, as stored. It is the partition's offset in the
    /// parameter, as the packing tool recorded it. **\[UNVERIFIED\]**
    pub nand_address: u32,
    /// The padded size field, as stored.
    pub padded_size: u32,
    /// How many bytes the entry holds.
    pub size: u64,
}

impl ArchiveEntry {
    /// Whether the entry marks the package itself rather than carrying bytes of its
    /// own.
    pub fn is_self(&self) -> bool {
        self.path == "SELF"
    }
}

/// Parse an `RKAF` archive's header from its first [`ARCHIVE_HEADER_LEN`] bytes.
///
/// Each entry's bytes must lie inside the archive's checksummed length, so every
/// byte a write takes from the archive is one the checksum covers. An entry marking
/// the package itself carries no bytes of its own and is not held to that. A header
/// that fails any of these is an [`Error::InvalidRequest`].
pub fn parse_archive_header(bytes: &[u8]) -> Result<Archive> {
    let header = bytes.get(..ARCHIVE_HEADER_LEN).ok_or_else(|| {
        malformed(format!(
            "the archive header is {} bytes, short of the {ARCHIVE_HEADER_LEN} an RKAF header \
             fills",
            bytes.len()
        ))
    })?;
    if !header.starts_with(ARCHIVE_MAGIC) {
        return Err(malformed(
            "the archive the RKFW header points at does not begin with RKAF".to_string(),
        ));
    }

    let length = u64::from(le_u32(header, OFF_LENGTH));
    if length < ARCHIVE_HEADER_LEN as u64 {
        return Err(malformed(format!(
            "the RKAF header gives the archive a length of {length} bytes, shorter than the \
             header itself"
        )));
    }

    let count = le_u32(header, OFF_COUNT) as usize;
    if count > MAX_ENTRIES {
        return Err(malformed(format!(
            "the RKAF header lists {count} entries, and an RKAF header holds at most {MAX_ENTRIES}"
        )));
    }

    let entries = (0..count)
        .map(|index| {
            let at = OFF_ENTRIES + index * ENTRY_LEN;
            let entry = &header[at..at + ENTRY_LEN];
            let parsed = ArchiveEntry {
                name: text(&entry[..E_NAME_LEN]),
                path: text(&entry[E_PATH..E_PATH + E_PATH_LEN]),
                nand_size: le_u32(entry, E_NAND_SIZE),
                position: u64::from(le_u32(entry, E_POSITION)),
                nand_address: le_u32(entry, E_NAND_ADDRESS),
                padded_size: le_u32(entry, E_PADDED_SIZE),
                size: u64::from(le_u32(entry, E_SIZE)),
            };
            if !parsed.is_self() && parsed.position + parsed.size > length {
                return Err(malformed(format!(
                    "entry '{}' runs from byte {} for {} bytes, past the {length} bytes the \
                     archive's checksum covers",
                    parsed.name, parsed.position, parsed.size
                )));
            }
            Ok(parsed)
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(Archive {
        length,
        model: text(&header[OFF_MODEL..OFF_MODEL + MODEL_LEN]),
        machine_id: text(&header[OFF_MACHINE_ID..OFF_MACHINE_ID + MACHINE_ID_LEN]),
        manufacturer: text(&header[OFF_MANUFACTURER..OFF_MANUFACTURER + MANUFACTURER_LEN]),
        version: le_u32(header, OFF_ARCHIVE_VERSION),
        entries,
    })
}

/// A container a tool unpacks, identified by its first four bytes.
///
/// Written raw to flash, none of these boots: a tool has to take each apart first.
/// [`verbs::image_refusal`](crate::verbs::image_refusal) refuses an image that
/// begins with one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    /// A firmware package, `RKFW`.
    Package,
    /// A bare `RKAF` archive, the inner layer of a package.
    Archive,
    /// An RKBOOT loader container, `LDR ` or `BOOT`.
    Loader,
}

impl Container {
    /// What the container is, in words for a person.
    pub fn describe(self) -> &'static str {
        match self {
            Container::Package => "a Rockchip firmware package (RKFW, an update.img)",
            Container::Archive => "a Rockchip RKAF archive, the inner layer of an update.img",
            Container::Loader => "a Rockchip loader container (RKBOOT)",
        }
    }
}

/// The container `first` begins, or `None`.
///
/// `first` is an image's first bytes. Fewer than four can begin nothing.
pub fn identify(first: &[u8]) -> Option<Container> {
    match first.get(..4)? {
        b"RKFW" => Some(Container::Package),
        b"RKAF" => Some(Container::Archive),
        b"LDR " | b"BOOT" => Some(Container::Loader),
        _ => None,
    }
}

/// A NUL-padded text field, up to its first NUL. A byte that is not UTF-8 becomes
/// the replacement character, because these are labels for a person.
fn text(field: &[u8]) -> String {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

/// A little-endian `u32` at `offset`. Every caller reads inside a slice whose
/// length it has already checked.
fn le_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

/// Wrap a reason as the malformed-package error. A package is user input, so a bad
/// one is [`Error::InvalidRequest`]: only the person who supplied it can fix it.
fn malformed(detail: String) -> Error {
    Error::InvalidRequest(format!("firmware package: {detail}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{PackageEntry as Entry, firmware_package as package, rkaf_archive};

    /// The header's ranges point at the loader and the archive the fixture laid
    /// down, and the version and release time read back as stored.
    #[test]
    fn the_package_header_says_where_everything_is() {
        let file = package(
            b"LDR loader bytes",
            &[Entry {
                name: "boot",
                path: "Image/boot.img",
                data: vec![7; 10],
            }],
        );
        let header = parse_header(&file).expect("the fixture is a package");

        assert_eq!(
            header.loader,
            Range {
                offset: 102,
                len: 16
            }
        );
        assert_eq!(&file[102..118], b"LDR loader bytes");
        assert_eq!(header.archive.offset, 118);
        assert_eq!(&file[118..122], ARCHIVE_MAGIC);
        assert_eq!(version_text(header.version), "1.2.3");
        assert_eq!(header.release.year, 2026);
        assert_eq!((header.release.month, header.release.day), (10, 4));
        assert_eq!(header.chip, [0x38, 0, 0, 0]);
    }

    /// A package past 4 GiB carries the archive size's high half after an `HI`
    /// mark, and the size reads back whole.
    #[test]
    fn an_archive_past_four_gib_reads_its_high_half() {
        let mut file = package(b"LDR ", &[]);
        file[OFF_HIGH_MARK..OFF_HIGH_MARK + 2].copy_from_slice(b"HI");
        file[OFF_HIGH_SIZE..OFF_HIGH_SIZE + 4].copy_from_slice(&1u32.to_le_bytes());
        let header = parse_header(&file).expect("parses");
        let low = u64::from(le_u32(&file, OFF_ARCHIVE_SIZE));
        assert_eq!(header.archive.len, (1 << 32) | low);
    }

    /// The archive header lists each entry where the fixture put it.
    #[test]
    fn the_archive_header_lists_its_entries() {
        let file = package(
            b"LDR ",
            &[
                Entry {
                    name: "parameter",
                    path: "Image/parameter.txt",
                    data: vec![1; 30],
                },
                Entry {
                    name: "boot",
                    path: "Image/boot.img",
                    data: vec![2; 3000],
                },
            ],
        );
        let header = parse_header(&file).expect("package");
        let at = header.archive.offset as usize;
        let archive = parse_archive_header(&file[at..]).expect("archive");

        assert_eq!(archive.model, "RK3576");
        assert_eq!(archive.manufacturer, "rockchip");
        assert_eq!(version_text(archive.version), "1.0.0");
        let names: Vec<&str> = archive.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["parameter", "boot"]);
        let boot = &archive.entries[1];
        assert_eq!(boot.path, "Image/boot.img");
        assert_eq!(boot.size, 3000);
        let start = at + boot.position as usize;
        assert!(file[start..start + 3000].iter().all(|&b| b == 2));
        assert_eq!(archive.length + 4, header.archive.len);
    }

    /// An entry that claims bytes past the checksummed length is refused, so no
    /// byte a write takes from the archive escapes the checksum.
    #[test]
    fn an_entry_past_the_checksummed_length_is_refused() {
        let file = package(
            b"LDR ",
            &[Entry {
                name: "boot",
                path: "b",
                data: vec![1; 10],
            }],
        );
        let at = parse_header(&file).expect("package").archive.offset as usize;
        let mut archive = file[at..].to_vec();
        let size_at = OFF_ENTRIES + E_SIZE;
        archive[size_at..size_at + 4].copy_from_slice(&0x10_0000u32.to_le_bytes());
        let err = parse_archive_header(&archive).expect_err("past the length");
        assert!(err.to_string().contains("checksum covers"), "{err}");
    }

    /// A file that is not a package, and one too short to be one, are refused.
    #[test]
    fn a_file_that_is_not_a_package_is_refused() {
        assert!(parse_header(&[0u8; HEADER_LEN]).is_err());
        assert!(parse_header(b"RKFW").is_err());
        let mut not_rkaf = vec![0u8; ARCHIVE_HEADER_LEN];
        not_rkaf[..4].copy_from_slice(b"RKAX");
        assert!(parse_archive_header(&not_rkaf).is_err());
    }

    /// A header listing more entries than it has room for is refused.
    #[test]
    fn too_many_entries_are_refused() {
        let mut archive = rkaf_archive(&[]);
        archive[OFF_COUNT..OFF_COUNT + 4].copy_from_slice(&17u32.to_le_bytes());
        assert!(parse_archive_header(&archive).is_err());
    }

    /// The containers a raw write refuses are told apart by their first four
    /// bytes, and an ID block is not one of them.
    #[test]
    fn containers_are_identified_by_their_first_bytes() {
        assert_eq!(identify(b"RKFW...."), Some(Container::Package));
        assert_eq!(identify(b"RKAF"), Some(Container::Archive));
        assert_eq!(identify(b"LDR \x66"), Some(Container::Loader));
        assert_eq!(identify(b"BOOT"), Some(Container::Loader));
        assert_eq!(
            identify(b"RKNS"),
            None,
            "an ID block is written raw at sector 64"
        );
        assert_eq!(identify(b"PARM"), None);
        assert_eq!(identify(b"RKF"), None, "three bytes begin nothing");
    }
}
