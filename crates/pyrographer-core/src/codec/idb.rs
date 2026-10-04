//! The ID block: the first stage a Rockchip BootROM reads from flash.
//!
//! The BootROM reads the ID block from sector 64. It holds the code that brings up
//! DRAM and loads the SPL. A loader container does not carry one ready-made. It
//! carries the pieces, its **flash stages**, and a tool lays them out. This module
//! is that layout, and the check that it is right.
//!
//! # The `RKNS` header is the authority
//!
//! One flash stage is an `RKNS` header. It lists the images the ID block holds.
//! For each, it records the sector it sits at, counted from the start of the block,
//! the sectors it fills, and its SHA-256. It also carries a SHA-256 of its own first
//! bytes. [`build`] places each image where the header says, and finds the stage
//! for each image **by hash**, not by name. The block it returns is therefore the
//! one its header describes, and a stage the header does not name is not written.
//!
//! Laying the stages out by name instead goes wrong on a real container. On the
//! RK3576 container, Rockchip's rkdeveloptool concatenates three stages by name and
//! never reads a fourth, `FlashBoost`, which the header places first. The block it
//! writes puts DRAM init and the SPL eight sectors before the places its own header
//! names. That is measured from the file and the tool's source, and not observed on
//! a board.
//!
//! # What the flash holds
//!
//! A container stores every flash stage scrambled, per 512-byte block, under the
//! fixed key in [`rc4`](super::rc4). Its RC4 flag says what the flash holds
//! ([`LoaderImage::rc4_disabled`]). With the flag set, the BootROM reads plaintext,
//! and the stages are unscrambled before they are laid out. The RK3576 container
//! sets it, and unscrambling its DRAM-init stage yields, byte for byte, the 471
//! blob its BootROM accepts over USB.
//!
//! With the flag clear, the reference tool writes the stored bytes for the BootROM
//! to unscramble. This module builds the first case only, because its check is the
//! header's hashes, and those are over plaintext. A container with the flag clear is
//! refused.
//!
//! A container whose stages carry no `RKNS` header predates it. Its ID block is
//! four sectors a tool builds itself, and this module refuses one by name.
//!
//! The layout is **\[COMMUNITY\]** (U-Boot's `header0_info_v2` in
//! `tools/rkcommon.c`), and every field this module reads is checked against the
//! RK3576 container, `rk3576_spl_loader_v1.12.108.bin`. Whether a board boots from
//! the block it builds is **\[UNVERIFIED\]**.

use super::rc4::{MASKROM_RC4_KEY, rc4};
use super::rkboot::{CodeBlob, LoaderImage};
use super::sha256::{DIGEST_LEN, sha256};
use crate::{Error, Result};

/// The sector the BootROM reads the ID block from, in 512-byte sectors.
/// **\[DOC\]**
pub const LBA: u64 = 64;

/// The unit every offset and length in an ID block counts, in bytes.
///
/// The header counts 512-byte sectors whatever the device's own sector size is.
pub const SECTOR_LEN: usize = 512;

/// The most sectors an ID block can fill.
///
/// The storage map reserves sectors 64 to 7167 for it. Vendor storage begins at
/// sector 7168, and a longer block would overwrite it. **\[DOC\]**
pub const MAX_SECTORS: u64 = 7104;

/// The four bytes an `RKNS` header begins with.
pub const MAGIC: &[u8; 4] = b"RKNS";

/// Where the header keeps its hashed length and its image count.
const OFF_SIZE_AND_COUNT: usize = 8;
/// Where the header names its hash algorithm.
const OFF_HASH_KIND: usize = 12;
/// Where the image table begins.
const OFF_IMAGES: usize = 120;
/// The length of one image entry.
const IMAGE_ENTRY_LEN: usize = 88;
/// The most images a header lists.
const MAX_IMAGES: usize = 4;
/// Where an image's hash sits inside its entry.
const ENTRY_HASH: usize = 24;
/// The hash algorithm this module checks: SHA-256. The header's field reads `1`
/// for it on the RK3576 container.
const HASH_SHA256: u32 = 1;
/// The load address that means none.
const NO_LOAD_ADDRESS: u32 = 0xffff_ffff;

/// An ID block, laid out and checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdBlock {
    /// The whole block, a whole number of 512-byte sectors: the header, then each
    /// image at the sector its header names, with zeros between.
    pub bytes: Vec<u8>,
    /// The flash stage the `RKNS` header came from, such as `FlashHead`.
    pub header_stage: String,
    /// The images the header lists, in header order.
    pub images: Vec<IdbImage>,
}

impl IdBlock {
    /// How many 512-byte sectors the block fills.
    pub fn sectors(&self) -> u64 {
        (self.bytes.len() / SECTOR_LEN) as u64
    }
}

/// One image an ID block holds, as its header lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdbImage {
    /// The flash stage whose bytes hash to the header's record, such as
    /// `FlashData`.
    pub stage: String,
    /// The image's first sector, counted from the start of the ID block.
    pub sector: u64,
    /// How many sectors it fills.
    pub sectors: u64,
    /// Where the BootROM loads it, or `None` where the header says none.
    pub load_address: Option<u32>,
}

/// Build the ID block from `loader`'s flash stages, and check it against its own
/// header.
///
/// Each of these is an [`Error::InvalidRequest`], refused before anything is
/// planned:
///
/// - A loader with no flash stages, such as one built from bare 471 and 472 files
/// - A container whose RC4 flag is clear, as the module documentation describes
/// - A flash stage that is not a whole number of sectors
/// - No `RKNS` header among the stages, which is the legacy layout, or more than one
/// - A header whose own SHA-256 does not hold, or that names a hash other than
///   SHA-256
/// - An image that overlaps the header or another image, or that no stage's bytes
///   hash to
/// - A block longer than [`MAX_SECTORS`]
pub fn build(loader: &LoaderImage) -> Result<IdBlock> {
    if loader.flash_stages.is_empty() {
        return Err(refused(
            "the loader carries no flash stages, so there is nothing to build an ID block from. \
             A loader made of bare 471 and 472 files has none. An RKBOOT container carries them"
                .to_string(),
        ));
    }
    if !loader.rc4_disabled {
        return Err(refused(
            "the container's RC4 flag is clear, so its flash stages go to the flash scrambled, \
             for the BootROM to unscramble. pyrographer checks an ID block against its header's \
             hashes, which are over plaintext, and builds only the plaintext form"
                .to_string(),
        ));
    }

    let stages = loader
        .flash_stages
        .iter()
        .map(unscramble)
        .collect::<Result<Vec<_>>>()?;

    // The header is the stage that unscrambles to `RKNS`, whatever it is called.
    let mut headers = stages.iter().filter(|stage| stage.data.starts_with(MAGIC));
    let header = headers.next().ok_or_else(|| {
        refused(
            "no flash stage is an RKNS header, so this is the legacy ID block layout, four \
             sectors a tool builds itself. pyrographer builds the RKNS layout only"
                .to_string(),
        )
    })?;
    if let Some(second) = headers.next() {
        return Err(refused(format!(
            "two flash stages are RKNS headers, '{}' and '{}', so the loader does not say which \
             one describes its ID block",
            header.name, second.name
        )));
    }

    let entries = read_header(&header.data)?;
    let header_sectors = (header.data.len() / SECTOR_LEN) as u64;

    // Each image, matched to the stage whose bytes its hash names. The header is
    // not a candidate: it describes the images and is not one of them.
    let mut images = Vec::with_capacity(entries.len());
    let mut placed: Vec<(u64, u64, &[u8])> = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let len = entry.sectors as usize * SECTOR_LEN;
        let stage = stages
            .iter()
            .filter(|stage| !std::ptr::eq(*stage, header))
            .find(|stage| stage.data.len() >= len && sha256(&stage.data[..len]) == entry.hash)
            .ok_or_else(|| {
                refused(format!(
                    "image {index} of the RKNS header (sector {}, {} sectors) matches no flash \
                     stage in the loader. Its header records SHA-256 {}, and no stage's first {len} \
                     bytes hash to it",
                    entry.sector,
                    entry.sectors,
                    hex(&entry.hash)
                ))
            })?;

        if entry.sector < header_sectors {
            return Err(refused(format!(
                "image {index} begins at sector {}, inside the {header_sectors} sectors the RKNS \
                 header itself fills",
                entry.sector
            )));
        }
        if let Some((other, ..)) = placed.iter().enumerate().find(|(_, (sector, sectors, _))| {
            entry.sector < sector + sectors && *sector < entry.sector + entry.sectors
        }) {
            return Err(refused(format!(
                "images {other} and {index} of the RKNS header overlap, so the header describes no \
                 block they can both be laid in"
            )));
        }

        placed.push((entry.sector, entry.sectors, &stage.data[..len]));
        images.push(IdbImage {
            stage: stage.name.clone(),
            sector: entry.sector,
            sectors: entry.sectors,
            load_address: (entry.load_address != NO_LOAD_ADDRESS).then_some(entry.load_address),
        });
    }

    let total = placed
        .iter()
        .map(|(sector, sectors, _)| sector + sectors)
        .chain([header_sectors])
        .max()
        .unwrap_or(header_sectors);
    if total > MAX_SECTORS {
        return Err(refused(format!(
            "the ID block would fill {total} sectors, past the {MAX_SECTORS} the storage map \
             reserves for it. Sector {} onward is vendor storage",
            LBA + MAX_SECTORS
        )));
    }

    let mut bytes = vec![0u8; total as usize * SECTOR_LEN];
    bytes[..header.data.len()].copy_from_slice(&header.data);
    for (sector, _, data) in &placed {
        let at = *sector as usize * SECTOR_LEN;
        bytes[at..at + data.len()].copy_from_slice(data);
    }

    // The block was assembled from bytes that matched; this reads it back the way a
    // BootROM would, so a placement mistake in the lines above cannot pass.
    check(&bytes)?;

    Ok(IdBlock {
        bytes,
        header_stage: header.name.clone(),
        images,
    })
}

/// Check an ID block in place: its `RKNS` header's own hash, and each image's hash
/// over the sectors the header places it in.
///
/// `bytes` is the block as it goes to the flash, from its first sector. It returns
/// the images the header lists, each found where the header says. [`build`] runs it
/// over every block it builds, and a caller can run it over a prebuilt
/// `idbloader.img`.
pub fn check(bytes: &[u8]) -> Result<Vec<(u64, u64)>> {
    let entries = read_header(bytes)?;
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let at = entry.sector as usize * SECTOR_LEN;
            let len = entry.sectors as usize * SECTOR_LEN;
            let image = at
                .checked_add(len)
                .and_then(|end| bytes.get(at..end))
                .ok_or_else(|| {
                    refused(format!(
                        "image {index} of the RKNS header runs to sector {}, past the {} sectors \
                         of the block",
                        entry.sector + entry.sectors,
                        bytes.len() / SECTOR_LEN
                    ))
                })?;
            if sha256(image) != entry.hash {
                return Err(refused(format!(
                    "image {index} at sector {} does not hash to the SHA-256 its header records",
                    entry.sector
                )));
            }
            Ok((entry.sector, entry.sectors))
        })
        .collect()
}

/// One image entry from an `RKNS` header, as read.
struct Entry {
    sector: u64,
    sectors: u64,
    load_address: u32,
    hash: [u8; DIGEST_LEN],
}

/// Read and check an `RKNS` header: its magic, its hash algorithm, its own hash,
/// and its image table.
fn read_header(header: &[u8]) -> Result<Vec<Entry>> {
    if !header.starts_with(MAGIC) {
        return Err(refused(
            "the block does not begin with an RKNS header".to_string(),
        ));
    }

    let size_and_count = le_u32(header, OFF_SIZE_AND_COUNT)?;
    let hashed_len = (size_and_count & 0xffff) as usize * 4;
    let count = (size_and_count >> 16) as usize;

    // The hashed region has to hold the image table, and the hash has to follow it
    // inside the header.
    let table_end = OFF_IMAGES + MAX_IMAGES * IMAGE_ENTRY_LEN;
    if hashed_len < table_end || hashed_len + DIGEST_LEN > header.len() {
        return Err(refused(format!(
            "the RKNS header declares a hashed length of {hashed_len} bytes, which does not cover \
             its image table and leave room for its hash in its {} bytes",
            header.len()
        )));
    }

    let kind = le_u32(header, OFF_HASH_KIND)?;
    if kind != HASH_SHA256 {
        return Err(refused(format!(
            "the RKNS header names hash algorithm {kind}, and pyrographer checks SHA-256 ({HASH_SHA256}) \
             only"
        )));
    }

    if sha256(&header[..hashed_len]) != header[hashed_len..hashed_len + DIGEST_LEN] {
        return Err(refused(format!(
            "the RKNS header's own SHA-256, over its first {hashed_len} bytes, does not hold"
        )));
    }

    if count == 0 || count > MAX_IMAGES {
        return Err(refused(format!(
            "the RKNS header lists {count} images, and a header lists from 1 to {MAX_IMAGES}"
        )));
    }

    (0..count)
        .map(|index| {
            let at = OFF_IMAGES + index * IMAGE_ENTRY_LEN;
            let place = le_u32(header, at)?;
            let sectors = u64::from(place >> 16);
            if sectors == 0 {
                return Err(refused(format!(
                    "image {index} of the RKNS header is zero sectors long"
                )));
            }
            let mut hash = [0u8; DIGEST_LEN];
            hash.copy_from_slice(&header[at + ENTRY_HASH..at + ENTRY_HASH + DIGEST_LEN]);
            Ok(Entry {
                sector: u64::from(place & 0xffff),
                sectors,
                load_address: le_u32(header, at + 4)?,
                hash,
            })
        })
        .collect()
}

/// A flash stage, unscrambled block by block.
///
/// The container scrambles each 512-byte block separately, so each is unscrambled
/// separately. A stage that is not a whole number of blocks was not built the way
/// the container builder builds one, and is refused rather than guessed at.
fn unscramble(stage: &CodeBlob) -> Result<CodeBlob> {
    if !stage.data.len().is_multiple_of(SECTOR_LEN) {
        return Err(refused(format!(
            "flash stage '{}' is {} bytes, which is not a whole number of {SECTOR_LEN}-byte blocks",
            stage.name,
            stage.data.len()
        )));
    }
    let data = stage
        .data
        .chunks(SECTOR_LEN)
        .flat_map(|block| rc4(&MASKROM_RC4_KEY, block))
        .collect();
    Ok(CodeBlob {
        name: stage.name.clone(),
        data,
        delay_ms: stage.delay_ms,
    })
}

/// Read a little-endian `u32` at `offset`, or refuse a header too short for it.
fn le_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    bytes
        .get(offset..offset + 4)
        .and_then(|field| field.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(|| {
            refused(format!(
                "the RKNS header is {} bytes, too short for the field at offset {offset}",
                bytes.len()
            ))
        })
}

/// A digest as lowercase hex, for an error a person reads.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Wrap a reason as the ID block's refusal. The loader is user input, so a bad one
/// is [`Error::InvalidRequest`]: nothing malfunctioned, and only the caller can
/// supply a different file.
fn refused(detail: String) -> Error {
    Error::InvalidRequest(format!("ID block: {detail}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stage's bytes as a container stores them: scrambled per 512-byte block.
    fn scrambled(plain: &[u8]) -> Vec<u8> {
        plain
            .chunks(SECTOR_LEN)
            .flat_map(|block| rc4(&MASKROM_RC4_KEY, block))
            .collect()
    }

    /// An image's plaintext: `sectors` sectors of a pattern seeded by `seed`.
    fn image(seed: u8, sectors: usize) -> Vec<u8> {
        (0..sectors * SECTOR_LEN)
            .map(|i| seed.wrapping_add((i % 251) as u8))
            .collect()
    }

    /// An `RKNS` header of `sectors` sectors placing each `(sector, image)`, with
    /// every hash computed, as the container's header stage holds it unscrambled.
    fn header(sectors: usize, images: &[(u64, &[u8])]) -> Vec<u8> {
        let mut h = vec![0u8; sectors * SECTOR_LEN];
        h[..4].copy_from_slice(MAGIC);
        let hashed_words: u32 = 384;
        h[8..12].copy_from_slice(&(hashed_words | (images.len() as u32) << 16).to_le_bytes());
        h[12..16].copy_from_slice(&HASH_SHA256.to_le_bytes());
        for (index, (sector, data)) in images.iter().enumerate() {
            let at = OFF_IMAGES + index * IMAGE_ENTRY_LEN;
            let len = (data.len() / SECTOR_LEN) as u32;
            h[at..at + 4].copy_from_slice(&((*sector as u32) | len << 16).to_le_bytes());
            h[at + 4..at + 8].copy_from_slice(&(0x1000_0000u32 + index as u32).to_le_bytes());
            h[at + 12..at + 16].copy_from_slice(&(index as u32 + 1).to_le_bytes());
            h[at + ENTRY_HASH..at + ENTRY_HASH + DIGEST_LEN].copy_from_slice(&sha256(data));
        }
        let digest = sha256(&h[..1536]);
        h[1536..1536 + DIGEST_LEN].copy_from_slice(&digest);
        h
    }

    /// A loader whose flash stages are `(name, plaintext)`, scrambled as a
    /// container stores them, with the RC4 flag set.
    fn loader(stages: &[(&str, Vec<u8>)]) -> LoaderImage {
        LoaderImage {
            code_471: Vec::new(),
            code_472: Vec::new(),
            flash_stages: stages
                .iter()
                .map(|(name, plain)| CodeBlob {
                    name: name.to_string(),
                    data: scrambled(plain),
                    delay_ms: 0,
                })
                .collect(),
            chip: Some(*b"6753"),
            rc4_disabled: true,
        }
    }

    /// The header places each image, and the stage for each is found by hash. The
    /// stages here are listed in an order the header does not follow and named
    /// nothing like their roles, and the block still comes out as the header
    /// describes it.
    #[test]
    fn each_image_lands_where_the_header_says_whatever_the_stages_are_called() {
        let boost = image(0x10, 8);
        let ddr = image(0x20, 16);
        let spl = image(0x30, 24);
        let head = header(8, &[(8, &boost), (16, &ddr), (32, &spl)]);
        let idb = build(&loader(&[
            ("zeta", spl.clone()),
            ("FlashHead", head.clone()),
            ("alpha", ddr.clone()),
            ("mu", boost.clone()),
        ]))
        .expect("a consistent loader builds");

        assert_eq!(idb.sectors(), 56, "the last image ends at sector 32 + 24");
        assert_eq!(
            &idb.bytes[..head.len()],
            &head[..],
            "the header, unscrambled"
        );
        assert_eq!(&idb.bytes[8 * 512..16 * 512], &boost[..]);
        assert_eq!(&idb.bytes[16 * 512..32 * 512], &ddr[..]);
        assert_eq!(&idb.bytes[32 * 512..56 * 512], &spl[..]);
        assert_eq!(idb.header_stage, "FlashHead");
        let found: Vec<(&str, u64, u64)> = idb
            .images
            .iter()
            .map(|image| (image.stage.as_str(), image.sector, image.sectors))
            .collect();
        assert_eq!(found, [("mu", 8, 8), ("alpha", 16, 16), ("zeta", 32, 24)]);
        assert_eq!(idb.images[0].load_address, Some(0x1000_0000));
    }

    /// A stage the header does not name is not written: the block holds what its
    /// header describes, and nothing else.
    #[test]
    fn a_stage_the_header_does_not_name_is_left_out() {
        let ddr = image(0x20, 8);
        let head = header(8, &[(8, &ddr)]);
        let idb = build(&loader(&[
            ("FlashHead", head),
            ("FlashData", ddr),
            ("FlashExtra", image(0x77, 4)),
        ]))
        .expect("builds");
        assert_eq!(idb.sectors(), 16);
        assert!(idb.images.iter().all(|image| image.stage != "FlashExtra"));
    }

    /// A gap between images is zeros, not whatever a buffer held.
    #[test]
    fn the_gaps_between_images_are_zero() {
        let ddr = image(0x20, 4);
        let spl = image(0x30, 4);
        let head = header(8, &[(8, &ddr), (20, &spl)]);
        let idb = build(&loader(&[("H", head), ("D", ddr), ("S", spl)])).expect("builds");
        assert!(idb.bytes[12 * 512..20 * 512].iter().all(|&b| b == 0));
    }

    /// An image whose bytes no stage carries is refused, naming the image.
    #[test]
    fn an_image_no_stage_hashes_to_is_refused() {
        let ddr = image(0x20, 8);
        let head = header(8, &[(8, &ddr)]);
        let mut wrong = ddr.clone();
        wrong[100] ^= 1;
        let err = build(&loader(&[("FlashHead", head), ("FlashData", wrong)]))
            .expect_err("the stage does not match the header's hash");
        let text = err.to_string();
        assert!(
            text.contains("image 0") && text.contains("matches no flash stage"),
            "{text}"
        );
    }

    /// A header whose own hash does not hold is refused before any image is
    /// matched: the table it carries cannot be believed.
    #[test]
    fn a_header_whose_own_hash_fails_is_refused() {
        let ddr = image(0x20, 8);
        let mut head = header(8, &[(8, &ddr)]);
        head[200] ^= 1;
        let err = build(&loader(&[("FlashHead", head), ("FlashData", ddr)])).expect_err("bad");
        assert!(err.to_string().contains("own SHA-256"), "{err}");
    }

    /// A container with its RC4 flag clear wants the stages scrambled on the flash,
    /// which this module does not build.
    #[test]
    fn a_container_with_the_rc4_flag_clear_is_refused() {
        let ddr = image(0x20, 8);
        let mut l = loader(&[("FlashHead", header(8, &[(8, &ddr)])), ("FlashData", ddr)]);
        l.rc4_disabled = false;
        let err = build(&l).expect_err("flag clear");
        assert!(err.to_string().contains("RC4 flag is clear"), "{err}");
    }

    /// Stages with no `RKNS` header among them are the legacy layout, refused by
    /// name.
    #[test]
    fn the_legacy_layout_is_refused_by_name() {
        let err = build(&loader(&[
            ("FlashData", image(1, 4)),
            ("FlashBoot", image(2, 4)),
        ]))
        .expect_err("no RKNS");
        assert!(err.to_string().contains("legacy"), "{err}");
    }

    /// A loader with no flash stages has nothing to build from.
    #[test]
    fn a_loader_with_no_stages_is_refused() {
        let err = build(&LoaderImage::from_raw(None, None)).expect_err("no stages");
        assert!(err.to_string().contains("no flash stages"), "{err}");
    }

    /// An image placed inside the header, or over another image, is refused.
    #[test]
    fn overlapping_images_are_refused() {
        let a = image(0x20, 8);
        let b = image(0x30, 8);
        let inside = header(8, &[(4, &a)]);
        let err = build(&loader(&[("H", inside), ("A", a.clone())])).expect_err("inside");
        assert!(err.to_string().contains("inside"), "{err}");

        let over = header(8, &[(8, &a), (12, &b)]);
        let err = build(&loader(&[("H", over), ("A", a), ("B", b)])).expect_err("overlap");
        assert!(err.to_string().contains("overlap"), "{err}");
    }

    /// A header naming a hash other than SHA-256 is refused rather than read as one.
    #[test]
    fn another_hash_algorithm_is_refused() {
        let ddr = image(0x20, 8);
        let mut head = header(8, &[(8, &ddr)]);
        head[12..16].copy_from_slice(&2u32.to_le_bytes());
        let digest = sha256(&head[..1536]);
        head[1536..1536 + DIGEST_LEN].copy_from_slice(&digest);
        let err = build(&loader(&[("H", head), ("D", ddr)])).expect_err("SHA-512");
        assert!(err.to_string().contains("algorithm 2"), "{err}");
    }

    /// A block that would run into vendor storage is refused.
    #[test]
    fn a_block_past_the_reserved_region_is_refused() {
        let big = image(0x20, 8);
        let head = header(8, &[(MAX_SECTORS, &big)]);
        let err = build(&loader(&[("H", head), ("D", big)])).expect_err("too long");
        assert!(err.to_string().contains("vendor storage"), "{err}");
    }

    /// A stage that is not a whole number of blocks is refused.
    #[test]
    fn a_stage_that_is_not_whole_blocks_is_refused() {
        let mut l = loader(&[("H", header(8, &[]))]);
        l.flash_stages[0].data.push(0);
        let err = build(&l).expect_err("ragged");
        assert!(err.to_string().contains("whole number"), "{err}");
    }

    /// `check` reads a built block back the way a BootROM would, and a flipped
    /// byte in an image fails it.
    #[test]
    fn check_finds_a_damaged_image() {
        let ddr = image(0x20, 8);
        let idb = build(&loader(&[("H", header(8, &[(8, &ddr)])), ("D", ddr)])).expect("builds");
        assert_eq!(check(&idb.bytes).expect("intact"), [(8, 8)]);
        let mut damaged = idb.bytes.clone();
        damaged[9 * 512] ^= 0xff;
        assert!(check(&damaged).is_err());
    }

    /// The real RK3576 container builds the ID block its own header describes:
    /// three images, `FlashBoost` among them, 704 sectors in all. The digest of
    /// the whole block is pinned against an independent build of the same layout.
    /// The file is not committed with the crate, so a checkout without it skips.
    #[test]
    fn the_real_rk3576_loader_builds_the_block_its_header_describes() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../reference/rk3576_spl_loader_v1.12.108.bin"
        );
        let Ok(bytes) = std::fs::read(path) else {
            eprintln!("skipping: reference loader not present at {path}");
            return;
        };
        let loader = super::super::rkboot::parse(&bytes).expect("the loader parses");
        let idb = build(&loader).expect("the RK3576 ID block builds");

        assert_eq!(idb.header_stage, "FlashHead");
        let found: Vec<(&str, u64, u64, Option<u32>)> = idb
            .images
            .iter()
            .map(|i| (i.stage.as_str(), i.sector, i.sectors, i.load_address))
            .collect();
        assert_eq!(
            found,
            [
                ("FlashBoost", 8, 8, Some(0x3ffc_0000)),
                ("FlashData", 16, 160, Some(0x3ff8_1000)),
                ("FlashBoot", 176, 528, None),
            ]
        );
        assert_eq!(idb.sectors(), 704);
        assert_eq!(
            hex(&sha256(&idb.bytes)),
            "0ee14d19e4f0f23cc16623867af29bc1114b4e7abc9e4373d5563a8338c4ae3f"
        );

        // DRAM init in the ID block is, byte for byte, the 471 blob the BootROM
        // accepts over USB.
        let ddr = &loader.code_471[1].data;
        assert_eq!(&idb.bytes[16 * 512..16 * 512 + ddr.len()], &ddr[..]);
    }
}
