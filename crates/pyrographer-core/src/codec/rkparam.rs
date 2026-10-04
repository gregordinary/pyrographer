//! Rockchip's `parameter` block, as it is laid out on the flash.
//!
//! It is the table carried by Rockchip boards that predate GPT. It has a `PARM`
//! header, a length, a run of `KEY: value` text, and a CRC. Inside the text, the
//! `CMDLINE` line holds an `mtdparts=` list, which is the partition table.
//!
//! Like [`gpt`](super::gpt), this is a structure the device stores rather than a
//! wire format. It is a sans-I/O codec for the same reasons: it has an endianness
//! and a checksum, and testing it needs no device.
//!
//! # The block
//!
//! ```text
//! offset 0        4 bytes   "PARM"
//! offset 4        u32 LE    the length of the text, and of nothing else
//! offset 8        len       the text, raw, and NOT NUL-terminated
//! offset 8 + len  u32 LE    the CRC of the text, and of nothing else
//! ```
//!
//! Zeros follow, to the end of the sector. **\[DOC\]**
//!
//! That layout holds two traps:
//!
//! - **The CRC is Rockchip's own**, not the standard one. It covers the text
//!   alone: not the header, not itself, and not the padding. [`crc32_rockchip`] is
//!   defined beside the standard one, so that the two are hard to confuse.
//! - **The text is not NUL-terminated.** Its length is the header's, and the four
//!   bytes immediately after it are the CRC. A reader that treated the block as a C
//!   string would run off the end of the text and into the checksum. U-Boot's own
//!   reader does that, on a parameter whose text does not end in a newline. This
//!   module takes the length from the header, and reads exactly that many bytes of
//!   text.
//!
//! # Keys read from the text
//!
//! The text carries a dozen keys, such as `FIRMWARE_VER`, `MACHINE_MODEL` and
//! `CHECK_MASK`. pyrographer reads one of them, `CMDLINE`, because it holds the
//! partition table. The others concern the kernel, and this module does not parse
//! them.
//!
//! The `MAGIC: 0x5041524B` line inside the text is distinct from the `PARM` tag at
//! the start of the block. It is a fixed legacy constant that spells `PARK`, and
//! every board that boots from a device tree ignores it. The two are easy to
//! confuse. **\[DOC\]**

use super::crc::crc32_rockchip;
use crate::{Error, Result};

/// The four bytes a parameter block begins with.
pub const MAGIC: &[u8; 4] = b"PARM";

/// The bytes before the text: the magic, and the text's length.
pub const HEADER_LEN: usize = 8;

/// The bytes after the text: its CRC.
pub const CRC_LEN: usize = 4;

/// The largest parameter text, in bytes, that pyrographer reads.
///
/// The length in the header sets how many sectors are read from the device. A
/// damaged header can give an implausible length, so [`block_len`] refuses one past
/// this cap. Rockchip's specification caps the parameter file at 64 KiB, and
/// U-Boot's reader sizes its buffer at exactly that. **\[DOC\]**
pub const MAX_TEXT_LEN: usize = 64 * 1024;

/// Somewhere a parameter block is written, and what the partition offsets in it
/// are counted from.
///
/// On an eMMC, both are [`EMMC_BASE_LBA`]. On NAND, every copy counts its offsets
/// from sector zero, though only the first copy sits there. [`LOCATIONS`] lists
/// every pairing, and [`parse`] explains where the base matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Location {
    /// The sector that holds the block.
    pub lba: u64,
    /// The first sector of the region that holds the block. The partition offsets
    /// in its text are counted from it.
    pub base_lba: u64,
}

/// The sector that holds an eMMC's parameter block, which is also the base the
/// partition offsets in it are counted from.
///
/// Rockchip reserves the first 4 MiB of an eMMC for what the BootROM and the loader
/// own. It puts the parameter block at the end of that region. **\[DOC\]**
pub const EMMC_BASE_LBA: u64 = 0x2000;

/// Every sector a Rockchip tool writes a parameter block to, in the order
/// pyrographer looks in them.
///
/// The eMMC copy comes first, because a board made this decade has it.
/// rkdeveloptool writes exactly one, at [`EMMC_BASE_LBA`].
///
/// The eight after it are the raw-NAND layout. On NAND, the rknand layer hides the
/// reserved region, so the parameter begins at sector zero. There, rkflashtool
/// writes the block eight times, every `0x400` sectors. A bad block in one copy then
/// does not cost the board its table. [`partition::read`] therefore tries each in
/// turn, rather than giving up on the first that does not parse. Their offsets are
/// counted from zero. **\[COMMUNITY\]**
///
/// [`partition::read`]: crate::partition::read
pub const LOCATIONS: &[Location] = &[
    Location {
        lba: EMMC_BASE_LBA,
        base_lba: EMMC_BASE_LBA,
    },
    Location {
        lba: 0x0000,
        base_lba: 0,
    },
    Location {
        lba: 0x0400,
        base_lba: 0,
    },
    Location {
        lba: 0x0800,
        base_lba: 0,
    },
    Location {
        lba: 0x0c00,
        base_lba: 0,
    },
    Location {
        lba: 0x1000,
        base_lba: 0,
    },
    Location {
        lba: 0x1400,
        base_lba: 0,
    },
    Location {
        lba: 0x1800,
        base_lba: 0,
    },
    Location {
        lba: 0x1c00,
        base_lba: 0,
    },
];

/// The size value that marks the partition that grows to fill the part.
///
/// The text spells it `-`, and both reference tools convert that to this sentinel
/// before resolving it against the size of the part. A text that carries the number
/// literally therefore means to them what a `-` means, and it means the same here.
/// **\[COMMUNITY\]**
const GROW: u64 = 0xffff_ffff;

/// One partition, as an `mtdparts` entry names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partition {
    /// Its name, with any flags stripped off.
    pub name: String,
    /// Its first sector, as an LBA: the offset in the text, plus the base that
    /// offset is counted from. See [`parse`].
    pub first_lba: u64,
    /// How many sectors it occupies, with a `-` resolved against the size of the
    /// part.
    pub sectors: u64,
}

/// A parameter block's partition table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    /// The partitions its `CMDLINE` names, in the order it names them.
    pub partitions: Vec<Partition>,
}

/// Whether `bytes` begins with the parameter magic.
///
/// A caller asks this of one sector before reading the block in full. The magic
/// alone does not identify a block. [`parse`] checks the CRC, which separates a
/// block from a four-byte coincidence.
pub fn has_magic(bytes: &[u8]) -> bool {
    bytes.starts_with(MAGIC)
}

/// How many bytes the whole block occupies, read from its own header.
///
/// That is the header, the text, and the CRC. A caller needs it to read the rest of
/// a block whose first sector it has. The padding to the end of the last sector is
/// not part of the block, and no checksum covers it.
///
/// A block without the magic, too short to hold its length, or declaring more than
/// [`MAX_TEXT_LEN`] bytes of text is an [`Error::CorruptTable`].
pub fn block_len(bytes: &[u8]) -> Result<usize> {
    if !has_magic(bytes) {
        return Err(corrupt("the block does not begin with PARM".to_string()));
    }

    let field: [u8; 4] = bytes
        .get(4..8)
        .and_then(|slice| slice.try_into().ok())
        .ok_or_else(|| corrupt("the block is too short to hold its own length".to_string()))?;
    let text_len = u32::from_le_bytes(field) as usize;

    if text_len > MAX_TEXT_LEN {
        return Err(corrupt(format!(
            "the block declares {text_len} bytes of text, past the {MAX_TEXT_LEN} a parameter is \
             allowed to be"
        )));
    }

    Ok(HEADER_LEN + text_len + CRC_LEN)
}

/// Parse a parameter block into the partitions its `CMDLINE` names.
///
/// `block` is the bytes from the sector [`has_magic`] answered for, at least
/// [`block_len`] of them. `flash_sectors` is the size of the part, against which a
/// partition marked to grow is resolved. `base_lba` is the sector the offsets in
/// the text are counted from, which the rest of this comment describes.
///
/// A block that fails its CRC, or whose `CMDLINE` carries no `mtdparts=` list, is
/// an [`Error::CorruptTable`].
///
/// # Where the offsets are counted from
///
/// **An offset in the text is not always the LBA it names.** This is the one detail
/// in this format that can destroy a board.
///
/// Rockchip reserves the front of an eMMC for what the BootROM and the loader own.
/// That region holds the boot structures, from the GPT and the IDB to the parameter
/// block itself. A legacy parameter's partition offsets are counted from the end of
/// that reserved region, not from sector zero. On raw NAND, the rknand layer hides
/// the region, so there is nothing to skip, and the offsets are counted from zero.
/// U-Boot's reader states the whole rule:
///
/// ```text
/// if (dev_desc->if_type != IF_TYPE_RKNAND)
///         offset = RK_PARAM_OFFSET;   /* 0x2000 */
/// part->start = start + offset;
/// ```
///
/// So a `uboot` written `@0x2000` in the text is at LBA `0x4000` on an eMMC, and at
/// LBA `0x2000` on NAND. A reader that skipped the fixup would report it 4 MiB low.
/// That address is the parameter block the reader had just read from those same
/// sectors. **\[DOC\]**
///
/// U-Boot knows which kind of part it is on, and pyrographer does not:
/// `K_FW_READ_FLASH_INFO` reports geometry, not the type of chip. pyrographer takes
/// the same fact from the one place the difference is visible, **the sector the
/// parameter block was found in**. An eMMC's one copy sits at `0x2000`, the sector
/// its offsets are counted from. NAND's eight copies sit below `0x2000`, and all of
/// them count from `0`. [`LOCATIONS`] pairs each sector with its base, and a caller
/// passes that base here.
///
/// The inference holds because a parameter block is identified by more than its
/// magic. Four bytes reading `PARM` at some sector can be a coincidence. Four bytes
/// reading `PARM`, then a length, then a CRC that agrees with the text between
/// them, cannot. The checksum establishes that the parameter is at that sector, so
/// the sector is a reliable basis for the fixup.
pub fn parse(block: &[u8], base_lba: u64, flash_sectors: u64) -> Result<Param> {
    let text = text(block)?;

    let cmdline = text
        .lines()
        .find_map(|line| line.trim_start().strip_prefix("CMDLINE:"))
        .ok_or_else(|| corrupt("the parameter has no CMDLINE line".to_string()))?;

    // `mtdparts` is one whitespace-delimited token among the kernel's other
    // arguments, and it is not always the last of them.
    let mtdparts = cmdline
        .split_whitespace()
        .find_map(|token| token.strip_prefix("mtdparts="))
        .ok_or_else(|| corrupt("the CMDLINE has no mtdparts= in it".to_string()))?;

    Ok(Param {
        partitions: parse_mtdparts(mtdparts, base_lba, flash_sectors)?,
    })
}

/// The text of a parameter block, once its CRC holds: the `KEY: value` lines.
///
/// `block` is at least [`block_len`] bytes from the sector the magic is in. The
/// checksum is checked before a byte of the text is believed, because it is what
/// makes the block a finding rather than a coincidence. [`parse`] reads the
/// partition list from this text. A firmware package carries its parameter as one
/// of these blocks, and its reader takes the whole text, `TYPE` and `uuid:` lines
/// included.
///
/// A block that fails its CRC, or is shorter than it declares, is an
/// [`Error::CorruptTable`].
pub fn text(block: &[u8]) -> Result<String> {
    let len = block_len(block)?;
    if block.len() < len {
        return Err(corrupt(format!(
            "the block declares {len} bytes and only {} were read",
            block.len()
        )));
    }

    let text = &block[HEADER_LEN..len - CRC_LEN];
    let stored = u32::from_le_bytes([
        block[len - 4],
        block[len - 3],
        block[len - 2],
        block[len - 1],
    ]);

    // Rockchip's CRC, not the standard one, and over the text alone.
    let computed = crc32_rockchip(text);
    if computed != stored {
        return Err(corrupt(format!(
            "the text's CRC is {computed:#010x}, and the block says {stored:#010x}"
        )));
    }

    // Lossy, deliberately. The CRC has already said these are the bytes the tool
    // wrote, so a byte that is not UTF-8 is a byte in some vendor's model name
    // rather than damage -- and refusing a board its partition table over a
    // character in a field pyrographer does not read would be a refusal that
    // protects nobody.
    Ok(String::from_utf8_lossy(text).into_owned())
}

/// Parse an `mtdparts` value into partitions, with offsets counted from `base_lba`.
///
/// The value is the `rk29xxnand:<list>` that follows `mtdparts=` on the command
/// line. This is the list-parsing half of [`parse`]. Authoring a table from an
/// `mtdparts` line a person supplies uses this same function. A supplied line and a
/// board's own text are therefore parsed identically. `base_lba` is the fixup [`parse`]
/// documents, and `flash_sectors` is the size against which a partition marked to
/// grow is resolved.
pub fn parse_mtdparts(mtdparts: &str, base_lba: u64, flash_sectors: u64) -> Result<Vec<Partition>> {
    parse_mtdparts_growing_to(mtdparts, base_lba, flash_sectors, flash_sectors)
}

/// [`parse_mtdparts`], with a partition marked to grow ending at `grow_end` rather
/// than at the end of the part.
///
/// A GPT keeps its backup in the last sectors of the part. The partition an
/// `mtdparts` line marks to grow therefore ends at the GPT's last usable sector,
/// not at the last sector. A GPT authored from a firmware package's parameter passes that
/// sector's successor here. `flash_sectors` still bounds where a partition can
/// begin, and every explicit size is kept as written, for the GPT author to check.
pub fn parse_mtdparts_growing_to(
    mtdparts: &str,
    base_lba: u64,
    flash_sectors: u64,
    grow_end: u64,
) -> Result<Vec<Partition>> {
    // The list is introduced by a storage identifier, which is the literal
    // `rk29xxnand` on every Rockchip part ever made -- including the ones that
    // are not NAND, and including the ones that are not RK29xx. It carries no
    // information, both reference tools skip past it without looking, and so
    // does this.
    let (_id, list) = mtdparts.split_once(':').ok_or_else(|| {
        corrupt(format!(
            "the mtdparts '{mtdparts}' names no storage identifier, so it has no partition list \
             behind one"
        ))
    })?;

    list.split(',')
        .map(|spec| parse_spec(spec, base_lba, flash_sectors, grow_end))
        .collect()
}

/// Parse one `<size>@<offset>(<name>)` out of an `mtdparts` list.
///
/// **The size comes first and the offset second**, the reverse of the order most
/// readers expect. Both count 512-byte sectors, not bytes. A `-` for the size means
/// the partition takes whatever is left of the part. **\[DOC\]**
fn parse_spec(spec: &str, base_lba: u64, flash_sectors: u64, grow_end: u64) -> Result<Partition> {
    let malformed = || {
        corrupt(format!(
            "'{spec}' is not a partition: an mtdparts entry is <size>@<offset>(<name>), the size \
             first, and both of them hexadecimal sectors"
        ))
    };

    let (size, rest) = spec.split_once('@').ok_or_else(malformed)?;
    let (offset, name) = rest.split_once('(').ok_or_else(malformed)?;
    let name = name.strip_suffix(')').ok_or_else(malformed)?;

    // A name may carry flags behind a colon -- `grow`, `bootable` -- and they are
    // not part of it. rkdeveloptool keeps them, with the consequence that its own
    // lookup cannot find a partition written `(userdata:grow)` under the name
    // `userdata`. That is a bug rather than a convention, so the flags come off.
    let (name, _flags) = name.split_once(':').unwrap_or((name, ""));

    let offset = hex_sectors(offset, spec)?;
    let first_lba = base_lba.checked_add(offset).ok_or_else(|| {
        corrupt(format!(
            "'{spec}' begins at sector {offset}, which overflows the sector count when added to \
             the base {base_lba}"
        ))
    })?;

    // A partition beginning at or past the end of the part is not a short
    // partition, it is a table that has been read wrong -- and on this format, a
    // base got wrong is precisely the shape that mistake takes. Refusing it also
    // makes the subtraction below total.
    if first_lba >= flash_sectors {
        return Err(corrupt(format!(
            "'{name}' begins at sector {first_lba}, and the device has {flash_sectors} sectors"
        )));
    }

    // A partition that grows takes the rest of the part, up to `grow_end`. One
    // that begins at or past that end has nothing to grow into.
    let grows = || {
        grow_end
            .checked_sub(first_lba)
            .filter(|&sectors| sectors > 0)
            .ok_or_else(|| {
                corrupt(format!(
                    "'{name}' grows to fill the part from sector {first_lba}, and the part ends \
                     at sector {grow_end}"
                ))
            })
    };
    let sectors = match size {
        "-" => grows()?,
        _ => match hex_sectors(size, spec)? {
            GROW => grows()?,
            n => n,
        },
    };

    Ok(Partition {
        name: name.to_string(),
        first_lba,
        sectors,
    })
}

/// Read one hexadecimal sector count or offset from an `mtdparts` entry.
///
/// These are always written with the `0x` prefix, and rkdeveloptool's parser
/// requires it. U-Boot's parser does not, and U-Boot is the one that reads these
/// from a flash. A bare number is therefore read as hexadecimal, as U-Boot reads
/// it, rather than refused.
fn hex_sectors(field: &str, spec: &str) -> Result<u64> {
    let digits = field
        .strip_prefix("0x")
        .or_else(|| field.strip_prefix("0X"))
        .unwrap_or(field);

    u64::from_str_radix(digits, 16).map_err(|_| {
        corrupt(format!(
            "'{field}' in '{spec}' is not a hexadecimal number of sectors"
        ))
    })
}

/// Frame parameter `text` into a block: the magic, the length of the text, the
/// text itself, and Rockchip's CRC of it.
///
/// It is the inverse of [`parse`], and it writes exactly the four fields [`parse`]
/// reads back, in the same order. The result is the block as the flash stores it,
/// without the padding to the end of the sector. The write layer adds that padding.
/// No checksum covers it, so it is not part of the block.
///
/// **The CRC is Rockchip's own**, [`crc32_rockchip`], not the standard one. The
/// board's loader does not read back a block checksummed the standard way. A table
/// written with the wrong CRC is therefore rejected by the board it was written
/// to. Rockchip's CRC is the only one this function writes.
///
/// The text is written verbatim with the header's length, and is not
/// NUL-terminated. Framing the text of a block that [`parse`] read therefore
/// reproduces that block, other keys included. A parameter can then be authored
/// from an existing block's text, keeping `FIRMWARE_VER` and the other keys a
/// partition list does not carry.
///
/// A text longer than [`MAX_TEXT_LEN`] is an [`Error::InvalidRequest`]. It exceeds
/// what a parameter is allowed to be, and what the board's own reader reads.
pub fn frame(text: &str) -> Result<Vec<u8>> {
    let text = text.as_bytes();
    if text.len() > MAX_TEXT_LEN {
        return Err(Error::InvalidRequest(format!(
            "the parameter text is {} bytes, past the {MAX_TEXT_LEN} a parameter is allowed to be",
            text.len()
        )));
    }

    let mut block = Vec::with_capacity(HEADER_LEN + text.len() + CRC_LEN);
    block.extend_from_slice(MAGIC);
    block.extend_from_slice(&(text.len() as u32).to_le_bytes());
    block.extend_from_slice(text);
    block.extend_from_slice(&crc32_rockchip(text).to_le_bytes());
    Ok(block)
}

/// Render an `mtdparts` partition list from a layout, with offsets counted from
/// `base_lba`.
///
/// It is the inverse of the per-entry parsing [`parse`] does. Each partition
/// becomes `<size>@<offset>(<name>)`, size first, both in hexadecimal sectors,
/// exactly the shape [`parse`] reads. The offset written is the partition's
/// absolute first sector minus the base its offsets are counted from, which undoes
/// the fixup [`parse`] applies. A layout in absolute LBAs, rendered against the base
/// it will be read at, therefore round-trips to the same LBAs.
///
/// A partition beginning before the base cannot be expressed against it, because
/// its offset is negative. On an eMMC, that is a partition inside the reserved first
/// 4 MiB. It is an [`Error::InvalidRequest`]. A wrapping subtraction would give an
/// offset of nearly 2^64 sectors, past the end of any part.
///
/// The storage identifier is the literal `rk29xxnand` every Rockchip tool writes
/// and none reads. A `-` size is never synthesized. Authoring writes explicit
/// sizes, so the block states what it lays down, and no partition grows into
/// whatever the reader decides is left. A name that is empty, or that contains one
/// of the `,@():` delimiters, cannot be read back as one partition and is refused.
pub fn render_mtdparts(partitions: &[Partition], base_lba: u64) -> Result<String> {
    let mut list = String::from("rk29xxnand:");

    for (index, part) in partitions.iter().enumerate() {
        if index > 0 {
            list.push(',');
        }

        if part.name.is_empty() || part.name.contains([',', '@', '(', ')', ':']) {
            return Err(Error::InvalidRequest(format!(
                "'{}' is not a name an mtdparts entry can carry: a name is non-empty and free of \
                 the ',@():' the format delimits with",
                part.name
            )));
        }

        let offset = part.first_lba.checked_sub(base_lba).ok_or_else(|| {
            Error::InvalidRequest(format!(
                "'{}' begins at sector {}, before the base {base_lba} its offset is counted from",
                part.name, part.first_lba
            ))
        })?;

        list.push_str(&format!(
            "0x{:x}@0x{:x}({})",
            part.sectors, offset, part.name
        ));
    }

    Ok(list)
}

/// The [`Error::CorruptTable`] for a parameter block that fails one of this
/// module's checks.
fn corrupt(detail: String) -> Error {
    Error::CorruptTable {
        format: "Rockchip parameter",
        detail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `CMDLINE` of a real Rockchip parameter, in the legacy form. Sizes and
    /// offsets are in hexadecimal sectors with the size first, and the last
    /// partition grows into whatever is left of the part.
    const CMDLINE: &str = "CMDLINE: console=ttyFIQ0 root=/dev/mmcblk0p6 \
                           mtdparts=rk29xxnand:0x00002000@0x00002000(uboot),\
                           0x00002000@0x00004000(trust),0x00002000@0x00006000(misc),\
                           0x00010000@0x00008000(boot:bootable),-@0x00018000(rootfs:grow)";

    /// The size of the part these tests plan against: 122,142,720 sectors of 512
    /// bytes, about 58.2 GiB. That is the capacity of a 64 GB-class eMMC.
    const FLASH_SECTORS: u64 = 122_142_720;

    /// Build a parameter block the way a device holds one: the header, the text,
    /// and Rockchip's CRC of the text. Padding follows to the end of the sector. It
    /// is not part of the block, and no checksum covers it.
    fn block(text: &str) -> Vec<u8> {
        let text = text.as_bytes();

        let mut block = Vec::new();
        block.extend_from_slice(MAGIC);
        block.extend_from_slice(&(text.len() as u32).to_le_bytes());
        block.extend_from_slice(text);
        block.extend_from_slice(&crc32_rockchip(text).to_le_bytes());
        block.resize(block.len().next_multiple_of(512), 0);
        block
    }

    /// A whole parameter, as a board carries one.
    fn a_parameter() -> Vec<u8> {
        block(&format!(
            "FIRMWARE_VER: 8.1\n\
             MACHINE_MODEL: RK3399\n\
             MAGIC: 0x5041524B\n\
             CHECK_MASK: 0x80\n\
             {CMDLINE}\n"
        ))
    }

    /// The block, parsed end to end from bytes laid out as a device holds them. It
    /// is on an eMMC, so every offset in the text has the base added to it.
    #[test]
    fn a_parameter_parses_into_the_partitions_its_cmdline_names() {
        let param = parse(&a_parameter(), EMMC_BASE_LBA, FLASH_SECTORS).expect("a good parameter");

        let names: Vec<&str> = param.partitions.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["uboot", "trust", "misc", "boot", "rootfs"]);

        // `uboot` is `0x2000@0x2000` in the text, and this is an eMMC: so it is
        // 0x2000 sectors long, and it begins at 0x2000 past the base.
        let uboot = &param.partitions[0];
        assert_eq!(uboot.first_lba, 0x4000);
        assert_eq!(uboot.sectors, 0x2000);
    }

    /// **The fixup.** The same text, read as an eMMC and as raw NAND, puts every
    /// partition 4 MiB apart. Between the two readings, a write lands on the
    /// parameter block instead of on the bootloader. Nothing in the text says which
    /// reading applies. The sector the block was found in decides it, and this test
    /// pins that.
    #[test]
    fn the_offsets_are_counted_from_the_base_the_block_was_found_at() {
        let param = a_parameter();

        let emmc = parse(&param, EMMC_BASE_LBA, FLASH_SECTORS).unwrap();
        let nand = parse(&param, 0, FLASH_SECTORS).unwrap();

        assert_eq!(nand.partitions[0].first_lba, 0x2000, "as the text says");
        assert_eq!(
            emmc.partitions[0].first_lba,
            0x2000 + EMMC_BASE_LBA,
            "and 4 MiB further on, which is where it actually is"
        );

        // Every partition moves, not merely the first -- and none of them changes
        // its name or its length in the process.
        for (emmc, nand) in emmc.partitions.iter().zip(nand.partitions.iter()) {
            assert_eq!(emmc.name, nand.name);
            assert_eq!(emmc.first_lba - nand.first_lba, EMMC_BASE_LBA);
        }
    }

    /// The last partition takes whatever is left of the part, and how much that is
    /// depends on the size of the part. The size of the flash is therefore an
    /// argument here rather than a guess.
    #[test]
    fn a_partition_marked_to_grow_takes_the_rest_of_the_part() {
        let param = parse(&a_parameter(), EMMC_BASE_LBA, FLASH_SECTORS).unwrap();

        let rootfs = param.partitions.last().expect("the grown one");
        assert_eq!(
            rootfs.name, "rootfs",
            "and its flag is not part of its name"
        );
        assert_eq!(rootfs.first_lba, 0x18000 + EMMC_BASE_LBA);
        assert_eq!(
            rootfs.first_lba + rootfs.sectors,
            FLASH_SECTORS,
            "to the last sector there is"
        );

        // A smaller part, and it grows less.
        let small = parse(&a_parameter(), EMMC_BASE_LBA, 0x30000).unwrap();
        let rootfs = small.partitions.last().unwrap();
        assert_eq!(rootfs.sectors, 0x30000 - (0x18000 + EMMC_BASE_LBA));
    }

    /// Flags follow a colon in the name and are not part of it. rkdeveloptool keeps
    /// them, and then cannot find `rootfs` in a table that spells it `rootfs:grow`.
    /// This parser strips them.
    #[test]
    fn a_name_does_not_carry_its_flags() {
        let param = parse(&a_parameter(), EMMC_BASE_LBA, FLASH_SECTORS).unwrap();

        assert!(param.partitions.iter().any(|p| p.name == "boot"));
        assert!(param.partitions.iter().any(|p| p.name == "rootfs"));
        assert!(
            !param.partitions.iter().any(|p| p.name.contains(':')),
            "no name kept a flag: {:?}",
            param.partitions
        );
    }

    /// The CRC separates a block from a coincidence. A block whose text fails its
    /// checksum is no basis for planning a write, and it is the state a
    /// half-finished write leaves.
    #[test]
    fn a_block_whose_crc_disagrees_with_its_text_is_corrupt() {
        let mut param = a_parameter();
        assert!(has_magic(&param), "the magic is still there");

        param[HEADER_LEN + 2] ^= 0xff; // one byte of the text

        let error = parse(&param, EMMC_BASE_LBA, FLASH_SECTORS).expect_err("the CRC covers it");
        assert!(
            matches!(
                error,
                Error::CorruptTable {
                    format: "Rockchip parameter",
                    ..
                }
            ),
            "{error:?}"
        );
    }

    /// **The CRC mix-up that silently ruins a board.** A parameter is checked with
    /// Rockchip's CRC and not the standard one, and the two differ by a single bit
    /// of a single constant. A block checksummed the standard way must be refused.
    /// Otherwise pyrographer could one day write one that way, and the board's own
    /// loader would reject it.
    #[test]
    fn a_block_checksummed_with_the_standard_crc_is_not_accepted() {
        let text = "CMDLINE: mtdparts=rk29xxnand:0x100@0x200(boot)\n";

        let mut param = Vec::new();
        param.extend_from_slice(MAGIC);
        param.extend_from_slice(&(text.len() as u32).to_le_bytes());
        param.extend_from_slice(text.as_bytes());
        // The wrong CRC: the one the GPT beside this uses.
        param.extend_from_slice(&super::super::crc::crc32(text.as_bytes()).to_le_bytes());

        let error = parse(&param, 0, FLASH_SECTORS).expect_err("that is the other CRC");
        assert!(matches!(error, Error::CorruptTable { .. }), "{error:?}");

        // And with Rockchip's, the very same text is fine -- so what was refused
        // is the checksum, and not the parameter.
        parse(&block(text), 0, FLASH_SECTORS).expect("Rockchip's CRC over the same bytes");
    }

    /// The text is not NUL-terminated: its length is the header's, and the four
    /// bytes after it are the checksum. A reader looking for a terminator would read
    /// the CRC as text. U-Boot's own reader does that, on a parameter whose text does
    /// not end in a newline.
    #[test]
    fn the_text_is_measured_by_the_header_and_not_by_a_terminator() {
        // No trailing newline, so the CRC's bytes sit flush against the last
        // character of the mtdparts list.
        let text = "CMDLINE: mtdparts=rk29xxnand:0x100@0x200(boot)";
        let param = parse(&block(text), 0, FLASH_SECTORS).expect("a parameter with no newline");

        assert_eq!(param.partitions.len(), 1);
        assert_eq!(param.partitions[0].name, "boot");
        assert_eq!(param.partitions[0].sectors, 0x100);
    }

    /// The size comes before the offset, the reverse of the order most readers
    /// expect. A parser with the two swapped produces partitions that are
    /// plausible, wrongly placed and wrongly sized.
    #[test]
    fn the_size_comes_before_the_offset() {
        let param = parse(
            &block("CMDLINE: mtdparts=rk29xxnand:0x00000200@0x00001000(boot)"),
            0,
            FLASH_SECTORS,
        )
        .unwrap();

        assert_eq!(param.partitions[0].first_lba, 0x1000, "the second field");
        assert_eq!(param.partitions[0].sectors, 0x200, "and the first");
    }

    /// `mtdparts` is one token among the kernel's arguments, and it need not be
    /// the last of them.
    #[test]
    fn mtdparts_is_found_wherever_it_sits_on_the_command_line() {
        let param = parse(
            &block(
                "CMDLINE: earlycon=uart8250 mtdparts=rk29xxnand:0x100@0x200(boot) \
                 root=/dev/mmcblk0p1 rw",
            ),
            0,
            FLASH_SECTORS,
        )
        .unwrap();

        assert_eq!(param.partitions[0].name, "boot");
    }

    /// A block with no magic is not a parameter. A caller detects that with a
    /// yes-or-no question rather than by catching an error, as it does for a GPT.
    #[test]
    fn a_block_with_no_magic_says_so_rather_than_failing() {
        assert!(!has_magic(&[0u8; 512]));
        assert!(!has_magic(b"EFI PART"));
        assert!(!has_magic(&[]));
        assert!(has_magic(&a_parameter()));
    }

    /// The length in the header sizes the read from the device, so a header naming
    /// an absurd length is refused before anything is read.
    #[test]
    fn a_length_past_what_a_parameter_may_be_is_refused() {
        let mut damaged = a_parameter();
        damaged[4..8].copy_from_slice(&(MAX_TEXT_LEN as u32 + 1).to_le_bytes());
        assert!(matches!(
            block_len(&damaged),
            Err(Error::CorruptTable { .. })
        ));

        // And a block's true length is the header, the text, and the CRC -- not
        // the padding out to the sector, which is covered by nothing.
        let param = a_parameter();
        let text_len = u32::from_le_bytes([param[4], param[5], param[6], param[7]]) as usize;
        assert_eq!(block_len(&param).unwrap(), HEADER_LEN + text_len + CRC_LEN);
        assert!(block_len(&param).unwrap() < param.len(), "there is padding");
    }

    /// A partition beginning past the end of the part means the table was read
    /// wrong. On this format, a wrong base produces exactly that result.
    #[test]
    fn a_partition_beginning_past_the_end_of_the_part_is_refused() {
        let param = block("CMDLINE: mtdparts=rk29xxnand:0x100@0x40000(boot)");

        // On a part with 0x40000 sectors, that partition begins one sector past
        // the end of it.
        let error = parse(&param, 0, 0x40000).expect_err("there is no sector 0x40000");
        assert!(matches!(error, Error::CorruptTable { .. }), "{error:?}");

        // One sector larger, and it is a partition.
        parse(&param, 0, 0x40001).expect("its first sector exists");
    }

    /// The parameter's text is checksummed, so a byte in it that is not UTF-8 is
    /// one the vendor wrote. It is in a field pyrographer does not read. Refusing
    /// the board its partition table over that byte would protect nobody.
    #[test]
    fn a_byte_of_the_text_that_is_not_utf8_does_not_cost_the_board_its_table() {
        let text =
            b"MACHINE_MODEL: caf\xe9\nCMDLINE: mtdparts=rk29xxnand:0x100@0x200(boot)\n".to_vec();

        let mut param = Vec::new();
        param.extend_from_slice(MAGIC);
        param.extend_from_slice(&(text.len() as u32).to_le_bytes());
        param.extend_from_slice(&text);
        param.extend_from_slice(&crc32_rockchip(&text).to_le_bytes());

        let param = parse(&param, 0, FLASH_SECTORS).expect("the CMDLINE is still ASCII");
        assert_eq!(param.partitions[0].name, "boot");
    }

    /// Malformed entries are refused, entry by entry, rather than skipped. A
    /// partition list that half-parsed would be a table with a hole in it. No
    /// caller could tell that from a table with fewer partitions.
    #[test]
    fn an_entry_that_is_not_a_partition_is_refused_rather_than_skipped() {
        let refuses = |list: &str| {
            let text = format!("CMDLINE: mtdparts=rk29xxnand:{list}");
            matches!(
                parse(&block(&text), 0, FLASH_SECTORS),
                Err(Error::CorruptTable { .. })
            )
        };

        assert!(refuses("0x100(boot)"), "no offset");
        assert!(refuses("0x100@0x200"), "no name");
        assert!(refuses("0x100@0x200(boot"), "unclosed");
        assert!(refuses("0x100@zzz(boot)"), "not a number");
        assert!(
            refuses("0x100@0x200(boot),junk"),
            "and the second entry too"
        );
        assert!(!refuses("0x100@0x200(boot)"), "this one is a partition");
    }

    /// A parameter with no partition list in it is not a partition table, and
    /// answering "no partitions" would hide that behind an empty list.
    #[test]
    fn a_parameter_with_no_partition_list_is_not_a_partition_table() {
        let error = parse(&block("FIRMWARE_VER: 8.1\n"), 0, FLASH_SECTORS)
            .expect_err("there is no CMDLINE");
        assert!(matches!(error, Error::CorruptTable { .. }), "{error:?}");

        let error = parse(&block("CMDLINE: console=ttyFIQ0\n"), 0, FLASH_SECTORS)
            .expect_err("there is no mtdparts");
        assert!(matches!(error, Error::CorruptTable { .. }), "{error:?}");
    }

    /// Framing is the inverse of parsing. A layout rendered to `mtdparts`, wrapped
    /// in a `CMDLINE` and framed into a block parses back into the layout it
    /// started from. It is parsed at the base it was rendered against, so parsing
    /// reapplies the fixup that rendering removed.
    #[test]
    fn a_layout_framed_into_a_block_parses_back_into_the_same_layout() {
        let layout = vec![
            Partition {
                name: "uboot".to_string(),
                first_lba: 0x4000,
                sectors: 0x2000,
            },
            Partition {
                name: "trust".to_string(),
                first_lba: 0x6000,
                sectors: 0x2000,
            },
        ];

        let mtdparts = render_mtdparts(&layout, EMMC_BASE_LBA).expect("a renderable layout");
        let block = frame(&format!("CMDLINE: mtdparts={mtdparts}\n")).expect("a framable text");

        let parsed = parse(&block, EMMC_BASE_LBA, FLASH_SECTORS).expect("a well-formed block");
        assert_eq!(parsed.partitions, layout);
    }

    /// Framing text is byte-faithful. Framing the text of a block that [`parse`]
    /// read reproduces that block. The padding is the one difference, and it is not
    /// part of the block. This carries a parameter's other keys through authoring
    /// from existing text without adding or dropping a byte.
    #[test]
    fn framing_text_reproduces_the_block_it_was_read_from() {
        let text = "FIRMWARE_VER: 8.1\n\
                    MACHINE_MODEL: RK3399\n\
                    CMDLINE: mtdparts=rk29xxnand:0x100@0x200(boot)\n";

        let framed = frame(text).expect("a framable text");
        let via_helper = block(text); // the same block, padded to the sector

        assert_eq!(
            framed,
            &via_helper[..framed.len()],
            "the block, byte for byte"
        );
        assert!(
            via_helper[framed.len()..].iter().all(|&b| b == 0),
            "and the rest of the sector is padding, which frame does not write"
        );

        // And it is Rockchip's CRC that went in, so the very block round-trips.
        let parsed = parse(&framed, 0, FLASH_SECTORS).expect("Rockchip's CRC, framed");
        assert_eq!(parsed.partitions[0].name, "boot");
    }

    /// A text longer than a parameter is allowed to be is refused rather than
    /// framed. The board's own reader does not read that much back, so writing it
    /// would lay down a block the loader cannot parse.
    #[test]
    fn a_text_past_the_cap_is_not_framed() {
        let too_long = "x".repeat(MAX_TEXT_LEN + 1);
        assert!(matches!(frame(&too_long), Err(Error::InvalidRequest(_))));

        // Exactly at the cap is framable, so the bound is a bound and not a
        // blanket refusal.
        frame(&"x".repeat(MAX_TEXT_LEN)).expect("the cap itself is allowed");
    }

    /// A partition beginning before the base cannot be expressed against it. It is
    /// refused rather than rendered as a wrapped offset of nearly 2^64 sectors,
    /// past the end of any part.
    #[test]
    fn a_partition_before_the_base_is_not_rendered() {
        let layout = vec![Partition {
            name: "uboot".to_string(),
            first_lba: 0x1000, // below the eMMC base
            sectors: 0x2000,
        }];
        assert!(matches!(
            render_mtdparts(&layout, EMMC_BASE_LBA),
            Err(Error::InvalidRequest(_))
        ));
    }

    /// A name carrying one of the format's delimiters cannot be read back as one
    /// partition. It is refused rather than rendered into a block that would parse
    /// as something else.
    #[test]
    fn a_name_carrying_a_delimiter_is_not_rendered() {
        let refuses = |name: &str| {
            let layout = vec![Partition {
                name: name.to_string(),
                first_lba: 0x2000,
                sectors: 0x100,
            }];
            matches!(render_mtdparts(&layout, 0), Err(Error::InvalidRequest(_)))
        };

        assert!(refuses(""), "an empty name");
        assert!(refuses("boot,trust"), "a comma");
        assert!(refuses("boot@0"), "an at-sign");
        assert!(refuses("boot(a)"), "a paren");
        assert!(refuses("rootfs:grow"), "a colon");
        assert!(!refuses("rootfs"), "a plain name renders");
    }

    /// The eMMC copy is checked first, because a board made this decade has it.
    /// Every copy after it is the raw-NAND layout, whose offsets are counted from
    /// zero. Pairing each sector with its base is how pyrographer tells the two
    /// layouts apart.
    #[test]
    fn the_locations_pair_each_copy_with_the_base_its_offsets_are_counted_from() {
        assert_eq!(LOCATIONS[0].lba, EMMC_BASE_LBA);
        assert_eq!(LOCATIONS[0].base_lba, EMMC_BASE_LBA);

        let nand = &LOCATIONS[1..];
        assert_eq!(nand.len(), 8, "rkflashtool writes the block eight times");
        assert!(
            nand.iter().all(|location| location.base_lba == 0),
            "raw NAND counts from zero: {nand:?}"
        );

        // Every 0x400 sectors from zero, filling the region below the eMMC base.
        for (index, location) in nand.iter().enumerate() {
            assert_eq!(location.lba, index as u64 * 0x400);
            assert!(location.lba < EMMC_BASE_LBA);
        }
    }

    /// The text comes back whole, every key in it, once the CRC holds, and a block
    /// whose CRC fails yields no text at all.
    #[test]
    fn text_returns_every_line_once_the_crc_holds() {
        let source = "TYPE: GPT\nCMDLINE: mtdparts=rk29xxnand:0x2000@0x4000(uboot)\nuuid:rootfs=614e0000-0000-4b53-8000-1d28000054a9\n";
        let block = frame(source).expect("frames");
        assert_eq!(text(&block).expect("intact"), source);

        let mut damaged = block.clone();
        damaged[HEADER_LEN] ^= 1;
        assert!(matches!(text(&damaged), Err(Error::CorruptTable { .. })));
    }

    /// A growing partition ends where the caller says, and an explicit size is
    /// kept as written. A GPT's backup holds the last sectors, so a GPT passes its
    /// last usable sector's successor here.
    #[test]
    fn a_growing_partition_ends_where_it_is_told() {
        let parts = parse_mtdparts_growing_to(
            "rk29xxnand:0x2000@0x4000(uboot),-@0x6000(userdata:grow)",
            0,
            0x10000,
            0x10000 - 33,
        )
        .expect("parses");
        assert_eq!(parts[0].sectors, 0x2000, "an explicit size is kept");
        assert_eq!(parts[1].name, "userdata");
        assert_eq!(parts[1].first_lba + parts[1].sectors, 0x10000 - 33);

        let past = parse_mtdparts_growing_to("rk29xxnand:-@0x9000(userdata)", 0, 0x10000, 0x8000);
        assert!(matches!(past, Err(Error::CorruptTable { .. })), "{past:?}");
    }
}
