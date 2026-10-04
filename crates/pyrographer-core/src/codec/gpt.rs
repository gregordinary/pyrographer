//! The GUID Partition Table, as it is laid out on the flash.
//!
//! A GPT is a structure the device stores, and it is read back through the block
//! interface like any other sectors. This module is a sans-I/O codec all the same: a
//! byte layout with an endianness and a checksum, and no transport. Its tests feed it
//! the bytes a device returns.
//!
//! GPT is a UEFI specification. This module is therefore vendor-neutral, and every
//! backend that reads a GPT reads it through here. **\[DOC\]**
//!
//! A mistake in either of two details of this layout fails silently:
//!
//! - **A GUID is mixed-endian.** Its first three fields are little-endian, and its
//!   last two are not. [`Guid`] describes it.
//! - **`last_lba` is inclusive.** A partition of one sector has
//!   `first_lba == last_lba`, so its length is `last - first + 1`.
//!   [`Entry::sectors`] turns the bounds into a sector count, so a caller does not
//!   do that arithmetic itself.

use core::fmt;

use super::crc::crc32;
use crate::{Error, Result};

/// The eight bytes a GPT header begins with.
pub const SIGNATURE: &[u8; 8] = b"EFI PART";

/// The sector that holds the primary GPT header.
///
/// Sector 0 holds the protective MBR, which pyrographer does not read. A GPT is
/// identified by its own header and checked by its own CRCs. A board with a
/// malformed protective MBR and an intact GPT therefore still reads.
pub const HEADER_LBA: u64 = 1;

/// The smallest valid declared size of a GPT header, in bytes: the size of the
/// fields the specification defines.
pub const HEADER_LEN: usize = 92;

/// The smallest sector a GPT can be laid out in.
///
/// The format's own reserved structures set it. The protective MBR in sector 0
/// carries its boot signature at bytes 510 and 511. The header in sector 1 carries
/// fields out to byte [`HEADER_LEN`]. A device can report a smaller sector, as a
/// DFU gadget's `wTransferSize` legally can. [`author`] refuses such a device,
/// because the table would run past the end of its sector.
pub const MIN_SECTOR_SIZE: usize = 512;

/// Refuse a sector too small to hold a GPT header, with the reason as a sentence.
///
/// Both rebuilds call it, because each allocates a sector of the caller's size and
/// writes fields out to [`HEADER_LEN`]. It returns the detail that
/// [`Error::CorruptTable`] carries, because both rebuilds report that error.
/// [`author`] makes the same check against [`MIN_SECTOR_SIZE`] and returns
/// [`Error::InvalidRequest`] instead, because authoring is a request rather than a
/// reading.
fn check_sector_size(sector_size: usize) -> core::result::Result<(), String> {
    if sector_size < HEADER_LEN {
        return Err(format!(
            "a sector of {sector_size} bytes cannot hold a {HEADER_LEN}-byte GPT header, so there \
             is no sector to rebuild a copy into"
        ));
    }
    Ok(())
}

/// The size of one partition entry, in bytes, in every table in practical use.
///
/// The specification permits `128 * 2^n`. [`parse_header`] accepts any of those
/// sizes, and reads the actual size from the header. A caller sizing a buffer can
/// expect this one.
pub const ENTRY_LEN: usize = 128;

/// The largest partition-entry array, in bytes, that pyrographer reads from a device.
///
/// The header gives the number of entries and the size of each, and their product
/// sets how many sectors are read. A corrupt header can give numbers whose product
/// is enormous. That read is issued one 16 KiB command at a time. For a
/// plausible-looking pair of 32-bit numbers, it never finishes in practice.
///
/// [`parse_header`] therefore bounds the product before anything is read. A real
/// table is 128 entries of 128 bytes, which is 16 KiB. This cap is 64x that, so no
/// real table reaches it.
pub const MAX_ENTRY_ARRAY_LEN: usize = 1 << 20;

/// A globally unique identifier, as GPT stores one.
///
/// **The layout is mixed-endian.** A decoder that ignores this still produces a
/// plausible GUID, so the mistake does not show in the output. Of the five groups in
/// the canonical text form, the first three are stored little-endian. The last two
/// are stored in the order they are printed. So the sixteen bytes
///
/// ```text
/// 28 73 2a c1  1f f8  d2 11  ba 4b  00 a0 c9 3e c9 3b
/// ```
///
/// are the EFI System Partition type, `C12A7328-F81F-11D2-BA4B-00A0C93EC93B`, with
/// the first three groups reversed and the last two not.
///
/// The bytes are kept as they were read, and the swizzle happens once, in
/// [`Display`](fmt::Display). Nothing compares GUIDs by their text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Guid(pub [u8; 16]);

impl Guid {
    /// The all-zero GUID, which is how GPT marks a partition entry as unused.
    pub const ZERO: Guid = Guid([0u8; 16]);

    /// Whether this is the all-zero GUID.
    pub fn is_zero(&self) -> bool {
        self.0 == [0u8; 16]
    }

    /// Parse a GUID from its canonical text form: the exact inverse of
    /// [`Display`](fmt::Display).
    ///
    /// It reads five groups of hex digits, `8-4-4-4-12`, and applies the display's
    /// mixed-endian swizzle in reverse. The first three groups are byte-reversed on
    /// the way in, and the last two are not. So the text
    /// `C12A7328-F81F-11D2-BA4B-00A0C93EC93B` parses to the sixteen bytes the EFI
    /// System Partition type is stored as. `parse(g.to_string()) == g` for every
    /// `g`.
    ///
    /// It reads a type GUID or a unique-GUID override that a person wrote in a
    /// layout. A parse that reversed the wrong groups would give a partition a type
    /// that looks plausible and is wrong.
    ///
    /// A string that is not five hex groups of those lengths is an
    /// [`Error::InvalidRequest`]. The error quotes the string, so the person who
    /// typed it can correct it. The parse does not guess at a malformed one.
    pub fn parse(text: &str) -> Result<Guid> {
        let invalid = || {
            Error::InvalidRequest(format!(
                "'{text}' is not a GUID. A GUID is 32 hex digits grouped 8-4-4-4-12, like \
                 C12A7328-F81F-11D2-BA4B-00A0C93EC93B"
            ))
        };

        let groups: Vec<&str> = text.split('-').collect();
        let [g0, g1, g2, g3, g4] = groups[..] else {
            return Err(invalid());
        };
        if (g0.len(), g1.len(), g2.len(), g3.len(), g4.len()) != (8, 4, 4, 4, 12) {
            return Err(invalid());
        }

        // Each group as its bytes, most-significant first -- the order the text
        // prints them in. The swizzle is applied below, not here.
        let g0 = hex_bytes(g0).ok_or_else(invalid)?;
        let g1 = hex_bytes(g1).ok_or_else(invalid)?;
        let g2 = hex_bytes(g2).ok_or_else(invalid)?;
        let g3 = hex_bytes(g3).ok_or_else(invalid)?;
        let g4 = hex_bytes(g4).ok_or_else(invalid)?;

        // The first three groups are stored little-endian (reversed), the last two
        // as they print -- the mirror of [`Display`](fmt::Display).
        let mut bytes = [0u8; 16];
        bytes[0] = g0[3];
        bytes[1] = g0[2];
        bytes[2] = g0[1];
        bytes[3] = g0[0];
        bytes[4] = g1[1];
        bytes[5] = g1[0];
        bytes[6] = g2[1];
        bytes[7] = g2[0];
        bytes[8] = g3[0];
        bytes[9] = g3[1];
        bytes[10..16].copy_from_slice(&g4);
        Ok(Guid(bytes))
    }
}

impl fmt::Display for Guid {
    /// The canonical text form: five groups, and the first three byte-reversed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = &self.0;
        write!(
            f,
            "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-",
            b[3], b[2], b[1], b[0], b[5], b[4], b[7], b[6], b[8], b[9],
        )?;
        for byte in &b[10..16] {
            write!(f, "{byte:02X}")?;
        }
        Ok(())
    }
}

/// A GPT header, as [`parse_header`] reads one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// The header's own revision. `0x0001_0000` is GPT 1.0, the only revision that
    /// has been published.
    ///
    /// The revision is read and reported, and no check refuses on it. A header that
    /// passes its CRC is one a tool wrote intact, so an unknown revision number does
    /// not make a board unreadable. A revision that rearranged the layout would fail
    /// the CRC.
    pub revision: u32,
    /// The sector this header says it is stored in.
    pub current_lba: u64,
    /// The sector holding the other copy of this header.
    ///
    /// GPT keeps two copies: the primary near the start of the device, and a backup
    /// in its last sector. When the primary is damaged, [`partition::read`] falls
    /// back to the backup. It finds the backup by the device's geometry (the last
    /// sector) and not by this field, because a damaged primary's fields cannot be
    /// trusted. This field is reported for a caller to cross-check against the
    /// geometry.
    ///
    /// [`partition::read`]: crate::partition::read
    pub backup_lba: u64,
    /// The first sector a partition is allowed to occupy.
    pub first_usable_lba: u64,
    /// The last sector a partition is allowed to occupy, inclusive.
    pub last_usable_lba: u64,
    /// The identifier of the device as a whole.
    pub disk_guid: Guid,
    /// The sector the partition-entry array starts at.
    pub entry_array_lba: u64,
    /// How many entries the array holds, used and unused together.
    pub entry_count: u32,
    /// How many bytes one entry occupies. The specification permits `128 * 2^n`,
    /// and every table in practical use has 128.
    pub entry_len: u32,
    /// The CRC-32 of the entry array, which [`parse_entries`] checks the array
    /// against.
    pub entry_array_crc: u32,
}

impl Header {
    /// How many bytes of partition-entry array this header describes.
    ///
    /// [`parse_header`] bounds it by [`MAX_ENTRY_ARRAY_LEN`], so the read it sizes is
    /// bounded too. The product is taken in `u64`, because both factors are `u32`
    /// and their product can exceed a 32-bit `usize`. [`usize`] is 32-bit on
    /// `wasm32`, the target the web flasher runs on. A narrower multiply would wrap
    /// there before the bound could catch it.
    pub fn entry_array_len(&self) -> usize {
        // The parse bounds this below MAX_ENTRY_ARRAY_LEN, so the cast back is
        // lossless on every target; the u64 product is what keeps the multiply
        // itself from wrapping on a 32-bit one.
        (u64::from(self.entry_count) * u64::from(self.entry_len)) as usize
    }
}

/// One partition, as GPT records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// What kind of partition this is: an EFI system partition, Linux
    /// filesystem data, a vendor's own type. All-zero means the entry is unused,
    /// and [`parse_entries`] does not return those.
    pub type_guid: Guid,
    /// This partition's own identifier, unique to it.
    pub unique_guid: Guid,
    /// The first sector it occupies.
    pub first_lba: u64,
    /// The last sector it occupies, **inclusive**. Use [`sectors`](Self::sectors)
    /// rather than subtracting these.
    pub last_lba: u64,
    /// The specification's attribute bits. Bit 0 marks a partition the platform
    /// requires. The `type_guid` defines the meaning of bits 48 through 63.
    ///
    /// They are carried raw, and nothing in pyrographer acts on them.
    pub attributes: u64,
    /// The partition's name, decoded from the 36 UTF-16 code units the entry
    /// holds. It is empty for an entry that names nothing.
    pub name: String,
}

impl Entry {
    /// How many sectors the partition occupies.
    ///
    /// This is the one place the inclusive `last_lba` becomes a count.
    /// [`parse_entries`] refuses an entry whose end is before its start. It also
    /// refuses the one entry whose count overflows, the partition from sector 0 to
    /// `u64::MAX`. For a parsed entry, the result is therefore exact.
    ///
    /// The arithmetic saturates, because the fields are public and a hand-built
    /// [`Entry`] does not pass through the parse. A backwards entry counts as one
    /// sector, and the overflowing one as `u64::MAX`. No device has `u64::MAX`
    /// sectors, so the saturated value misrepresents no real partition.
    pub fn sectors(&self) -> u64 {
        self.last_lba
            .saturating_sub(self.first_lba)
            .saturating_add(1)
    }
}

/// Whether `sector`, the contents of [`HEADER_LBA`], begins with the GPT signature.
///
/// A caller asks this before [`parse_header`]. "This device has no GPT" and "this
/// device has a damaged GPT" are different findings, and only the first is normal.
/// A board that stores a Rockchip parameter table, or nothing at all, is not
/// broken. Once the signature is present, every later failure is damage to a table
/// that exists.
///
/// A sector shorter than the signature returns `false`.
pub fn has_signature(sector: &[u8]) -> bool {
    sector.starts_with(SIGNATURE)
}

/// Parse a GPT header out of the sector at [`HEADER_LBA`].
///
/// It checks the signature, the declared size and the header's CRC over itself. It
/// also refuses an entry size other than `128 * 2^n`, and an entry array larger
/// than [`MAX_ENTRY_ARRAY_LEN`]. Every failure is an [`Error::CorruptTable`]. A
/// caller asks [`has_signature`] first, so a header that fails here is a damaged
/// header and not a device with no GPT.
pub fn parse_header(sector: &[u8]) -> Result<Header> {
    let corrupt = |detail: String| Error::CorruptTable {
        format: "GPT",
        detail,
    };

    if !has_signature(sector) {
        return Err(corrupt(format!(
            "the header does not begin with {}",
            String::from_utf8_lossy(SIGNATURE)
        )));
    }

    // The header says how big it is, and its CRC covers exactly that many bytes.
    // Read the length first, and refuse one that would reach past the sector we
    // were given or fall short of the fields the specification defines -- the
    // CRC below cannot be computed at all until this number is trustworthy.
    let header_len = le_u32(sector, 0x0c)? as usize;
    if header_len < HEADER_LEN || header_len > sector.len() {
        return Err(corrupt(format!(
            "the header declares itself {header_len} bytes, which is not between {HEADER_LEN} and \
             the {} of the sector holding it",
            sector.len()
        )));
    }

    // The CRC is computed over the header with the CRC field itself zeroed,
    // because at the time it was computed that is what the field held.
    let declared = le_u32(sector, 0x10)?;
    let mut check = sector[..header_len].to_vec();
    check[0x10..0x14].fill(0);
    let computed = crc32(&check);
    if computed != declared {
        return Err(corrupt(format!(
            "the header's CRC is {computed:#010x}, and the header says {declared:#010x}"
        )));
    }

    let entry_count = le_u32(sector, 0x50)?;
    let entry_len = le_u32(sector, 0x54)?;

    // The specification permits an entry size of `128 * 2^n`, which is to say a
    // power of two no smaller than 128. Anything else is a number that would
    // slice the array into records at the wrong stride and read every field of
    // every partition out of the middle of its neighbors.
    if entry_len < ENTRY_LEN as u32 || !entry_len.is_power_of_two() {
        return Err(corrupt(format!(
            "an entry is {entry_len} bytes, which is not the 128 * 2^n the specification permits"
        )));
    }

    // And the product decides how much gets read off the device, so it is bounded
    // here, before anything is read, rather than discovered afterwards. Taken in
    // u64: both factors are u32, so their product need not fit a 32-bit usize,
    // and on wasm32 -- where the web flasher runs -- a usize multiply would wrap
    // past this very check.
    let array_len = u64::from(entry_count) * u64::from(entry_len);
    if array_len > MAX_ENTRY_ARRAY_LEN as u64 {
        return Err(corrupt(format!(
            "the header describes {entry_count} entries of {entry_len} bytes, which is \
             {array_len} bytes of partition table. That exceeds the {MAX_ENTRY_ARRAY_LEN} bytes \
             pyrographer reads, and no real table is that large"
        )));
    }

    Ok(Header {
        revision: le_u32(sector, 0x08)?,
        current_lba: le_u64(sector, 0x18)?,
        backup_lba: le_u64(sector, 0x20)?,
        first_usable_lba: le_u64(sector, 0x28)?,
        last_usable_lba: le_u64(sector, 0x30)?,
        disk_guid: guid(sector, 0x38)?,
        entry_array_lba: le_u64(sector, 0x48)?,
        entry_count,
        entry_len,
        entry_array_crc: le_u32(sector, 0x58)?,
    })
}

/// Parse the partition-entry array `header` describes.
///
/// `array` is the bytes read from [`Header::entry_array_lba`]. It must hold at least
/// [`Header::entry_array_len`] of them. The sectors it was read in can carry more,
/// and the CRC does not cover the surplus.
///
/// The array's CRC is checked before any entry is read from it. A table that fails
/// its checksum is in the state a half-finished write leaves, and is no basis for
/// planning a write. A CRC mismatch is an [`Error::CorruptTable`]. So is an array
/// shorter than the header describes, an entry whose end precedes its start, and
/// the one entry whose sector count overflows.
///
/// Unused entries, the ones with an all-zero type GUID, are dropped, so the result
/// holds partitions only. They keep the order the table lists them in, which can
/// differ from their order on the flash.
pub fn parse_entries(header: &Header, array: &[u8]) -> Result<Vec<Entry>> {
    let corrupt = |detail: String| Error::CorruptTable {
        format: "GPT",
        detail,
    };

    let len = header.entry_array_len();
    if array.len() < len {
        return Err(corrupt(format!(
            "the header describes {len} bytes of partition entries and only {} were read",
            array.len()
        )));
    }
    let array = &array[..len];

    let computed = crc32(array);
    if computed != header.entry_array_crc {
        return Err(corrupt(format!(
            "the partition entries' CRC is {computed:#010x}, and the header says {:#010x}",
            header.entry_array_crc
        )));
    }

    let mut entries = Vec::new();
    for (index, raw) in array.chunks_exact(header.entry_len as usize).enumerate() {
        let type_guid = guid(raw, 0x00)?;
        if type_guid.is_zero() {
            continue;
        }

        let first_lba = le_u64(raw, 0x20)?;
        let last_lba = le_u64(raw, 0x28)?;
        // An inclusive end below its start is not a partition of any length; it
        // is a partition of a negative one. Refused here so that `sectors()` is
        // total and cannot underflow -- and a range that ran backwards through a
        // write plan would be an underflowed sector count naming most of the
        // device.
        if last_lba < first_lba {
            return Err(corrupt(format!(
                "entry {index} runs from sector {first_lba} to sector {last_lba}, which is backwards"
            )));
        }
        // The count is the inclusive span plus one, and exactly one pair
        // overflows it: sector 0 through sector u64::MAX. A CRC proves the bytes
        // are what some tool wrote, not that they are sane, so this pair reaches
        // here on a table that checks out -- and left alone it wraps the count to
        // zero, which would make a partition spanning the whole device vanish
        // from the overlap check a write plan is read off of. No device has that
        // many sectors; it is a table read wrong.
        if first_lba == 0 && last_lba == u64::MAX {
            return Err(corrupt(format!(
                "entry {index} claims every sector up to u64::MAX, which is not a length that can \
                 be counted"
            )));
        }

        entries.push(Entry {
            type_guid,
            unique_guid: guid(raw, 0x10)?,
            first_lba,
            last_lba,
            attributes: le_u64(raw, 0x30)?,
            name: utf16_name(&raw[0x38..0x80]),
        });
    }

    Ok(entries)
}

/// A GPT copy rebuilt into the sectors that carry it: a header and an entry array,
/// laid out contiguously and ready to write.
///
/// [`rebuild_primary_from_backup`] and [`rebuild_backup_from_primary`] return one.
/// [`author`] returns two, as the copies of a fresh table. The two copies lay their
/// bytes out in opposite orders. A primary is the header, then the array. A backup
/// is the array, then the header in the device's last sector.
///
/// This type therefore records only where the run begins, [`lba`](Self::lba), and
/// the bytes. Either way, the copy is one contiguous run, written with one command.
/// An authored primary begins at sector 0, because it carries the protective MBR
/// before the header. A repaired primary begins at [`HEADER_LBA`], because a repair
/// does not touch sector 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebuiltGpt {
    /// The first sector the rebuilt bytes occupy. A repaired primary begins at
    /// [`HEADER_LBA`]. An authored primary begins at sector 0, because its run
    /// carries the protective MBR. A backup begins at its entry-array sector.
    pub lba: u64,
    /// The header and entry array, in the order the copy stores them, padded to a
    /// whole number of sectors. These are the exact bytes to write at
    /// [`lba`](Self::lba).
    pub bytes: Vec<u8>,
}

/// Rebuild the primary GPT, byte-faithfully, from an intact backup.
///
/// This is the repair for a damaged primary. When the primary is damaged,
/// [`partition::read`] recovers the partitions from the backup, and the table on
/// the device stays damaged. This function produces a fresh primary for a person to
/// write over the damaged one, which restores the second copy.
///
/// The copy is byte-for-byte. The backup's entry array is copied verbatim, so
/// every partition's type GUID, unique GUID and attribute bits survive. The uniform
/// [`Partition`] drops those fields, as [`partition`] explains, so a rebuild from a
/// partition list would have to invent them. The two copies of a healthy GPT hold
/// identical entry arrays. The repaired primary is therefore the board's own table,
/// not a new table that resembles it.
///
/// The header is rebuilt, because the two copies' headers differ: each names its
/// own sector and its twin's. Everything else carries across from the backup
/// unchanged, including the disk GUID, the usable region and the entry array's CRC.
/// Only the fields that give this copy's position are set to the primary's own:
/// `current_lba`, `backup_lba` and `entry_array_lba`. The header CRC is computed
/// fresh over them.
///
/// The arguments are these:
///
/// - `backup_header` is the backup's header sector, the same width as the sector
///   at [`HEADER_LBA`], read from the device's last sector.
/// - `backup_array` is the backup's entry-array bytes, at least
///   [`Header::entry_array_len`] of them.
/// - `backup_lba` is the sector the backup was found in, the device's last by
///   geometry. It becomes the rebuilt primary's `backup_lba`. It comes from where
///   the backup is, not from what the backup's header says. A field that points
///   at a copy is not the authority on where that copy is. [`partition::read`]
///   locates the backup by geometry for the same reason.
///
/// The backup is validated first: its header against its own CRC, and its array
/// against the CRC the header carries. A backup that fails either check is an
/// [`Error::CorruptTable`], and nothing is copied. The function makes these checks
/// whether or not the caller already has.
///
/// [`partition`]: crate::partition
/// [`partition::read`]: crate::partition::read
/// [`Partition`]: crate::partition::Partition
pub fn rebuild_primary_from_backup(
    backup_header: &[u8],
    backup_array: &[u8],
    backup_lba: u64,
    sector_size: usize,
) -> Result<RebuiltGpt> {
    let corrupt = |detail: String| Error::CorruptTable {
        format: "GPT",
        detail,
    };

    // The rebuilt copy is laid out in a sector of `sector_size`, and its fields
    // reach byte `HEADER_LEN`. `parse_header` bounds the *declared* header length
    // by the sector it was handed, which is the backup's, not this one -- so a
    // caller naming a smaller sector size than the copy it read is refused here
    // rather than panicking on the layout below.
    check_sector_size(sector_size).map_err(corrupt)?;

    // The backup must check out before it is copied: its header against its own
    // CRC, and its array against the CRC the header carries. `parse_entries`
    // makes the second check, and refusing to build a primary from bytes that do
    // not check out is what keeps a repair from faithfully reproducing damage.
    let header = parse_header(backup_header)?;
    parse_entries(&header, backup_array)?;

    // The primary array goes at HEADER_LBA + 1 -- the sector after the header,
    // where every tool writes it -- and it must fit in front of the usable region
    // the backup describes. A backup whose first usable sector leaves no room for
    // a primary array is not one this can lay a standard primary out from.
    let array_len = header.entry_array_len();
    let array_sectors = array_len.div_ceil(sector_size);
    let array_lba = HEADER_LBA + 1;
    if array_lba + array_sectors as u64 > header.first_usable_lba {
        return Err(corrupt(format!(
            "the backup GPT's first usable sector is {}, which leaves no room for a primary entry \
             array of {array_sectors} sectors after the header at sector {HEADER_LBA}",
            header.first_usable_lba
        )));
    }

    // The header's declared length, carried across verbatim so the CRC below
    // covers the same bytes the backup's did. `parse_header` has already refused
    // one outside [HEADER_LEN, sector_size].
    let header_len = le_u32(backup_header, 0x0c)? as usize;

    let mut primary = vec![0u8; sector_size];
    primary[0x00..0x08].copy_from_slice(SIGNATURE);
    primary[0x08..0x0c].copy_from_slice(&header.revision.to_le_bytes());
    primary[0x0c..0x10].copy_from_slice(&(header_len as u32).to_le_bytes());
    // 0x10..0x14 is the header CRC, computed last over zeros here.
    // 0x14..0x18 is reserved, and stays zero.
    primary[0x18..0x20].copy_from_slice(&HEADER_LBA.to_le_bytes()); // this copy is the primary
    primary[0x20..0x28].copy_from_slice(&backup_lba.to_le_bytes()); // its twin is where we found it
    primary[0x28..0x30].copy_from_slice(&header.first_usable_lba.to_le_bytes());
    primary[0x30..0x38].copy_from_slice(&header.last_usable_lba.to_le_bytes());
    primary[0x38..0x48].copy_from_slice(&header.disk_guid.0);
    primary[0x48..0x50].copy_from_slice(&array_lba.to_le_bytes()); // the primary's own array
    primary[0x50..0x54].copy_from_slice(&header.entry_count.to_le_bytes());
    primary[0x54..0x58].copy_from_slice(&header.entry_len.to_le_bytes());
    primary[0x58..0x5c].copy_from_slice(&header.entry_array_crc.to_le_bytes());

    let crc = crc32(&primary[..header_len]);
    primary[0x10..0x14].copy_from_slice(&crc.to_le_bytes());

    // The header sector, then the array padded to whole sectors: one contiguous
    // run from HEADER_LBA. The array is copied for exactly its declared length --
    // the surplus a whole-sector read carried is not part of the table -- and the
    // rest of its last sector is zeroed.
    let mut bytes = primary;
    bytes.resize(sector_size + array_sectors * sector_size, 0);
    bytes[sector_size..sector_size + array_len].copy_from_slice(&backup_array[..array_len]);

    Ok(RebuiltGpt {
        lba: HEADER_LBA,
        bytes,
    })
}

/// Rebuild the backup GPT, byte-faithfully, from an intact primary.
///
/// It is the counterpart of [`rebuild_primary_from_backup`], for a primary that
/// passes its checks and a backup that is stale, damaged or missing. It produces a
/// fresh backup to write into the device's last sectors. It is the safer of the two
/// repairs. Its run covers only the redundant copy at the end of the disk, and
/// never the primary the board boots from.
///
/// The copy is byte-for-byte, as in the primary rebuild. The primary's entry
/// array is copied verbatim, GUIDs and attribute bits included. Only the header
/// fields that give this copy's position are set fresh: `current_lba`, `backup_lba`
/// and `entry_array_lba`. The header CRC is computed fresh over them.
///
/// A backup lays its bytes out in the opposite order from a primary: the entry
/// array first, then the header in the device's last sector. The run this returns
/// is therefore `[array][header]`, beginning at [`RebuiltGpt::lba`] and ending on
/// the last sector.
///
/// The arguments are these:
///
/// - `primary_header` is the primary's header sector, read from [`HEADER_LBA`].
/// - `primary_array` is the primary's entry-array bytes, at least
///   [`Header::entry_array_len`] of them.
/// - `flash_sectors` is how many sectors the device has, which locates its last
///   sector and so the backup. The specification puts the backup in the last
///   sector. It is located by geometry rather than by the primary's own
///   `backup_lba`, for the reason [`partition::read`] locates it that way.
///
/// The primary is validated first: its header against its own CRC, and its array
/// against the CRC the header carries. A primary that fails either check is an
/// [`Error::CorruptTable`], and nothing is copied. A device with no room for a
/// backup array after the primary's usable region is also an
/// [`Error::CorruptTable`].
///
/// [`partition::read`]: crate::partition::read
pub fn rebuild_backup_from_primary(
    primary_header: &[u8],
    primary_array: &[u8],
    flash_sectors: u64,
    sector_size: usize,
) -> Result<RebuiltGpt> {
    let corrupt = |detail: String| Error::CorruptTable {
        format: "GPT",
        detail,
    };

    // As in `rebuild_primary_from_backup`: the sector the rebuilt copy is laid out
    // in is the caller's number, not the one `parse_header` bounded the source
    // against.
    check_sector_size(sector_size).map_err(corrupt)?;

    let header = parse_header(primary_header)?;
    parse_entries(&header, primary_array)?;

    // The backup header sits in the device's last sector, and its array in the
    // sectors just before it. A device with no last sector to hold a backup, or one
    // whose usable region would collide with the backup array, is not one a standard
    // backup can be laid out on.
    let last_lba = flash_sectors.checked_sub(1).ok_or_else(|| {
        corrupt(
            "the device reports no sectors, so there is no last sector to hold a backup GPT"
                .to_string(),
        )
    })?;
    let array_len = header.entry_array_len();
    let array_sectors = array_len.div_ceil(sector_size);
    let array_lba = last_lba.checked_sub(array_sectors as u64).ok_or_else(|| {
        corrupt(format!(
            "the device has {flash_sectors} sectors, too few to hold a backup entry array of \
             {array_sectors} sectors before its last"
        ))
    })?;
    // The array must sit after the usable region the primary describes: writing it
    // over the end of somebody's data is not a repair.
    if array_lba <= header.last_usable_lba {
        return Err(corrupt(format!(
            "the primary GPT's last usable sector is {}, which leaves no room for a backup entry \
             array of {array_sectors} sectors before the last sector {last_lba}",
            header.last_usable_lba
        )));
    }

    let header_len = le_u32(primary_header, 0x0c)? as usize;

    let mut backup = vec![0u8; sector_size];
    backup[0x00..0x08].copy_from_slice(SIGNATURE);
    backup[0x08..0x0c].copy_from_slice(&header.revision.to_le_bytes());
    backup[0x0c..0x10].copy_from_slice(&(header_len as u32).to_le_bytes());
    // 0x10..0x14 is the header CRC, computed last over zeros here.
    // 0x14..0x18 is reserved, and stays zero.
    backup[0x18..0x20].copy_from_slice(&last_lba.to_le_bytes()); // this copy is the backup
    backup[0x20..0x28].copy_from_slice(&HEADER_LBA.to_le_bytes()); // its twin is the primary
    backup[0x28..0x30].copy_from_slice(&header.first_usable_lba.to_le_bytes());
    backup[0x30..0x38].copy_from_slice(&header.last_usable_lba.to_le_bytes());
    backup[0x38..0x48].copy_from_slice(&header.disk_guid.0);
    backup[0x48..0x50].copy_from_slice(&array_lba.to_le_bytes()); // the backup's own array
    backup[0x50..0x54].copy_from_slice(&header.entry_count.to_le_bytes());
    backup[0x54..0x58].copy_from_slice(&header.entry_len.to_le_bytes());
    backup[0x58..0x5c].copy_from_slice(&header.entry_array_crc.to_le_bytes());

    let crc = crc32(&backup[..header_len]);
    backup[0x10..0x14].copy_from_slice(&crc.to_le_bytes());

    // The array first, then the header in the last sector: a backup's layout is a
    // primary's reversed. The run begins at `array_lba` and its header lands on
    // `last_lba`.
    let mut bytes = vec![0u8; (array_sectors + 1) * sector_size];
    bytes[..array_len].copy_from_slice(&primary_array[..array_len]);
    bytes[array_sectors * sector_size..].copy_from_slice(&backup);

    Ok(RebuiltGpt {
        lba: array_lba,
        bytes,
    })
}

/// How many entries an authored GPT lays down: 128, the count other tools write.
///
/// A real GPT's entry array has 128 slots, however many partitions exist. The first
/// slots hold the layout's partitions, and the rest are the all-zero slots
/// [`parse_entries`] skips. The reserved region at the front of an authored table
/// holds a protective MBR, a header, and 128 entries of 128 bytes. On 512-byte
/// sectors, that is the usual 34 sectors, and a partition placed at sector 34 lands
/// where a vendor's would. On 4096-byte sectors, it is 6.
pub const GPT_ENTRIES: u32 = 128;

/// The EFI System Partition type, `C12A7328-F81F-11D2-BA4B-00A0C93EC93B`.
///
/// The bytes are the mixed-endian layout a table stores, which the worked example in
/// [`Guid`] decodes. [`type_guid_for`] hands it back for the token `esp` or `efi`.
pub const EFI_SYSTEM: Guid = Guid([
    0x28, 0x73, 0x2a, 0xc1, 0x1f, 0xf8, 0xd2, 0x11, 0xba, 0x4b, 0x00, 0xa0, 0xc9, 0x3e, 0xc9, 0x3b,
]);

/// Linux filesystem data, `0FC63DAF-8483-4772-8E79-3D69D8477DE4`.
///
/// [`type_guid_for`] gives this type to a partition whose layout named no type, and
/// maps `linux` or `data` to it. It is the usual type for an embedded board's data
/// partitions. It serves as the default because no special handling is keyed on it.
pub const LINUX_DATA: Guid = Guid([
    0xaf, 0x3d, 0xc6, 0x0f, 0x83, 0x84, 0x72, 0x47, 0x8e, 0x79, 0x3d, 0x69, 0xd8, 0x47, 0x7d, 0xe4,
]);

/// Linux swap, `0657FD6D-A4AB-43C4-84E5-0933C84B4F4F`.
///
/// What [`type_guid_for`] maps the token `swap` to.
pub const LINUX_SWAP: Guid = Guid([
    0x6d, 0xfd, 0x57, 0x06, 0xab, 0xa4, 0xc4, 0x43, 0x84, 0xe5, 0x09, 0x33, 0xc8, 0x4b, 0x4f, 0x4f,
]);

/// The partition the JH7110 boot ROM loads its SPL from in SD mode,
/// `2E54B353-1271-4842-806F-E436D6AF6985`.
///
/// The ROM finds the partition by this type GUID. StarFive's VisionFive 2 SDK gives
/// it to the `spl` partition at 2 MiB, which holds a `.normal.out`. **\[DOC\]**
///
/// [`type_guid_for`] maps the token `jh7110-spl` to it. The token names the SoC,
/// because the meaning is that SoC's ROM's.
pub const JH7110_SPL: Guid = Guid([
    0x53, 0xb3, 0x54, 0x2e, 0x71, 0x12, 0x42, 0x48, 0x80, 0x6f, 0xe4, 0x36, 0xd6, 0xaf, 0x69, 0x85,
]);

/// The type GUID a layout's type token names.
///
/// The vocabulary of GPT partition types lives here, with the format that uses it.
/// The neutral [`Layout`](crate::layout::Layout) carries the raw token unchanged.
/// The token resolves three ways:
///
/// - A small named set covers the types an embedded board's layout uses: `esp` or
///   `efi`, `linux` or `data`, `swap`, and `jh7110-spl`. The names match in any
///   case.
/// - Any other token is read as a raw type GUID, so any type can be named.
/// - A partition whose layout named no type (`None`) gets [`LINUX_DATA`], the
///   default.
///
/// A token that is neither a known name nor a well-formed GUID is an
/// [`Error::InvalidRequest`] that quotes the token. It is not replaced by the
/// default, because the default would give the partition a type the person did not
/// write.
pub fn type_guid_for(kind: Option<&str>) -> Result<Guid> {
    let Some(token) = kind else {
        return Ok(LINUX_DATA);
    };

    Ok(match token.to_ascii_lowercase().as_str() {
        "linux" | "data" => LINUX_DATA,
        "esp" | "efi" => EFI_SYSTEM,
        "swap" => LINUX_SWAP,
        "jh7110-spl" => JH7110_SPL,
        // Anything else is read as a raw type GUID -- any type is nameable, at the
        // cost of naming it in full. A token that is not a GUID either is refused
        // here, with both what it was and what it could have been.
        _ => Guid::parse(token).map_err(|_| {
            Error::InvalidRequest(format!(
                "'{token}' is not a known partition type or a type GUID. Use one of linux, data, \
                 esp, efi, swap, jh7110-spl, or a GUID like {LINUX_DATA}"
            ))
        })?,
    })
}

/// The 64-bit FNV-1a offset basis, from which [`derive_guid`]'s two halves start.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// The 64-bit FNV-1a prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// A large odd constant, the 64-bit golden-ratio fraction, that separates
/// [`derive_guid`]'s two halves. It makes them two different functions of the same
/// material.
const FNV_SPREAD: u64 = 0x9e37_79b9_7f4a_7c15;

/// FNV-1a over `bytes`, from an arbitrary starting `hash`.
///
/// The starting value is normally the offset basis. [`derive_guid`] varies it to
/// separate domains and to fill two independent halves. It is pure arithmetic, like
/// the checksums in [`crc`](super::crc), and is written here for the same reason: a
/// small deterministic mixer needs no dependency.
fn fnv1a64(mut hash: u64, bytes: &[u8]) -> u64 {
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Synthesize a GUID deterministically from `material`, tagged by `domain`.
///
/// An authored table gets the two identifiers GPT requires to be unique from this
/// function by default: the disk GUID and each partition's unique GUID. A person can
/// supply them instead. The result is deterministic: the same material yields the
/// same GUID. Authoring the same layout twice therefore produces the same table, and
/// a dump of an authored table verifies against a re-author of its layout.
///
/// Determinism costs global uniqueness: two boards authored from an identical layout
/// get identical GUIDs. A byte-faithful [`clone`] has the same property. A person
/// who needs a specific GUID sets it in the layout.
///
/// `domain` separates uses that must not collide on identical material, such as a
/// disk GUID and a one-partition table's only unique GUID. The domain is mixed into
/// the seeds both FNV halves start from.
///
/// The result is a well-formed version-4 UUID. The version nibble and the RFC-4122
/// variant bits are set at their canonical positions. The mixed-endian layout
/// stores those at bytes 7 and 8, as [`Guid`] explains. A tool that reads the table
/// back therefore shows a valid v4 GUID.
///
/// [`clone`]: crate::verbs::clone
pub fn derive_guid(domain: u8, material: &[u8]) -> Guid {
    let low = fnv1a64(FNV_OFFSET ^ u64::from(domain), material);
    let high = fnv1a64(FNV_OFFSET ^ u64::from(domain) ^ FNV_SPREAD, material);

    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&low.to_be_bytes());
    bytes[8..].copy_from_slice(&high.to_be_bytes());

    // Version 4 in the high nibble of the canonical time-high field, and the
    // RFC-4122 variant in the top bits of the canonical clock-seq field. The
    // mixed-endian layout stores those canonical bytes at 7 and 8, so a display of
    // the result reads as a proper v4.
    bytes[7] = (bytes[7] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Guid(bytes)
}

/// One partition an authored table is to hold, with its GUIDs already resolved.
///
/// It is the input to [`author`]. The uniform [`Partition`] a table read returns,
/// and the [`LayoutPartition`] a person supplies, each carry a name and a range.
/// This type adds the two GUIDs a GPT entry needs and neither of those keeps. The
/// type GUID comes from the layout's type token, through [`type_guid_for`]. The
/// unique GUID is a person's override, or [`derive_guid`]'s synthesis.
///
/// The caller resolves both, as [`author_gpt`](crate::verbs::author_gpt) does.
/// [`author`] lays down exactly what it is given.
///
/// [`Partition`]: crate::partition::Partition
/// [`LayoutPartition`]: crate::layout::LayoutPartition
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthoredPartition {
    /// The name the entry will carry, at most 36 UTF-16 code units.
    pub name: String,
    /// The first sector it occupies.
    pub first_lba: u64,
    /// How many sectors it occupies. [`author`] turns this into the inclusive
    /// `last_lba` a GPT entry stores.
    pub sectors: u64,
    /// What kind of partition it is.
    pub type_guid: Guid,
    /// Its own identifier, unique within the table.
    pub unique_guid: Guid,
}

/// A fresh GPT, laid out and ready to write: both copies and the disk GUID.
///
/// [`author`] returns it. Each copy is a [`RebuiltGpt`] run, the type a repair
/// produces, so a caller writes both through the same path. The
/// [`primary`](Self::primary) begins at sector 0, with a protective MBR before the
/// header. The [`backup`](Self::backup) ends on the device's last sector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthoredGpt {
    /// The protective MBR, primary header, and entry array, one run from sector 0.
    pub primary: RebuiltGpt,
    /// The entry array and backup header, one run ending on the last sector.
    pub backup: RebuiltGpt,
    /// The disk GUID the table was given, a person's override or the synthesized
    /// default, for a caller that wants to report it.
    pub disk_guid: Guid,
}

/// The first and last sectors an authored GPT leaves for partitions, on a part of
/// `flash_sectors` sectors of `sector_size` bytes.
///
/// Sector 0 is the protective MBR and sector 1 the header. The [`GPT_ENTRIES`]-slot
/// array follows the header, and the first usable sector is after the array. The
/// backup mirrors that at the end: its header is on the last sector, and its array
/// is before it. The last usable sector is therefore a whole array plus that header
/// short of the end. On a 512-byte part, the range is 34 to `flash_sectors - 34`.
///
/// [`author`] lays a table out on this range. A caller resolving a partition that grows to
/// fill the part ends it at the last usable sector.
///
/// A part too small to hold a table and its backup is an [`Error::InvalidRequest`].
pub fn usable_range(flash_sectors: u64, sector_size: usize) -> Result<(u64, u64)> {
    let bad = Error::InvalidRequest;
    let array_sectors = (GPT_ENTRIES as usize * ENTRY_LEN).div_ceil(sector_size.max(1)) as u64;
    let first_usable = HEADER_LBA + 1 + array_sectors;
    let last_lba = flash_sectors.checked_sub(1).ok_or_else(|| {
        bad("the device reports no sectors, so there is nowhere to author a GPT".to_string())
    })?;
    let last_usable = last_lba
        .checked_sub(array_sectors)
        .and_then(|lba| lba.checked_sub(1))
        .filter(|&last_usable| last_usable >= first_usable)
        .ok_or_else(|| {
            bad(format!(
                "the device has {flash_sectors} sectors, too few to hold a GPT and its backup: the \
                 header, two entry arrays, and the backup header leave no usable region"
            ))
        })?;
    Ok((first_usable, last_usable))
}

/// Where a partition marked to grow ends in an authored GPT: one past the last
/// sector of [`usable_range`].
///
/// A layout's `-` size grows a partition to the end of the part. A GPT keeps its
/// backup in the part's last sectors, so a partition grown to the last sector
/// overlaps it, and [`author`] refuses the table. The layout parsers take this
/// value as the end a growing partition stops at, as
/// [`Layout::parse_native_growing_to`](crate::layout::Layout::parse_native_growing_to)
/// describes.
///
/// A part too small to hold a table and its backup is an [`Error::InvalidRequest`].
pub fn grow_end(flash_sectors: u64, sector_size: usize) -> Result<u64> {
    let (_, last_usable) = usable_range(flash_sectors, sector_size)?;
    Ok(last_usable + 1)
}

/// Author a fresh GPT from a resolved partition list: a protective MBR, a primary,
/// and a backup, with every CRC computed.
///
/// A repair rebuilds a damaged copy from an intact one and invents nothing.
/// Authoring builds a table from a layout the board does not hold. It lays down
/// these:
///
/// - The [`GPT_ENTRIES`]-slot array every real table has, with the given partitions
///   in the first slots and the rest empty
/// - A primary header at [`HEADER_LBA`], with its array immediately after it
/// - A protective MBR at sector 0, so a host that reads sector 0 sees a GPT disk and
///   not an empty one
/// - A backup at the end of the device, built by the same
///   [`rebuild_backup_from_primary`] a repair uses, so a backup's byte order has one
///   implementation
///
/// The partitions must fit the table's geometry. Each of these is an
/// [`Error::InvalidRequest`], refused here rather than discovered on the board:
///
/// - More than [`GPT_ENTRIES`] partitions
/// - A partition with no length, or with a name longer than the 36-unit field
/// - A partition starting before the first usable sector, inside the reserved front
/// - A partition ending after the last usable sector, inside the backup's region
///
/// The caller checks the partitions for overlap first, with
/// [`Layout::validate`](crate::layout::Layout::validate). This function checks each
/// partition against the geometry.
///
/// `flash_sectors` is how many sectors the device has, which locates the last
/// sector and so the backup. `sector_size` sizes every run. A sector smaller than
/// [`MIN_SECTOR_SIZE`], or a device too small to hold a table and its backup, is
/// refused.
///
/// [`Partition`]: crate::partition::Partition
pub fn author(
    partitions: &[AuthoredPartition],
    disk_guid: Guid,
    flash_sectors: u64,
    sector_size: usize,
) -> Result<AuthoredGpt> {
    let bad = Error::InvalidRequest;

    // A GPT is laid out in sectors, and the smallest sector it can be laid out in
    // is the one the specification's own reserved structures need: a protective
    // MBR whose boot signature sits at bytes 510 and 511. A device that reports a
    // smaller sector -- a DFU gadget advertising a 64-byte `wTransferSize`, which
    // is legal and reaches here through `author-gpt` -- has no geometry a GPT can
    // be authored into, and without this the layout below runs off the end of the
    // sector it allocated and panics rather than refusing.
    if sector_size < MIN_SECTOR_SIZE {
        return Err(bad(format!(
            "a sector of {sector_size} bytes is too small to author a GPT into: the protective \
             MBR's boot signature alone sits at byte 511, so the format needs sectors of at least \
             {MIN_SECTOR_SIZE} bytes"
        )));
    }

    let slots = GPT_ENTRIES as usize;
    if partitions.len() > slots {
        return Err(bad(format!(
            "the layout names {} partitions, and a GPT of {slots} entries holds at most {slots}",
            partitions.len()
        )));
    }

    let (first_usable, last_usable) = usable_range(flash_sectors, sector_size)?;
    let array_len = slots * ENTRY_LEN;
    let array_lba = HEADER_LBA + 1;
    // `usable_range` has refused a part with no sectors, so this does not wrap.
    let last_lba = flash_sectors - 1;

    // The entry array: the partitions in the first slots, the rest left zero, which
    // is the unused-slot GUID `parse_entries` skips.
    let mut array = vec![0u8; array_len];
    for (index, part) in partitions.iter().enumerate() {
        if part.sectors == 0 {
            return Err(bad(format!(
                "partition '{}' is zero sectors long",
                part.name
            )));
        }
        let last = part
            .first_lba
            .checked_add(part.sectors)
            .and_then(|end| end.checked_sub(1))
            .ok_or_else(|| {
                bad(format!(
                    "partition '{}' runs past any sector that can be counted",
                    part.name
                ))
            })?;
        if part.first_lba < first_usable {
            return Err(bad(format!(
                "partition '{}' begins at sector {}, inside the {first_usable} sectors a GPT \
                 reserves at the front for its protective MBR, header, and entry array",
                part.name, part.first_lba
            )));
        }
        if last > last_usable {
            return Err(bad(format!(
                "partition '{}' ends at sector {last}, past sector {last_usable}, the last sector \
                 a partition can use before the backup GPT at the end of the device",
                part.name
            )));
        }
        let name_units = part.name.encode_utf16().count();
        if name_units > 36 {
            return Err(bad(format!(
                "partition name '{}' is {name_units} UTF-16 code units, and a GPT name field holds \
                 36",
                part.name
            )));
        }

        let slot = &mut array[index * ENTRY_LEN..(index + 1) * ENTRY_LEN];
        slot[0x00..0x10].copy_from_slice(&part.type_guid.0);
        slot[0x10..0x20].copy_from_slice(&part.unique_guid.0);
        slot[0x20..0x28].copy_from_slice(&part.first_lba.to_le_bytes());
        slot[0x28..0x30].copy_from_slice(&last.to_le_bytes());
        // 0x30..0x38 attributes stay zero.
        for (unit_slot, unit) in slot[0x38..0x80]
            .as_chunks_mut::<2>()
            .0
            .iter_mut()
            .zip(part.name.encode_utf16())
        {
            *unit_slot = unit.to_le_bytes();
        }
    }
    let array_crc = crc32(&array);

    // The primary header at HEADER_LBA, naming its own position and the backup's.
    let mut header = vec![0u8; sector_size];
    header[0x00..0x08].copy_from_slice(SIGNATURE);
    header[0x08..0x0c].copy_from_slice(&0x0001_0000u32.to_le_bytes()); // revision 1.0
    header[0x0c..0x10].copy_from_slice(&(HEADER_LEN as u32).to_le_bytes());
    // 0x10..0x14 is the header CRC, computed last over zeros here.
    header[0x18..0x20].copy_from_slice(&HEADER_LBA.to_le_bytes()); // this copy is the primary
    header[0x20..0x28].copy_from_slice(&last_lba.to_le_bytes()); // the backup is the last sector
    header[0x28..0x30].copy_from_slice(&first_usable.to_le_bytes());
    header[0x30..0x38].copy_from_slice(&last_usable.to_le_bytes());
    header[0x38..0x48].copy_from_slice(&disk_guid.0);
    header[0x48..0x50].copy_from_slice(&array_lba.to_le_bytes());
    header[0x50..0x54].copy_from_slice(&GPT_ENTRIES.to_le_bytes());
    header[0x54..0x58].copy_from_slice(&(ENTRY_LEN as u32).to_le_bytes());
    header[0x58..0x5c].copy_from_slice(&array_crc.to_le_bytes());
    let crc = crc32(&header[..HEADER_LEN]);
    header[0x10..0x14].copy_from_slice(&crc.to_le_bytes());

    // The backup, from the primary header and array by the same function a repair
    // uses. It validates the primary on the way through, which is a check that this
    // built a well-formed one.
    let backup = rebuild_backup_from_primary(&header, &array, flash_sectors, sector_size)?;

    // The primary run: the protective MBR, the header, and the array, one contiguous
    // write from sector 0. The array is a whole number of sectors already.
    let mut bytes = protective_mbr(flash_sectors, sector_size);
    bytes.extend_from_slice(&header);
    bytes.extend_from_slice(&array);
    let primary = RebuiltGpt { lba: 0, bytes };

    Ok(AuthoredGpt {
        primary,
        backup,
        disk_guid,
    })
}

/// The protective MBR in sector 0 of an authored GPT disk.
///
/// It holds one partition record of type `0xEE` spanning the device from sector 1,
/// and the `55 AA` boot signature. pyrographer does not read it, because a GPT is
/// found by its own header (see [`HEADER_LBA`]). A host that reads only sector 0
/// must still see a claimed disk, not an empty one it offers to partition.
/// Authoring therefore writes the MBR every GPT tool writes.
fn protective_mbr(flash_sectors: u64, sector_size: usize) -> Vec<u8> {
    let mut mbr = vec![0u8; sector_size];

    // The single protective partition record, at the first of the four slots.
    let record = 0x1be;
    mbr[record] = 0x00; // not bootable
    mbr[record + 1] = 0x00; // starting CHS, by convention the CHS of LBA 1
    mbr[record + 2] = 0x02;
    mbr[record + 3] = 0x00;
    mbr[record + 4] = 0xee; // type: a GPT protective partition
    mbr[record + 5] = 0xff; // ending CHS, by convention the maximum
    mbr[record + 6] = 0xff;
    mbr[record + 7] = 0xff;
    mbr[record + 8..record + 12].copy_from_slice(&1u32.to_le_bytes()); // first LBA
    // The sectors it spans: everything after the MBR, capped at the 32-bit field.
    // A disk larger than 2^32 - 1 sectors stores the maximum, as the specification
    // says to.
    let span = flash_sectors.saturating_sub(1).min(u64::from(u32::MAX)) as u32;
    mbr[record + 12..record + 16].copy_from_slice(&span.to_le_bytes());

    mbr[510] = 0x55;
    mbr[511] = 0xaa;
    mbr
}

/// Read a group of an even number of hex digits into its bytes, most-significant
/// first. `None` if any digit is not hex.
///
/// [`Guid::parse`] calls it after checking the group's length. The bytes come back
/// in the group's own order, and the parse applies the swizzle.
fn hex_bytes(group: &str) -> Option<Vec<u8>> {
    (0..group.len() / 2)
        .map(|index| u8::from_str_radix(group.get(index * 2..index * 2 + 2)?, 16).ok())
        .collect()
}

/// Decode a partition name: up to 36 UTF-16 code units, little-endian, and
/// whatever follows the first NUL is padding.
///
/// An unpaired surrogate becomes the replacement character, not an error. The name
/// is a label for a person to read. A table that passes its CRC is not rejected
/// over one malformed character in one partition's name.
fn utf16_name(bytes: &[u8]) -> String {
    let units = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&pair| u16::from_le_bytes(pair))
        .take_while(|&unit| unit != 0);

    char::decode_utf16(units)
        .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

/// Read a little-endian `u32` at `offset`.
fn le_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let field: [u8; 4] = bytes
        .get(offset..offset + 4)
        .and_then(|slice| slice.try_into().ok())
        .ok_or_else(|| short(bytes, offset, 4))?;
    Ok(u32::from_le_bytes(field))
}

/// Read a little-endian `u64` at `offset`.
fn le_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    let field: [u8; 8] = bytes
        .get(offset..offset + 8)
        .and_then(|slice| slice.try_into().ok())
        .ok_or_else(|| short(bytes, offset, 8))?;
    Ok(u64::from_le_bytes(field))
}

/// Read the sixteen bytes of a [`Guid`] at `offset`, as they lie.
fn guid(bytes: &[u8], offset: usize) -> Result<Guid> {
    let field: [u8; 16] = bytes
        .get(offset..offset + 16)
        .and_then(|slice| slice.try_into().ok())
        .ok_or_else(|| short(bytes, offset, 16))?;
    Ok(Guid(field))
}

/// The error for a field that runs past the end of the bytes it is read from.
///
/// One kind of input reaches it. `parse_header` reads the declared header length
/// at offset `0x0c` before it checks any length. A sector of 8 to 15 bytes that
/// begins with the signature therefore ends here.
///
/// Every other read lies within a length already checked. In `parse_header` and the
/// two rebuilds, that is the declared header length, checked against the sector. In
/// `parse_entries`, it is the declared entry size. The error lets the readers return
/// a `Result`. No slice in this module is then indexed in a way that can panic on a
/// device's bytes.
fn short(bytes: &[u8], offset: usize, len: usize) -> Error {
    Error::CorruptTable {
        format: "GPT",
        detail: format!(
            "a {len}-byte field at offset {offset} runs past the {} bytes it is stored in",
            bytes.len()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mixed-endian swizzle, checked against a GUID whose text form is
    /// published: the EFI System Partition type. Three groups are reversed and two
    /// are not. A parser that reversed all five, or none, would still produce
    /// something that looks exactly like a GUID.
    #[test]
    fn a_guid_reverses_its_first_three_groups_and_not_its_last_two() {
        let esp = Guid([
            0x28, 0x73, 0x2a, 0xc1, // C12A7328, little-endian
            0x1f, 0xf8, // F81F, little-endian
            0xd2, 0x11, // 11D2, little-endian
            0xba, 0x4b, // BA4B, as it lies
            0x00, 0xa0, 0xc9, 0x3e, 0xc9, 0x3b, // 00A0C93EC93B, as it lies
        ]);
        assert_eq!(esp.to_string(), "C12A7328-F81F-11D2-BA4B-00A0C93EC93B");

        // A second, so this pins the swizzle rather than one lucky palindrome.
        let linux = Guid([
            0xaf, 0x3d, 0xc6, 0x0f, 0x83, 0x84, 0x72, 0x47, 0x8e, 0x79, 0x3d, 0x69, 0xd8, 0x47,
            0x7d, 0xe4,
        ]);
        assert_eq!(linux.to_string(), "0FC63DAF-8483-4772-8E79-3D69D8477DE4");
    }

    #[test]
    fn the_all_zero_guid_is_the_one_that_marks_an_entry_unused() {
        assert!(Guid::ZERO.is_zero());
        assert!(!Guid([1u8; 16]).is_zero());
    }

    /// Build a partition entry the way a table on a device holds one.
    fn entry(type_guid: [u8; 16], first_lba: u64, last_lba: u64, name: &str) -> Vec<u8> {
        let mut raw = vec![0u8; ENTRY_LEN];
        raw[0x00..0x10].copy_from_slice(&type_guid);
        raw[0x10..0x20].copy_from_slice(&[0xab; 16]); // a unique GUID
        raw[0x20..0x28].copy_from_slice(&first_lba.to_le_bytes());
        raw[0x28..0x30].copy_from_slice(&last_lba.to_le_bytes());

        for (slot, unit) in raw[0x38..0x80]
            .as_chunks_mut::<2>()
            .0
            .iter_mut()
            .zip(name.encode_utf16())
        {
            *slot = unit.to_le_bytes();
        }
        raw
    }

    /// A type GUID that is not all-zero, so the entry counts as used. No test here
    /// depends on its value.
    const A_TYPE: [u8; 16] = [0x0f; 16];

    /// Build a header whose CRCs are correct for the entry array it describes: the
    /// header a tool writing this table produces.
    fn header_for(array: &[u8], entry_count: u32) -> Vec<u8> {
        let mut sector = vec![0u8; 512];
        sector[0x00..0x08].copy_from_slice(SIGNATURE);
        sector[0x08..0x0c].copy_from_slice(&0x0001_0000u32.to_le_bytes()); // revision 1.0
        sector[0x0c..0x10].copy_from_slice(&(HEADER_LEN as u32).to_le_bytes());
        // 0x10..0x14 is the header CRC, and it is computed last, over zeros here.
        sector[0x18..0x20].copy_from_slice(&1u64.to_le_bytes()); // current
        sector[0x20..0x28].copy_from_slice(&0x1fff_ffffu64.to_le_bytes()); // backup
        sector[0x28..0x30].copy_from_slice(&34u64.to_le_bytes()); // first usable
        sector[0x30..0x38].copy_from_slice(&0x1fff_ffdeu64.to_le_bytes()); // last usable
        sector[0x38..0x48].copy_from_slice(&[0xcd; 16]); // disk GUID
        sector[0x48..0x50].copy_from_slice(&2u64.to_le_bytes()); // entry array LBA
        sector[0x50..0x54].copy_from_slice(&entry_count.to_le_bytes());
        sector[0x54..0x58].copy_from_slice(&(ENTRY_LEN as u32).to_le_bytes());
        sector[0x58..0x5c].copy_from_slice(&crc32(array).to_le_bytes());

        let crc = crc32(&sector[..HEADER_LEN]);
        sector[0x10..0x14].copy_from_slice(&crc.to_le_bytes());
        sector
    }

    /// A whole table: two named partitions and an unused slot between them.
    fn a_table() -> (Vec<u8>, Vec<u8>) {
        let array = [
            entry(A_TYPE, 64, 8255, "uboot"),
            vec![0u8; ENTRY_LEN], // an unused slot, in the middle
            entry(A_TYPE, 8256, 16447, "trust"),
        ]
        .concat();
        let sector = header_for(&array, 3);
        (sector, array)
    }

    /// The header, parsed end to end from bytes laid out as a device holds them.
    #[test]
    fn a_header_parses_out_of_the_sector_a_device_returns() {
        let (sector, _) = a_table();
        let header = parse_header(&sector).expect("a well-formed header");

        assert_eq!(header.revision, 0x0001_0000);
        assert_eq!(header.current_lba, 1);
        assert_eq!(header.backup_lba, 0x1fff_ffff);
        assert_eq!(header.first_usable_lba, 34);
        assert_eq!(header.entry_array_lba, 2);
        assert_eq!(header.entry_count, 3);
        assert_eq!(header.entry_len, 128);
        assert_eq!(header.entry_array_len(), 3 * 128);
    }

    /// The entries parse, and the parse does its two main jobs. The unused slot is
    /// dropped rather than returned as an empty partition. The inclusive `last_lba`
    /// becomes a correct count.
    #[test]
    fn the_entries_parse_and_the_unused_slots_are_not_partitions() {
        let (sector, array) = a_table();
        let header = parse_header(&sector).unwrap();
        let entries = parse_entries(&header, &array).expect("a well-formed array");

        assert_eq!(entries.len(), 2, "the empty slot is not a partition");
        assert_eq!(entries[0].name, "uboot");
        assert_eq!(entries[0].first_lba, 64);
        assert_eq!(entries[0].last_lba, 8255);
        // Inclusive: 8255 - 64 + 1, and not 8191.
        assert_eq!(entries[0].sectors(), 8192);
        assert_eq!(entries[1].name, "trust");
        assert_eq!(entries[1].sectors(), 8192);
        // The two are flush against each other, which is what an off-by-one in
        // the inclusive end would have hidden.
        assert_eq!(
            entries[0].first_lba + entries[0].sectors(),
            entries[1].first_lba
        );
    }

    /// A device with no GPT is a normal device. The caller detects one with
    /// `has_signature`, a yes-or-no question, rather than by catching an error.
    #[test]
    fn a_sector_that_is_not_a_gpt_header_says_so_rather_than_failing() {
        assert!(!has_signature(&[0u8; 512]));
        assert!(!has_signature(b"PARM"));
        assert!(!has_signature(&[]), "a short sector has no signature");
        assert!(has_signature(SIGNATURE));

        let (sector, _) = a_table();
        assert!(has_signature(&sector));
    }

    /// A header with the signature and a CRC that disagrees with its own bytes is
    /// damage, which is a different finding from absence. Reporting it as "no
    /// partitions" would hide a half-written table behind a blank list. The person
    /// reading that list can be about to write to the device.
    #[test]
    fn a_header_whose_crc_disagrees_with_it_is_corrupt_and_not_absent() {
        let (mut sector, _) = a_table();
        sector[0x28] ^= 0xff; // move first_usable_lba, and the CRC no longer holds

        assert!(has_signature(&sector), "the signature is still there");
        let error = parse_header(&sector).expect_err("the CRC covers that byte");
        assert!(
            matches!(error, Error::CorruptTable { format: "GPT", .. }),
            "{error:?}"
        );
    }

    /// The entry array carries its own CRC, checked before any partition is read
    /// from the array. A table that fails its checksum is what a half-finished
    /// write leaves, and it is no basis for planning the next write.
    #[test]
    fn an_entry_array_whose_crc_disagrees_with_it_is_corrupt() {
        let (sector, mut array) = a_table();
        let header = parse_header(&sector).unwrap();

        array[0x20] ^= 0xff; // move a partition's first sector

        let error = parse_entries(&header, &array).expect_err("the array CRC covers that byte");
        assert!(
            matches!(error, Error::CorruptTable { format: "GPT", .. }),
            "{error:?}"
        );
    }

    /// The header sizes the read of the entry array. A header naming an absurd
    /// number of entries would have pyrographer read gigabytes from a device, one
    /// command at a time. The parse checks the bound before anything is read.
    #[test]
    fn a_header_describing_more_entries_than_could_exist_is_refused_before_any_read() {
        let array = entry(A_TYPE, 64, 8255, "uboot");
        let mut sector = header_for(&array, u32::MAX);
        // The count is inside the CRC, so a header claiming it has to be a header
        // whose CRC agrees -- which is precisely the corrupt-but-plausible case
        // the bound exists for. `header_for` computed it for us.
        assert_eq!(le_u32(&sector, 0x50).unwrap(), u32::MAX);

        let error = parse_header(&sector).expect_err("that is 500 GB of partition table");
        assert!(
            matches!(error, Error::CorruptTable { format: "GPT", .. }),
            "{error:?}"
        );

        // And the largest array that is allowed still parses, so the bound is a
        // bound and not a blanket refusal.
        let count = (MAX_ENTRY_ARRAY_LEN / ENTRY_LEN) as u32;
        sector = header_for(&[], count);
        let header = parse_header(&sector).expect("exactly at the cap is not past it");
        assert_eq!(header.entry_array_len(), MAX_ENTRY_ARRAY_LEN);
    }

    /// An entry size that is not `128 * 2^n` would slice the array at the wrong
    /// stride. Every field of every partition would be read from the middle of its
    /// neighbors, producing named partitions at sectors no partition occupies.
    #[test]
    fn an_entry_size_the_specification_does_not_permit_is_refused() {
        let refuses = |entry_len: u32| {
            let mut sector = vec![0u8; 512];
            sector[0x00..0x08].copy_from_slice(SIGNATURE);
            sector[0x0c..0x10].copy_from_slice(&(HEADER_LEN as u32).to_le_bytes());
            sector[0x54..0x58].copy_from_slice(&entry_len.to_le_bytes());
            let crc = crc32(&sector[..HEADER_LEN]);
            sector[0x10..0x14].copy_from_slice(&crc.to_le_bytes());

            matches!(
                parse_header(&sector),
                Err(Error::CorruptTable { format: "GPT", .. })
            )
        };

        assert!(refuses(0), "an entry of no bytes");
        assert!(refuses(64), "smaller than the specification's minimum");
        assert!(refuses(192), "not a power of two, though it is a multiple");
        assert!(!refuses(128), "the size every real table uses");
        assert!(!refuses(256), "128 * 2, which the specification permits");
    }

    /// An inclusive end below its start describes a partition of negative length.
    /// No count describes it, and `sectors()` saturates it to one sector. The parse
    /// refuses it, so every parsed entry runs forwards.
    #[test]
    fn a_partition_whose_end_precedes_its_start_is_refused() {
        let array = entry(A_TYPE, 8192, 64, "backwards");
        let sector = header_for(&array, 1);
        let header = parse_header(&sector).unwrap();

        let error = parse_entries(&header, &array).expect_err("it ends before it begins");
        assert!(
            matches!(error, Error::CorruptTable { format: "GPT", .. }),
            "{error:?}"
        );
    }

    /// A partition of a single sector has its end equal to its start. A mix-up of
    /// inclusive and exclusive ends shows at this boundary. A count of zero here
    /// would describe a partition that a write could pass over without reporting.
    #[test]
    fn a_partition_of_one_sector_is_one_sector_long() {
        let array = entry(A_TYPE, 64, 64, "tiny");
        let sector = header_for(&array, 1);
        let header = parse_header(&sector).unwrap();
        let entries = parse_entries(&header, &array).unwrap();

        assert_eq!(entries[0].sectors(), 1);
    }

    /// A partition from sector 0 to `u64::MAX` is the one pair whose
    /// inclusive-end-plus-one count overflows. A CRC proves the bytes are what some
    /// tool wrote, not that they are sane. This pair therefore reaches the parse on
    /// a table that passes its CRC. Its true count is 2^64, which no `u64` holds.
    /// `sectors()` saturates it to `u64::MAX`, one short. The parse refuses it, so
    /// every parsed entry's count is exact.
    #[test]
    fn a_partition_spanning_the_whole_u64_address_space_is_refused() {
        let array = entry(A_TYPE, 0, u64::MAX, "everything");
        let sector = header_for(&array, 1);
        let header = parse_header(&sector).unwrap();

        let error = parse_entries(&header, &array).expect_err("its count overflows");
        assert!(
            matches!(error, Error::CorruptTable { format: "GPT", .. }),
            "{error:?}"
        );
    }

    /// `sectors()` itself does not overflow, whatever an [`Entry`] holds. The
    /// fields are public, so a hand-built entry bypasses the parse that refuses
    /// this case. Saturating is safe, because no device has `u64::MAX` sectors for
    /// the saturated value to misrepresent.
    #[test]
    fn sectors_saturates_rather_than_overflowing_on_a_hand_built_entry() {
        let spanning = Entry {
            type_guid: Guid([0x0f; 16]),
            unique_guid: Guid::ZERO,
            first_lba: 0,
            last_lba: u64::MAX,
            attributes: 0,
            name: String::new(),
        };
        assert_eq!(spanning.sectors(), u64::MAX);

        // A backwards hand-built entry does not panic either, though the parse
        // would never have produced one.
        let backwards = Entry {
            last_lba: 10,
            first_lba: 20,
            ..spanning
        };
        assert_eq!(backwards.sectors(), 1);
    }

    /// The entry-array bound is taken in `u64`, so it also holds on a 32-bit
    /// `usize`, as on the `wasm32` target the web flasher runs on. On a 64-bit host
    /// the multiply cannot wrap, and this test only confirms the refusal. The value
    /// is chosen so that its product, truncated to 32 bits, falls under the cap.
    /// That is the regression the `u64` arithmetic prevents.
    #[test]
    fn an_entry_array_bound_holds_on_a_thirty_two_bit_usize() {
        // 0x0100_0001 entries of 256 bytes is ~4 GiB, which truncates to 256 in
        // a 32-bit multiply -- under the 1 MiB cap. In u64 it is refused.
        let mut sector = vec![0u8; 512];
        sector[0x00..0x08].copy_from_slice(SIGNATURE);
        sector[0x0c..0x10].copy_from_slice(&(HEADER_LEN as u32).to_le_bytes());
        sector[0x50..0x54].copy_from_slice(&0x0100_0001u32.to_le_bytes());
        sector[0x54..0x58].copy_from_slice(&256u32.to_le_bytes());
        let crc = crc32(&sector[..HEADER_LEN]);
        sector[0x10..0x14].copy_from_slice(&crc.to_le_bytes());

        let error = parse_header(&sector).expect_err("~4 GiB of entries is past the cap");
        assert!(
            matches!(error, Error::CorruptTable { format: "GPT", .. }),
            "{error:?}"
        );
    }

    /// Names come back as text, and the padding after them does not.
    #[test]
    fn a_name_is_decoded_from_utf16_and_stops_at_its_padding() {
        let array = entry(A_TYPE, 64, 127, "rootfs");
        let sector = header_for(&array, 1);
        let header = parse_header(&sector).unwrap();
        let entries = parse_entries(&header, &array).unwrap();

        assert_eq!(entries[0].name, "rootfs");
        assert_eq!(entries[0].name.len(), 6, "the 30 NUL units are not name");
    }

    /// A name is 36 UTF-16 code units, and a table can fill all of them with no
    /// terminator. The decoder must then stop at the end of the field rather than
    /// read into the next entry.
    #[test]
    fn a_name_that_fills_the_field_is_not_read_past_the_end_of_it() {
        let name = "x".repeat(36);
        let array = [
            entry(A_TYPE, 64, 127, &name),
            entry(A_TYPE, 128, 191, "next"),
        ]
        .concat();
        let sector = header_for(&array, 2);
        let header = parse_header(&sector).unwrap();
        let entries = parse_entries(&header, &array).unwrap();

        assert_eq!(entries[0].name, name);
        assert_eq!(entries[1].name, "next");
    }

    /// The array read from the device comes in whole sectors, so it is often longer
    /// than the entries it holds. Three entries are 384 bytes, and no device returns
    /// 384 bytes. The CRC does not cover the surplus, and including it would fail
    /// every real table.
    #[test]
    fn an_array_read_in_whole_sectors_checks_out_against_the_entries_it_holds() {
        let (sector, array) = a_table();
        let header = parse_header(&sector).unwrap();

        let mut padded = array.clone();
        padded.resize(512, 0); // what a one-sector read actually returns

        let entries = parse_entries(&header, &padded).expect("the padding is not table");
        assert_eq!(entries.len(), 2);
    }

    /// An array shorter than the header describes is a read that did not finish.
    /// The parse refuses it rather than parse part of it.
    #[test]
    fn an_array_shorter_than_the_header_describes_is_refused() {
        let (sector, array) = a_table();
        let header = parse_header(&sector).unwrap();

        let error =
            parse_entries(&header, &array[..array.len() - 1]).expect_err("a byte is missing");
        assert!(
            matches!(error, Error::CorruptTable { format: "GPT", .. }),
            "{error:?}"
        );
    }

    /// The rebuild round-trips, and it is faithful. A primary built from a backup
    /// parses as a healthy GPT with the backup's partitions. The names, the sectors,
    /// and the type and unique GUIDs all match. The uniform partition type drops
    /// those GUIDs, so the array is copied verbatim rather than rebuilt from a list.
    /// Only the fields that give this copy's position are new.
    #[test]
    fn a_primary_rebuilt_from_a_backup_parses_as_the_same_table() {
        let (backup_header, backup_array) = a_table();
        let backup_lba = 0x1fff_ffff; // where the backup was found, by geometry

        let rebuilt = rebuild_primary_from_backup(&backup_header, &backup_array, backup_lba, 512)
            .expect("the backup is well-formed");

        assert_eq!(rebuilt.lba, HEADER_LBA);

        // The header parses, and names the primary's own position.
        let header = parse_header(&rebuilt.bytes[..512]).expect("a well-formed primary header");
        assert_eq!(header.current_lba, HEADER_LBA, "this copy is the primary");
        assert_eq!(
            header.backup_lba, backup_lba,
            "its twin is where we found it"
        );
        assert_eq!(header.entry_array_lba, 2, "the array follows the header");

        // Everything else is the backup's, carried across unchanged.
        let backup = parse_header(&backup_header).unwrap();
        assert_eq!(header.disk_guid, backup.disk_guid);
        assert_eq!(header.first_usable_lba, backup.first_usable_lba);
        assert_eq!(header.last_usable_lba, backup.last_usable_lba);
        assert_eq!(header.entry_count, backup.entry_count);
        assert_eq!(header.entry_array_crc, backup.entry_array_crc);

        // The entries parse, and the GUIDs survived -- the point of a byte-faithful
        // copy over a rebuild-from-names.
        let rebuilt_entries =
            parse_entries(&header, &rebuilt.bytes[512..]).expect("a well-formed array");
        let backup_entries = parse_entries(&backup, &backup_array).unwrap();
        assert_eq!(rebuilt_entries, backup_entries, "faithful, GUIDs and all");
        assert_eq!(rebuilt_entries[0].name, "uboot");
        assert_eq!(rebuilt_entries[0].unique_guid, Guid([0xab; 16]));
    }

    /// The array bytes are the backup's, verbatim, for exactly the length the
    /// header declares. Only a byte-for-byte copy carries the GUIDs across. The
    /// surplus a whole-sector read brought is not part of the table.
    #[test]
    fn a_rebuilt_primary_copies_the_backup_array_verbatim() {
        let (backup_header, backup_array) = a_table();
        let rebuilt =
            rebuild_primary_from_backup(&backup_header, &backup_array, 0x1fff_ffff, 512).unwrap();

        let header = parse_header(&backup_header).unwrap();
        let len = header.entry_array_len();
        assert_eq!(&rebuilt.bytes[512..512 + len], &backup_array[..len]);
    }

    /// The standard 16 KiB array (128 entries of 128 bytes, 32 sectors) lands at
    /// the exact-fit boundary. The array runs from sector 2 to sector 33, and the
    /// usable region begins at 34, as on a real board. The rebuilt run is the
    /// header sector and the 32 array sectors, and no more.
    #[test]
    fn a_rebuilt_primary_lays_a_full_size_array_out_in_whole_sectors() {
        let mut array = Vec::new();
        array.extend(entry(A_TYPE, 34, 2047, "uboot"));
        for _ in 1..128 {
            array.extend(vec![0u8; ENTRY_LEN]); // 127 unused slots, to a full 16 KiB
        }
        let backup = header_for(&array, 128); // first_usable is 34

        let rebuilt = rebuild_primary_from_backup(&backup, &array, 0x1fff_ffff, 512).unwrap();

        assert_eq!(
            rebuilt.bytes.len(),
            512 + 32 * 512,
            "header plus 32 array sectors"
        );
        let header = parse_header(&rebuilt.bytes[..512]).unwrap();
        assert_eq!(header.first_usable_lba, 34);
        let entries = parse_entries(&header, &rebuilt.bytes[512..]).unwrap();
        assert_eq!(entries.len(), 1, "the 127 empty slots are not partitions");
    }

    /// No standard primary can be laid out from a backup whose usable region
    /// begins before there is room for a primary array. The rebuild refuses it
    /// rather than write a primary array over the front of the data.
    #[test]
    fn a_backup_with_no_room_for_a_primary_array_is_refused() {
        let (mut backup, array) = a_table();
        backup[0x28..0x30].copy_from_slice(&2u64.to_le_bytes()); // first usable = 2
        let crc = crc32(&backup[..HEADER_LEN]);
        backup[0x10..0x14].copy_from_slice(&crc.to_le_bytes());

        let error = rebuild_primary_from_backup(&backup, &array, 0x1fff_ffff, 512)
            .expect_err("the array will not fit before sector 2");
        assert!(
            matches!(error, Error::CorruptTable { format: "GPT", .. }),
            "{error:?}"
        );
    }

    /// A backup that fails its own CRC checks is not copied into a primary,
    /// because a faithful copy of a corrupt array repairs nothing. The rebuild
    /// reports the damage instead of writing it.
    #[test]
    fn a_rebuild_from_a_corrupt_backup_array_is_refused() {
        let (backup, mut array) = a_table();
        array[0x20] ^= 0xff; // move a partition's first sector; the array CRC breaks

        let error = rebuild_primary_from_backup(&backup, &array, 0x1fff_ffff, 512)
            .expect_err("the backup array's CRC no longer holds");
        assert!(
            matches!(error, Error::CorruptTable { format: "GPT", .. }),
            "{error:?}"
        );
    }

    /// The backup rebuild round-trips, and it is faithful too. A backup built from
    /// a primary parses as a healthy GPT with the primary's partitions and GUIDs.
    /// Its header lands on the device's last sector, and its array in the sectors
    /// immediately before it.
    #[test]
    fn a_backup_rebuilt_from_a_primary_parses_as_the_same_table() {
        let (primary_header, primary_array) = a_table();
        let flash_sectors = 0x2000_0000; // the backup lands on sector 0x1fff_ffff

        let rebuilt =
            rebuild_backup_from_primary(&primary_header, &primary_array, flash_sectors, 512)
                .expect("the primary is well-formed");

        // The run's header sector is its last, and it lands on the device's last
        // sector; the array is the sectors before it.
        let last_lba = flash_sectors - 1;
        let run_sectors = (rebuilt.bytes.len() / 512) as u64;
        assert_eq!(
            rebuilt.lba + run_sectors - 1,
            last_lba,
            "the header is the last sector"
        );

        let header_off = rebuilt.bytes.len() - 512;
        let header =
            parse_header(&rebuilt.bytes[header_off..]).expect("a well-formed backup header");
        assert_eq!(header.current_lba, last_lba, "this copy is the backup");
        assert_eq!(header.backup_lba, HEADER_LBA, "its twin is the primary");
        assert_eq!(
            header.entry_array_lba, rebuilt.lba,
            "and its array is the run's front"
        );

        // Faithful: the same table, GUIDs and all.
        let primary = parse_header(&primary_header).unwrap();
        assert_eq!(header.disk_guid, primary.disk_guid);
        assert_eq!(header.entry_array_crc, primary.entry_array_crc);
        let entries =
            parse_entries(&header, &rebuilt.bytes[..header_off]).expect("a well-formed array");
        let primary_entries = parse_entries(&primary, &primary_array).unwrap();
        assert_eq!(entries, primary_entries, "faithful, GUIDs and all");
    }

    /// A primary whose usable region runs to the end of the device leaves no room
    /// for a backup array before the last sector. The rebuild refuses it rather
    /// than write over the end of the data.
    #[test]
    fn a_primary_with_no_room_for_a_backup_array_is_refused() {
        let (mut primary, array) = a_table();
        let flash_sectors: u64 = 0x2000_0000;
        primary[0x30..0x38].copy_from_slice(&(flash_sectors - 1).to_le_bytes()); // last usable = the last sector
        let crc = crc32(&primary[..HEADER_LEN]);
        primary[0x10..0x14].copy_from_slice(&crc.to_le_bytes());

        let error = rebuild_backup_from_primary(&primary, &array, flash_sectors, 512)
            .expect_err("the usable region runs to the end");
        assert!(
            matches!(error, Error::CorruptTable { format: "GPT", .. }),
            "{error:?}"
        );
    }

    /// A device too small to hold a backup at all is refused before the sector
    /// arithmetic underflows. It has fewer sectors than the array and header need.
    #[test]
    fn a_backup_rebuild_on_a_device_with_no_room_at_all_is_refused() {
        let (primary, array) = a_table();
        let error = rebuild_backup_from_primary(&primary, &array, 2, 512)
            .expect_err("two sectors cannot hold a backup");
        assert!(matches!(error, Error::CorruptTable { .. }), "{error:?}");
    }

    /// The backup rebuild refuses a corrupt primary, as the primary rebuild refuses
    /// a corrupt backup. A faithful copy of damage repairs nothing.
    #[test]
    fn a_backup_rebuild_from_a_corrupt_primary_array_is_refused() {
        let (primary, mut array) = a_table();
        array[0x20] ^= 0xff; // the primary array's CRC breaks

        let error = rebuild_backup_from_primary(&primary, &array, 0x2000_0000, 512)
            .expect_err("the primary array's CRC no longer holds");
        assert!(
            matches!(error, Error::CorruptTable { format: "GPT", .. }),
            "{error:?}"
        );
    }

    // ---- Authoring: parsing GUIDs, the type vocabulary, synthesis, and the table ----

    /// The type constants are the published GUIDs, checked by parsing the canonical
    /// text a specification prints. This pins the constants' mixed-endian byte
    /// arrays against an independent source. A transposed byte in one of them fails
    /// here, instead of giving a partition the wrong type.
    #[test]
    fn the_type_constants_are_the_published_type_guids() {
        assert_eq!(
            Guid::parse("C12A7328-F81F-11D2-BA4B-00A0C93EC93B").unwrap(),
            EFI_SYSTEM
        );
        assert_eq!(
            Guid::parse("0FC63DAF-8483-4772-8E79-3D69D8477DE4").unwrap(),
            LINUX_DATA
        );
        assert_eq!(
            Guid::parse("0657FD6D-A4AB-43C4-84E5-0933C84B4F4F").unwrap(),
            LINUX_SWAP
        );
        // The SDK's genimage.cfg and Makefile print this one.
        assert_eq!(
            Guid::parse("2E54B353-1271-4842-806F-E436D6AF6985").unwrap(),
            JH7110_SPL
        );
    }

    /// `parse` is the exact inverse of `Display`. It applies the same mixed-endian
    /// swizzle in reverse, three groups reversed and two not. A round-trip through
    /// text returns the bytes it started with. A parser that reversed all five
    /// groups, or none, would round-trip its own output and still be wrong. This
    /// test therefore pins it against the canonical text of two published GUIDs.
    #[test]
    fn a_guid_parses_as_the_inverse_of_its_display() {
        for text in [
            "C12A7328-F81F-11D2-BA4B-00A0C93EC93B",
            "0FC63DAF-8483-4772-8E79-3D69D8477DE4",
        ] {
            assert_eq!(Guid::parse(text).unwrap().to_string(), text);
        }

        // Lowercase parses to the same bytes -- a person's layout is not required to
        // shout.
        assert_eq!(
            Guid::parse("c12a7328-f81f-11d2-ba4b-00a0c93ec93b").unwrap(),
            EFI_SYSTEM
        );
    }

    /// A string that is not a GUID is refused, not partly read. The cases are the
    /// wrong number of groups, a group of the wrong length, and a non-hex digit. A
    /// person typed it, so the error quotes it back rather than substituting a
    /// default.
    #[test]
    fn a_string_that_is_not_a_guid_is_refused() {
        let refused = |text: &str| matches!(Guid::parse(text), Err(Error::InvalidRequest(_)));

        assert!(refused("not-a-guid"));
        assert!(refused("C12A7328F81F11D2BA4B00A0C93EC93B"), "no groups");
        assert!(
            refused("C12A7328-F81F-11D2-BA4B-00A0C93EC93"),
            "the last group is a digit short"
        );
        assert!(
            refused("C12A732G-F81F-11D2-BA4B-00A0C93EC93B"),
            "a non-hex digit"
        );
        assert!(
            refused("C12A7328-F81F-11D2-BA4B-00A0C93EC93B-0000"),
            "too many groups"
        );
        assert!(!refused("C12A7328-F81F-11D2-BA4B-00A0C93EC93B"));
    }

    /// The type vocabulary. A small named set matches in any case. A partition with
    /// no type gets a default, and any other token is read as a raw GUID. A person
    /// can therefore name any type, including one the set does not name. The default
    /// is Linux data, not the all-zero type that means "unused".
    #[test]
    fn the_type_vocabulary_maps_names_defaults_and_passes_raw_guids() {
        assert_eq!(type_guid_for(None).unwrap(), LINUX_DATA, "the default");
        assert_eq!(type_guid_for(Some("linux")).unwrap(), LINUX_DATA);
        assert_eq!(type_guid_for(Some("DATA")).unwrap(), LINUX_DATA, "any case");
        assert_eq!(type_guid_for(Some("esp")).unwrap(), EFI_SYSTEM);
        assert_eq!(type_guid_for(Some("efi")).unwrap(), EFI_SYSTEM);
        assert_eq!(type_guid_for(Some("swap")).unwrap(), LINUX_SWAP);
        assert_eq!(type_guid_for(Some("JH7110-SPL")).unwrap(), JH7110_SPL);

        // A raw type GUID is read as itself, so any type is nameable.
        assert_eq!(
            type_guid_for(Some("C12A7328-F81F-11D2-BA4B-00A0C93EC93B")).unwrap(),
            EFI_SYSTEM
        );

        // A token that is neither a known name nor a GUID is refused.
        assert!(matches!(
            type_guid_for(Some("frobnicate")),
            Err(Error::InvalidRequest(_))
        ));
    }

    /// Synthesized GUIDs have four properties:
    ///
    /// - They are deterministic. The same material yields the same GUID, which
    ///   makes authoring idempotent.
    /// - They are domain-separated. A disk GUID and a partition's do not collide
    ///   on identical material.
    /// - Different material yields a different GUID.
    /// - They are well-formed version-4 UUIDs. The version nibble and the RFC-4122
    ///   variant are set at the canonical positions, which display as the third
    ///   and fourth groups.
    #[test]
    fn synthesized_guids_are_deterministic_domain_separated_and_wellformed() {
        let guid = derive_guid(0, b"pyrographer");
        assert_eq!(guid, derive_guid(0, b"pyrographer"), "deterministic");
        assert_ne!(guid, derive_guid(1, b"pyrographer"), "the domain separates");
        assert_ne!(guid, derive_guid(0, b"pyrographe"), "the material matters");

        let text = guid.to_string();
        let groups: Vec<&str> = text.split('-').collect();
        assert_eq!(&groups[2][..1], "4", "version 4: {text}");
        assert!(
            matches!(&groups[3][..1], "8" | "9" | "A" | "B"),
            "an RFC-4122 variant: {text}"
        );
    }

    /// Build a partition for [`author`], with a type and a unique GUID a test can
    /// recognize on the way back out.
    fn authored(name: &str, first_lba: u64, sectors: u64, unique: u8) -> AuthoredPartition {
        AuthoredPartition {
            name: name.to_string(),
            first_lba,
            sectors,
            type_guid: LINUX_DATA,
            unique_guid: Guid([unique; 16]),
        }
    }

    /// A sector too small to hold a protective MBR is refused rather than
    /// panicking on the layout. A DFU gadget's sector size is its advertised
    /// `wTransferSize`, and 64 or 128 bytes is legal there. This case is therefore
    /// reachable from `author-gpt` against a real board, not only from a hand-built
    /// geometry.
    #[test]
    fn authoring_into_a_sector_too_small_for_the_format_is_refused() {
        for sector_size in [1, 64, 92, 256, 511] {
            let error = author(
                &[AuthoredPartition {
                    name: "boot".to_string(),
                    type_guid: Guid([0x11; 16]),
                    unique_guid: Guid([0x22; 16]),
                    first_lba: 64,
                    sectors: 64,
                }],
                Guid([0x33; 16]),
                4096,
                sector_size,
            )
            .expect_err("a GPT cannot be authored into a sector this small");
            assert!(
                matches!(error, Error::InvalidRequest(_)),
                "{sector_size}: {error:?}"
            );
        }
    }

    /// An authored table round-trips in both copies, and holds what it was given.
    /// The primary parses as a healthy GPT at sector 1, with the partitions, names
    /// and both GUIDs it was handed. The backup parses the same at the end of the
    /// device. Sector 0 carries a protective MBR, so a host that reads only sector 0
    /// sees a claimed disk.
    #[test]
    fn an_authored_table_round_trips_both_copies_and_carries_a_protective_mbr() {
        let flash_sectors: u64 = 0x1_0000;
        let disk_guid = Guid([0xcd; 16]);
        let parts = [
            authored("uboot", 64, 8192, 0x11),
            authored("rootfs", 8256, 8192, 0x22),
        ];

        let table = author(&parts, disk_guid, flash_sectors, 512).expect("a well-formed layout");

        // The primary run is [MBR][header][array], from sector 0.
        assert_eq!(table.primary.lba, 0);
        assert_eq!(
            &table.primary.bytes[510..512],
            &[0x55, 0xaa],
            "the MBR signature"
        );
        assert_eq!(
            table.primary.bytes[0x1be + 4],
            0xee,
            "a GPT protective type"
        );
        assert_eq!(table.disk_guid, disk_guid);

        // The header parses and names the standard geometry.
        let header = parse_header(&table.primary.bytes[512..1024]).expect("a well-formed header");
        assert_eq!(header.current_lba, HEADER_LBA);
        assert_eq!(header.backup_lba, flash_sectors - 1);
        assert_eq!(
            header.first_usable_lba, 34,
            "128 entries reserve 34 sectors"
        );
        assert_eq!(header.entry_count, GPT_ENTRIES);
        assert_eq!(header.disk_guid, disk_guid);

        // The entries parse, faithful to the partitions -- ranges, names, and both
        // GUIDs, the 126 empty slots dropped.
        let entries =
            parse_entries(&header, &table.primary.bytes[1024..]).expect("a well-formed array");
        assert_eq!(entries.len(), 2, "the empty slots are not partitions");
        assert_eq!(entries[0].name, "uboot");
        assert_eq!(entries[0].first_lba, 64);
        assert_eq!(entries[0].sectors(), 8192);
        assert_eq!(entries[0].type_guid, LINUX_DATA);
        assert_eq!(entries[0].unique_guid, Guid([0x11; 16]));
        assert_eq!(entries[1].name, "rootfs");
        assert_eq!(entries[1].unique_guid, Guid([0x22; 16]));

        // The backup parses too, from its header on the last sector.
        let header_off = table.backup.bytes.len() - 512;
        let backup_header =
            parse_header(&table.backup.bytes[header_off..]).expect("a well-formed backup header");
        assert_eq!(
            backup_header.current_lba,
            flash_sectors - 1,
            "the backup's own sector"
        );
        assert_eq!(
            backup_header.backup_lba, HEADER_LBA,
            "its twin is the primary"
        );
        let backup_entries = parse_entries(&backup_header, &table.backup.bytes[..header_off])
            .expect("a well-formed array");
        assert_eq!(
            backup_entries, entries,
            "the two copies hold the same partitions"
        );
    }

    /// Authoring refuses a layout that does not fit the geometry, rather than
    /// leaving the board to discover it. The refused layouts include these:
    ///
    /// - A partition in the reserved front
    /// - More partitions than the array holds
    /// - A device too small for a table and its backup
    /// - A name longer than the field
    #[test]
    fn an_authored_table_refuses_a_layout_that_does_not_fit_its_geometry() {
        let refused = |parts: &[AuthoredPartition], flash: u64| {
            matches!(
                author(parts, Guid([1; 16]), flash, 512),
                Err(Error::InvalidRequest(_))
            )
        };

        // Inside the 34 reserved sectors at the front.
        assert!(refused(&[authored("early", 10, 8, 0x11)], 0x1_0000));
        // Ending past the last usable sector, into the backup.
        assert!(refused(&[authored("late", 34, 0x1_0000, 0x11)], 0x1_0000));
        // A device too small for a table and its backup at all.
        assert!(refused(&[authored("boot", 34, 1, 0x11)], 50));
        // A zero-length partition.
        assert!(refused(&[authored("empty", 34, 0, 0x11)], 0x1_0000));
        // A name longer than the 36-unit field.
        assert!(refused(&[authored(&"x".repeat(37), 34, 8, 0x11)], 0x1_0000));

        // And a layout that fits is not refused.
        assert!(!refused(&[authored("boot", 34, 8, 0x11)], 0x1_0000));

        // More partitions than the 128-entry array can hold, count-checked before
        // the per-partition geometry so overlap does not mask it.
        let too_many: Vec<AuthoredPartition> =
            (0..129).map(|_| authored("p", 34, 1, 0x11)).collect();
        assert!(refused(&too_many, 0x1_0000));
    }

    /// On a 512-byte part the usable range is 34 to 34 short of the end, the
    /// numbers Rockchip's own tool writes, and a part too small for two tables has
    /// none.
    #[test]
    fn the_usable_range_leaves_room_for_both_copies() {
        assert_eq!(
            usable_range(0x10000, 512).expect("fits"),
            (34, 0x10000 - 34)
        );
        assert!(usable_range(60, 512).is_err());
        assert!(usable_range(0, 512).is_err());
    }
}
