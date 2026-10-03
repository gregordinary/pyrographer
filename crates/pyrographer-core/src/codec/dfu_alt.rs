//! The DFU alt-setting partition source: a device's own `dfu_alt_info`, read
//! back as a name -> range map.
//!
//! An Ingenic board running a DFU-capable U-Boot exposes each partition as a DFU
//! *alt-setting* on its DFU interface. It names each one with an interface string
//! descriptor. That list is a third partition source, beside the on-flash UEFI
//! [`gpt`](super::gpt) and Rockchip [`rkparam`](super::rkparam) blocks. It is the
//! simplest of the three. There is no on-flash table to parse and no checksum to
//! verify, and the device is authoritative about what it exposes.
//!
//! This codec turns one alt-setting's descriptor string into an [`AltSetting`]. It
//! also owns the packing that lets a set of them share the one flat LBA space the
//! verbs address.
//!
//! # Names and ranges
//!
//! What the alt-setting string carries is **\[UNVERIFIED\]** until an Ingenic board
//! is on the bench. U-Boot most often sets it to the bare partition name, such as
//! `uboot`, `kernel` or `rootfs`, but some builds carry a size.
//! [`AltSetting::size`] is therefore an [`Option`]. A string that exposes a byte
//! length gives `Some`, and one that does not gives `None`, the *names-only* case.
//!
//! The parser recognizes U-Boot's `raw <addr> <len>` form as the one plausible
//! size-bearing shape, and reads a bare name otherwise. A board that names its
//! ranges some other way needs a change to [`parse_alt`] and nothing else.
//!
//! # Packing alt-settings into one LBA space
//!
//! DFU has no device-wide address. A transfer addresses a block number within
//! whichever alt-setting is selected, from block zero. The verbs address one flat
//! 64-bit LBA. Each alt-setting therefore gets its own *slice* of that LBA space.
//! Alt-setting `i` owns `[i << `[`ALT_LBA_SHIFT`]`, (i+1) << `[`ALT_LBA_SHIFT`]`)`,
//! and [`split_lba`] unpacks an LBA back into `(alt-setting, block offset)`.
//!
//! The packing is an addressing convenience. **It does not mean the alt-settings
//! are adjacent on the flash.** Laying them end to end at their real sizes would
//! make a concatenated device image. That layout needs every size up front, and
//! this packing does not assume the sizes are known. The slice is far larger than
//! any flash region, so a real read never runs past one alt-setting's slice into
//! the next. The packing is **\[UNVERIFIED\]**, as the rest of the DFU flash path
//! is.

use crate::{Error, Result};

/// The width, in bits, of the block-offset half of a DFU LBA.
///
/// An LBA splits into an alt-setting index in its high bits, and a block offset
/// within that alt-setting in its low [`ALT_LBA_SHIFT`] bits. At 40 bits, each
/// alt-setting owns a slice of 2^40 sectors. That is more than any flash a DFU
/// board exposes, whatever its transfer size, so a read never crosses out of one
/// alt-setting's slice. A `SET_INTERFACE` alternate is one byte, so the index is at
/// most 255. The packed LBA then fits in 48 bits, well within the 64 bits that
/// carry it.
pub const ALT_LBA_SHIFT: u32 = 40;

/// The mask selecting the block-offset half of a packed LBA.
const OFFSET_MASK: u64 = (1u64 << ALT_LBA_SHIFT) - 1;

/// One past the last LBA this packing can name.
///
/// A `SET_INTERFACE` alternate is one byte, so alt-setting 255 is the last one a
/// device can have. The end of its slice ends the address space. A write plan asks
/// for this through [`FlashAgent::address_ceiling`]. A range that runs past it
/// names no alt-setting, and reaches nothing on the device.
///
/// [`FlashAgent::address_ceiling`]: crate::agent::FlashAgent::address_ceiling
pub const PAST_LAST_LBA: u64 = 256u64 << ALT_LBA_SHIFT;

/// The base LBA of alt-setting `index`: the first sector of its slice.
///
/// It is the inverse of the alt-setting half of [`split_lba`]. A partition table
/// built from a set of alt-settings gives each partition this as its `first_lba`.
/// A verb that aims a read at the partition therefore lands at the alt-setting's
/// block zero.
pub fn alt_base_lba(index: u8) -> u64 {
    (u64::from(index)) << ALT_LBA_SHIFT
}

/// Unpack an LBA into the alt-setting it names and the block offset within it.
///
/// The alt-setting is deliberately returned as a [`u64`], not a [`u8`]. An LBA
/// whose high bits name an alt-setting past 255 is out of range. Truncating it to a
/// byte would fold it back onto a real alt-setting. The caller checks the value
/// against the alt-settings it has, and refuses one that is too large.
pub fn split_lba(lba: u64) -> (u64, u64) {
    (lba >> ALT_LBA_SHIFT, lba & OFFSET_MASK)
}

/// One DFU alt-setting: a partition the device exposes over DFU.
///
/// It has three parts:
///
/// - [`index`](AltSetting::index) is the alternate-setting number a
///   `SET_INTERFACE` selects.
/// - [`name`](AltSetting::name) is what the device's interface string descriptor
///   calls it.
/// - [`size`](AltSetting::size) is the region's byte length, or `None` for a
///   string that named the partition without a range.
///
/// The module documentation explains why the size is optional.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AltSetting {
    /// The alternate-setting number, as a `SET_INTERFACE` addresses it.
    pub index: u8,
    /// The partition's name, from the alt-setting's interface string descriptor.
    pub name: String,
    /// The region's length in bytes, as the descriptor exposed it. `None` is the
    /// names-only case: the partition is named, but the host does not know its
    /// extent. A size-driven read of it waits for a board on the bench.
    pub size: Option<u64>,
}

impl AltSetting {
    /// How many `sector_size`-byte sectors the region spans, or zero for an
    /// unknown size.
    ///
    /// A partition table built from alt-settings reports this as the partition's
    /// sector count. **Zero is the names-only case.** The size was not exposed, so
    /// there is no sector count to give. A caller must read zero as "the extent is
    /// unknown", not "the partition is empty". The count rounds up, because a region
    /// that does not end on a sector boundary still occupies the whole of its last
    /// sector.
    pub fn sectors(&self, sector_size: u32) -> u64 {
        match self.size {
            Some(bytes) => bytes.div_ceil(u64::from(sector_size)),
            None => 0,
        }
    }
}

/// Parse one alt-setting's interface string into an [`AltSetting`].
///
/// `index` is the alternate-setting number, and `descriptor` the interface string
/// the device gave it. The name is the first whitespace-separated token, and is
/// required. An empty or all-whitespace string returns [`Error::Protocol`], because
/// it came from the device. A partition with no name is a device fault, not a
/// caller mistake.
///
/// A size is read only from U-Boot's `raw <addr> <len>` form, where `<len>` is an
/// unambiguous byte count. Any other string is taken as names-only. This is the one
/// size-bearing shape the parser recognizes, and it is **\[UNVERIFIED\]**. No
/// Ingenic board has been read yet to confirm what its alt-settings say.
///
/// A board that names its ranges another way needs a change here and nowhere else.
/// The [`AltSetting`] shape, and everything built on it, handles a size or its
/// absence either way.
pub fn parse_alt(index: u8, descriptor: &str) -> Result<AltSetting> {
    let tokens: Vec<&str> = descriptor.split_whitespace().collect();
    let name = tokens.first().ok_or_else(|| {
        Error::Protocol(format!(
            "DFU alt-setting {index} has an empty interface string, so it names no partition"
        ))
    })?;

    // U-Boot's raw entry is `<name> raw <addr> <len>`: the length two tokens past
    // the `raw` marker is a byte count. Recognizing it here is what lets a
    // range-bearing board work with no rework; a board that names differently
    // falls through to names-only.
    let size = tokens
        .iter()
        .position(|token| token.eq_ignore_ascii_case("raw"))
        .and_then(|raw_at| tokens.get(raw_at + 2))
        .and_then(|len| parse_byte_count(len));

    Ok(AltSetting {
        index,
        name: (*name).to_string(),
        size,
    })
}

/// Parse a byte count the way U-Boot writes one: hex with a `0x` prefix or plain
/// decimal, and an optional single `k`/`m`/`g` binary-multiple suffix.
///
/// Returns `None` for anything it does not recognize, so a token that is not a
/// size leaves the alt-setting names-only rather than guessing a length. `k` is
/// 1024, `m` is 1024^2 and `g` is 1024^3, the binary multiples U-Boot's memory
/// sizes use. A value that overflows [`u64`] is rejected (`None`) rather than
/// wrapped.
fn parse_byte_count(token: &str) -> Option<u64> {
    let (digits, multiplier) = match token.chars().last().map(|c| c.to_ascii_lowercase()) {
        Some('k') => (&token[..token.len() - 1], 1024u64),
        Some('m') => (&token[..token.len() - 1], 1024 * 1024),
        Some('g') => (&token[..token.len() - 1], 1024 * 1024 * 1024),
        _ => (token, 1),
    };

    let base = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"));
    let value = match base {
        Some(hex) => u64::from_str_radix(hex, 16).ok()?,
        None => digits.parse::<u64>().ok()?,
    };

    value.checked_mul(multiplier)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The common case: an alt-setting names its partition and nothing more, so it
    /// is names-only and its extent is unknown until a board is read.
    #[test]
    fn a_bare_name_is_a_names_only_alt_setting() {
        let alt = parse_alt(2, "rootfs").expect("a bare name is a valid alt-setting");
        assert_eq!(alt.index, 2);
        assert_eq!(alt.name, "rootfs");
        assert_eq!(alt.size, None);
        // Names-only reports no sector count, which a caller reads as "unknown".
        assert_eq!(alt.sectors(512), 0);
    }

    /// The `raw <addr> <len>` form carries a byte length, and it is read as the
    /// partition's size. The length here is 512 KiB, which rounds to 1024 sectors
    /// of 512 bytes.
    #[test]
    fn a_raw_entry_exposes_its_length() {
        let alt = parse_alt(0, "uboot raw 0x0 0x80000").expect("a raw entry parses");
        assert_eq!(alt.name, "uboot");
        assert_eq!(alt.size, Some(0x80000));
        assert_eq!(alt.sectors(512), 0x80000 / 512);
    }

    /// A binary-multiple suffix is honored, so `4m` is four mebibytes, not four
    /// bytes. Read as four bytes, the size would cover only the partition's first
    /// sector.
    #[test]
    fn a_raw_length_with_a_binary_suffix_is_scaled() {
        let alt = parse_alt(1, "kernel raw 0x80000 4m").expect("a suffixed raw entry parses");
        assert_eq!(alt.size, Some(4 * 1024 * 1024));
    }

    /// Surrounding whitespace is not part of the name. An all-whitespace string
    /// names no partition. That is a device fault, not a caller error, so it is a
    /// protocol error.
    #[test]
    fn whitespace_is_trimmed_and_an_empty_string_is_refused() {
        let alt = parse_alt(0, "  boot  ").expect("a padded name is still a name");
        assert_eq!(alt.name, "boot");

        let error = parse_alt(0, "   ").expect_err("an empty string names nothing");
        assert!(matches!(error, Error::Protocol(_)), "{error:?}");
    }

    /// A token that is not a byte count leaves the alt-setting names-only, with no
    /// invented length. A `raw` marker followed by a non-numeric size gives no
    /// size.
    #[test]
    fn an_unparseable_raw_length_is_names_only() {
        let alt = parse_alt(0, "data raw 0x0 whoknows").expect("still a valid name");
        assert_eq!(alt.name, "data");
        assert_eq!(alt.size, None);
    }

    /// Each alt-setting gets its own slice of the LBA space, and an LBA unpacks
    /// back into the alt-setting and the block offset it names. This is the round
    /// trip the agent's read depends on: `alt_base_lba` places a partition, and
    /// `split_lba` recovers where a read landed.
    #[test]
    fn an_lba_packs_and_unpacks_its_alt_setting_and_offset() {
        assert_eq!(alt_base_lba(0), 0);
        assert_eq!(alt_base_lba(1), 1u64 << ALT_LBA_SHIFT);
        assert_eq!(alt_base_lba(3), 3u64 << ALT_LBA_SHIFT);

        let (alt, offset) = split_lba(alt_base_lba(3) + 5);
        assert_eq!(alt, 3);
        assert_eq!(offset, 5);

        // The block offset fills its half exactly to the edge without spilling
        // into the alt-setting bits.
        let (alt, offset) = split_lba(alt_base_lba(2) + OFFSET_MASK);
        assert_eq!(alt, 2);
        assert_eq!(offset, OFFSET_MASK);
    }

    /// An LBA whose high bits name an alt-setting past a byte is reported whole,
    /// not folded back onto a real one. The agent checks this against the
    /// alt-settings it has and refuses it. Truncating to `u8` here would let an
    /// out-of-range address pass as a valid region.
    #[test]
    fn an_out_of_range_alt_setting_is_not_truncated() {
        let wild = 300u64 << ALT_LBA_SHIFT;
        let (alt, offset) = split_lba(wild);
        assert_eq!(alt, 300);
        assert_eq!(offset, 0);
    }
}
