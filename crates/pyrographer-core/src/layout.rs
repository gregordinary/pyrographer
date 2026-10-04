//! A partition layout a person supplies to author a table from scratch.
//!
//! [`partition`](crate::partition) reads a table a board already has. A repair
//! rebuilds a damaged copy of a table from an intact copy of the same table.
//! Authoring builds a table from a layout the board does not hold: the partitions
//! a person means to lay down. This module reads that layout from text.
//!
//! # Layout formats
//!
//! A [`Layout`] is neutral: an ordered list of partitions in **absolute** sectors,
//! each with an optional type token. The authoring verbs consume a `Layout` and
//! never parse text, so either input format drives either table format. A board's
//! own `mtdparts` line and one format standardized across many boards reach the
//! same authoring path. The two input formats are these:
//!
//! - [`Layout::parse_mtdparts`] reads the `mtdparts=` list a Rockchip board already
//!   carries, with the same parser [`rkparam`] uses to read a board's table. It is
//!   exact for a parameter block. Its offsets are relative to a base
//!   ([eMMC or NAND](crate::codec::rkparam::parse)), which it needs to resolve them
//!   to the absolute sectors a [`Layout`] holds. This is the one place a person
//!   must name the medium.
//! - [`Layout::parse_native`] reads pyrographer's own format, one partition per
//!   line. Its LBAs are absolute, with no base to fix up, so it cannot place a
//!   partition 4 MiB from where it was meant. It also carries three things a GPT
//!   needs and an `mtdparts` line cannot express: a partition type, a
//!   per-partition unique GUID, and a `disk-guid` directive.

use crate::codec::rkparam;
use crate::partition::Partition;
use crate::{Error, Result};

/// A partition layout: the partitions a fresh table is to hold, in absolute
/// sectors and in the order they were given.
///
/// It is neutral between the formats it can be written to. A parameter block
/// ignores the per-partition [`kind`](LayoutPartition::kind), and GPT authoring
/// maps it to a type GUID. Every target uses the name, the first sector and the
/// length, and a [`LayoutPartition`] carries those.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// The partitions, in the order the layout gave them.
    pub partitions: Vec<LayoutPartition>,
    /// The disk GUID a person pinned for an authored GPT, as the raw token they
    /// wrote, or `None`.
    ///
    /// It is the table-level half of the GUID override in the "deterministic +
    /// override" authoring policy. `None` means GPT authoring synthesizes the disk
    /// GUID from the layout, and `Some` means it uses this instead.
    ///
    /// It is kept as the raw token, not a parsed [`Guid`], for the reason
    /// [`kind`](LayoutPartition::kind) is. The parameter format has no disk GUID and
    /// ignores this, and GPT authoring, the one consumer that reads it, parses it as
    /// a GUID. It comes from the native format's `disk-guid <GUID>` directive. The
    /// `mtdparts` front-end cannot express it, and leaves it `None`.
    ///
    /// [`Guid`]: crate::codec::gpt::Guid
    pub disk_guid: Option<String>,
}

/// One partition in a [`Layout`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutPartition {
    /// The name the table will give it.
    pub name: String,
    /// The first sector it occupies, absolute and not relative to any base. The
    /// `mtdparts` front-end adds the base its offsets were counted from, so a
    /// [`Layout`] never holds a base-relative offset.
    pub first_lba: u64,
    /// How many sectors it occupies, with a `-` size already resolved against the
    /// part.
    pub sectors: u64,
    /// The type token from the native format's optional type column, or `None`.
    ///
    /// The parameter format has no notion of a partition type, and ignores this. GPT
    /// authoring maps it to a type GUID, and its absence to a default. It is kept as
    /// the raw token, not a decoded type, so the vocabulary of types lives with the
    /// format that reads it, in [`type_guid_for`](crate::codec::gpt::type_guid_for).
    /// The layout does not interpret it.
    pub kind: Option<String>,
    /// The unique GUID a person pinned for this partition in an authored GPT, as the
    /// raw token they wrote, or `None`.
    ///
    /// It is the per-partition half of the GUID override. `None` means GPT
    /// authoring synthesizes this partition's unique GUID, and `Some` means it uses
    /// this. It is kept raw for the same reason [`kind`](Self::kind) is, and the
    /// parameter format ignores it the same way. It comes from the native format's
    /// `uuid=<GUID>` attribute, which `mtdparts` cannot express.
    pub unique_guid: Option<String>,
}

impl Layout {
    /// Parse an `mtdparts` value into a layout, offsets counted from `base_lba`.
    ///
    /// `input` takes one of three forms: the `mtdparts=rk29xxnand:<list>` from a
    /// board's command line, the `rk29xxnand:<list>` alone, or a whole `CMDLINE:`
    /// line carrying one. In each case, the list is parsed by the same
    /// [`rkparam::parse_mtdparts`] that reads a board's own table. A layout authored
    /// from an `mtdparts` line therefore means exactly what that line means on the
    /// board it came from.
    ///
    /// `base_lba` is the medium's base: [`EMMC_BASE_LBA`](rkparam::EMMC_BASE_LBA)
    /// for an eMMC, and `0` for raw NAND. An `mtdparts` offset is relative to it,
    /// and a [`Layout`] is absolute. `flash_sectors` is what a `-` size is resolved
    /// against.
    pub fn parse_mtdparts(input: &str, base_lba: u64, flash_sectors: u64) -> Result<Layout> {
        Self::parse_mtdparts_growing_to(input, base_lba, flash_sectors, flash_sectors)
    }

    /// [`parse_mtdparts`](Self::parse_mtdparts), with a partition marked to grow
    /// ending at `grow_end` rather than at the end of the part.
    ///
    /// A GPT authored from the layout passes [`gpt::grow_end`], for the reason
    /// [`parse_native_growing_to`](Self::parse_native_growing_to) gives.
    /// `flash_sectors` still bounds where a partition can begin.
    ///
    /// [`gpt::grow_end`]: crate::codec::gpt::grow_end
    pub fn parse_mtdparts_growing_to(
        input: &str,
        base_lba: u64,
        flash_sectors: u64,
        grow_end: u64,
    ) -> Result<Layout> {
        // Accept a whole command line, an `mtdparts=...` token, or the bare
        // `rk29xxnand:...`: find the token if it is there, else take the input as
        // the list itself.
        let mtdparts = input
            .split_whitespace()
            .find_map(|token| token.strip_prefix("mtdparts="))
            .unwrap_or_else(|| input.trim());

        let partitions =
            rkparam::parse_mtdparts_growing_to(mtdparts, base_lba, flash_sectors, grow_end)?
                .into_iter()
                .map(|part| LayoutPartition {
                    name: part.name,
                    first_lba: part.first_lba,
                    sectors: part.sectors,
                    kind: None,
                    unique_guid: None,
                })
                .collect();

        // An `mtdparts` line expresses neither partition types nor GUIDs, so an
        // authored GPT from one gets the default type and synthesized GUIDs. A
        // person who wants to pin either reaches for the native format.
        Ok(Layout {
            partitions,
            disk_guid: None,
        })
    }

    /// Parse pyrographer's native layout format into a layout.
    ///
    /// One partition per line, whitespace-separated: `name first_lba sectors [type]
    /// [uuid=<GUID>]`. The first sector and the size are hexadecimal with a `0x`
    /// prefix, and decimal otherwise. A size of `-` grows the partition to the end
    /// of the part, resolved against `flash_sectors`. A `#` begins a comment, to the
    /// end of the line, and blank lines are skipped.
    ///
    /// The LBAs are absolute, with no base to add, so this format cannot get the
    /// eMMC/NAND fixup wrong. It can express three things a GPT needs that
    /// `mtdparts` cannot:
    ///
    /// - An optional **type** token. It is a name from the vocabulary
    ///   [`type_guid_for`](crate::codec::gpt::type_guid_for) reads (`esp`, `linux`,
    ///   `swap`, ...), or a raw type GUID.
    /// - An optional **`uuid=<GUID>`** attribute, pinning the partition's unique
    ///   GUID. It is the per-partition half of the GUID override.
    /// - A **`disk-guid <GUID>`** directive on its own line, pinning the table's disk
    ///   GUID. It is the table-level half.
    ///
    /// The type and the `uuid=` attribute can appear in either order after the three
    /// required columns. A `key=value` token is an attribute, and only `uuid=` is
    /// defined. A bare token is the type. Whatever is not pinned is synthesized
    /// deterministically as the table is authored, and the parameter format ignores
    /// all three.
    pub fn parse_native(text: &str, flash_sectors: u64) -> Result<Layout> {
        Self::parse_native_growing_to(text, flash_sectors, flash_sectors)
    }

    /// [`parse_native`](Self::parse_native), with a partition marked to grow ending
    /// at `grow_end` rather than at the end of the part.
    ///
    /// A GPT keeps its backup in the last sectors of the part. A partition marked
    /// to grow therefore ends at the GPT's last usable sector, and a GPT authored
    /// from the layout passes [`gpt::grow_end`] here. A parameter block keeps
    /// nothing at the end of the part, and grows to it with [`parse_native`].
    /// `flash_sectors` still bounds where a partition can begin. A partition marked
    /// to grow that begins at or past `grow_end` has nothing to grow into, and is
    /// refused.
    ///
    /// [`gpt::grow_end`]: crate::codec::gpt::grow_end
    /// [`parse_native`]: Self::parse_native
    pub fn parse_native_growing_to(
        text: &str,
        flash_sectors: u64,
        grow_end: u64,
    ) -> Result<Layout> {
        let mut partitions = Vec::new();
        let mut disk_guid: Option<String> = None;

        for (index, raw) in text.lines().enumerate() {
            let lineno = index + 1;

            // A comment runs to the end of its line, and a line that is only a
            // comment or only whitespace is not a partition.
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }

            let mut columns = line.split_whitespace();
            // A line leads with a directive keyword or a partition name. The one
            // directive is `disk-guid`, so its keyword is reserved as a first token.
            let head = columns.next().ok_or_else(|| malformed(lineno, line))?;

            if head == "disk-guid" {
                let value = columns.next().ok_or_else(|| {
                    Error::InvalidRequest(format!(
                        "layout line {lineno}: 'disk-guid' needs a value, like 'disk-guid \
                         C12A7328-F81F-11D2-BA4B-00A0C93EC93B'"
                    ))
                })?;
                if columns.next().is_some() {
                    return Err(Error::InvalidRequest(format!(
                        "layout line {lineno}: 'disk-guid' takes one value: '{line}'"
                    )));
                }
                if disk_guid.replace(value.to_string()).is_some() {
                    return Err(Error::InvalidRequest(format!(
                        "layout line {lineno}: 'disk-guid' is given more than once"
                    )));
                }
                continue;
            }

            let name = head;
            let first = columns.next().ok_or_else(|| malformed(lineno, line))?;
            let size = columns.next().ok_or_else(|| malformed(lineno, line))?;

            // The tail is an optional type token and an optional `uuid=<GUID>`
            // attribute, in either order: a `key=value` token is an attribute, a
            // bare token is the type, and a second of either is the person's mistake.
            let mut kind = None;
            let mut unique_guid = None;
            for token in columns {
                if let Some(value) = token.strip_prefix("uuid=") {
                    if unique_guid.replace(value.to_string()).is_some() {
                        return Err(Error::InvalidRequest(format!(
                            "layout line {lineno}: '{name}' has more than one 'uuid=' attribute"
                        )));
                    }
                } else if token.contains('=') {
                    return Err(Error::InvalidRequest(format!(
                        "layout line {lineno}: '{token}' is not a column of 'name first_lba sectors \
                         [type] [uuid=<GUID>]'. The only attribute is 'uuid='"
                    )));
                } else if kind.replace(token.to_string()).is_some() {
                    return Err(Error::InvalidRequest(format!(
                        "layout line {lineno}: '{name}' has more than one type token"
                    )));
                }
            }

            let first_lba = number(first, lineno)?;
            if first_lba >= flash_sectors {
                return Err(Error::InvalidRequest(format!(
                    "layout line {lineno}: '{name}' begins at sector {first_lba}, and the device \
                     has {flash_sectors} sectors"
                )));
            }

            let sectors = if size == "-" {
                // Grow to `grow_end`. A partition that begins at or past it has
                // nothing to grow into.
                grow_end
                    .checked_sub(first_lba)
                    .filter(|&sectors| sectors > 0)
                    .ok_or_else(|| {
                        Error::InvalidRequest(format!(
                            "layout line {lineno}: '{name}' grows to fill the part from sector \
                             {first_lba}, and the part ends at sector {grow_end}"
                        ))
                    })?
            } else {
                let sectors = number(size, lineno)?;
                if sectors == 0 {
                    return Err(Error::InvalidRequest(format!(
                        "layout line {lineno}: '{name}' is zero sectors long"
                    )));
                }
                sectors
            };

            partitions.push(LayoutPartition {
                name: name.to_string(),
                first_lba,
                sectors,
                kind,
                unique_guid,
            });
        }

        if partitions.is_empty() {
            return Err(Error::InvalidRequest(
                "the layout names no partitions".to_string(),
            ));
        }

        Ok(Layout {
            partitions,
            disk_guid,
        })
    }

    /// Check that the layout is one a fresh table can hold on a part of
    /// `flash_sectors` sectors.
    ///
    /// This runs [`check_placement`] over the layout's own partitions.
    /// [`check_placement`] is a free function over `&[Partition]`, because the check
    /// concerns where partitions sit, not where their description came from.
    /// Partitions with no layout, such as those of a parameter block authored from
    /// an existing block's verbatim text, are checked by calling it directly.
    pub fn validate(&self, flash_sectors: u64) -> Result<()> {
        check_placement(&self.as_partitions(), flash_sectors)
    }

    /// The layout's partitions in the neutral shape the placement check reads.
    ///
    /// The type token and any pinned GUID say nothing about where a partition sits,
    /// so the result omits them.
    pub fn as_partitions(&self) -> Vec<Partition> {
        self.partitions
            .iter()
            .map(|part| Partition {
                name: part.name.clone(),
                first_lba: part.first_lba,
                sectors: part.sectors,
            })
            .collect()
    }
}

/// Check that a set of partitions is one a fresh table can hold on a part of
/// `flash_sectors` sectors.
///
/// Every partition must have a nonzero length and a unique name, and must fit. No
/// partition can overlap another. A set that fails any of these checks returns
/// [`Error::InvalidRequest`].
///
/// Authoring lays down what it is given. A set that overlaps itself would be a
/// table with two partitions claiming one sector. A partition that runs off the end
/// is one the device cannot hold. The plan refuses both, so the person's mistake is
/// caught before a write and not discovered by the device during one.
///
/// Two more checks are made here, not in either authoring path, so that one check
/// holds for both formats:
///
/// - **A partition of zero sectors.** [`gpt::author`] refuses one, so a GPT is
///   safe without this check. A parameter block is not, and
///   `rkparam::render_mtdparts` would write `0x0@0x...(name)` into it.
/// - **Two partitions with the same name.** A name is how a write is aimed safely,
///   because it is the only form that knows where a partition ends.
///   [`PartitionTable::find`] refuses an ambiguous name once the table is read
///   back. Refusing the duplicate here keeps such a table from reaching the board.
///
/// Overlap is judged in sector order, so the error names the two partitions that
/// collide, whatever order they were listed in.
///
/// [`gpt::author`]: crate::codec::gpt::author
/// [`PartitionTable::find`]: crate::partition::PartitionTable::find
pub fn check_placement(partitions: &[Partition], flash_sectors: u64) -> Result<()> {
    for (index, part) in partitions.iter().enumerate() {
        if part.sectors == 0 {
            return Err(Error::InvalidRequest(format!(
                "'{}' is zero sectors long. A partition must span at least one sector",
                part.name
            )));
        }
        if let Some(other) = partitions[..index]
            .iter()
            .find(|other| other.name == part.name)
        {
            return Err(Error::InvalidRequest(format!(
                "'{}' is named twice. A write aimed by partition name needs each name to be \
                 unique",
                other.name
            )));
        }

        let end = part.first_lba.checked_add(part.sectors).ok_or_else(|| {
            Error::InvalidRequest(format!(
                "'{}' runs past any sector that can be counted",
                part.name
            ))
        })?;
        if end > flash_sectors {
            return Err(Error::InvalidRequest(format!(
                "'{}' ends at sector {end}, past the {flash_sectors} sectors the device has",
                part.name
            )));
        }
    }

    // In sector order, each partition must begin at or after the one before it
    // ends. Sorting references rather than the partitions themselves keeps the
    // caller's own order for the plan a person reads.
    let mut order: Vec<&Partition> = partitions.iter().collect();
    order.sort_by_key(|part| part.first_lba);
    for pair in order.windows(2) {
        let (before, after) = (pair[0], pair[1]);
        if after.first_lba < before.first_lba + before.sectors {
            return Err(Error::InvalidRequest(format!(
                "'{}' (sectors {}..{}) and '{}' (from sector {}) overlap. Two partitions in an \
                 authored layout cannot share a sector",
                before.name,
                before.first_lba,
                before.first_lba + before.sectors,
                after.name,
                after.first_lba
            )));
        }
    }

    Ok(())
}

/// A layout line that is not a partition.
fn malformed(lineno: usize, line: &str) -> Error {
    Error::InvalidRequest(format!(
        "layout line {lineno} is not a partition: a line is 'name first_lba sectors [type] \
         [uuid=<GUID>]', the first sector and the size in hex or decimal sectors: '{line}'"
    ))
}

/// Read one sector count or offset from a native layout line: hexadecimal behind a
/// `0x`, decimal otherwise.
fn number(field: &str, lineno: usize) -> Result<u64> {
    let parsed = match field
        .strip_prefix("0x")
        .or_else(|| field.strip_prefix("0X"))
    {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => field.parse::<u64>(),
    };

    parsed.map_err(|_| {
        Error::InvalidRequest(format!(
            "layout line {lineno}: '{field}' is not a number of sectors"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLASH_SECTORS: u64 = 122_142_720;

    /// The native format parses into a layout, in absolute sectors, with hex and
    /// decimal both read and the optional type column kept.
    #[test]
    fn the_native_format_parses_into_a_layout() {
        let layout = Layout::parse_native(
            "# a board's layout\n\
             uboot   0x4000   0x2000\n\
             trust   0x6000   8192\n\
             rootfs  0x8000   -        linux\n",
            FLASH_SECTORS,
        )
        .expect("a well-formed layout");

        assert_eq!(layout.partitions.len(), 3);
        assert_eq!(layout.partitions[0].name, "uboot");
        assert_eq!(layout.partitions[0].first_lba, 0x4000);
        assert_eq!(layout.partitions[0].sectors, 0x2000);
        // Decimal is read as decimal.
        assert_eq!(layout.partitions[1].sectors, 8192);
        // `-` grows to the end of the part, and the type column is kept.
        assert_eq!(
            layout.partitions[2].first_lba + layout.partitions[2].sectors,
            FLASH_SECTORS
        );
        assert_eq!(layout.partitions[2].kind.as_deref(), Some("linux"));
        assert_eq!(layout.partitions[0].kind, None);
        // No GUID overrides were written, so both halves are left to synthesis.
        assert_eq!(layout.disk_guid, None);
        assert_eq!(layout.partitions[2].unique_guid, None);
    }

    /// The GUID overrides parse, table-level and per-partition. A `disk-guid`
    /// directive pins the table's disk GUID, and a `uuid=` attribute pins a
    /// partition's. The type and the attribute are read in either order. All are
    /// kept as the raw tokens a person wrote, for GPT authoring to parse.
    #[test]
    fn the_native_format_parses_guid_overrides() {
        let layout = Layout::parse_native(
            "disk-guid  01234567-89AB-CDEF-0123-456789ABCDEF\n\
             uboot   0x4000   0x2000\n\
             rootfs  0x8000   -    linux  uuid=0FC63DAF-8483-4772-8E79-3D69D8477DE4\n\
             spare   0x6000   0x800  uuid=11111111-2222-3333-4444-555555555555  esp\n",
            FLASH_SECTORS,
        )
        .expect("a well-formed layout with overrides");

        assert_eq!(
            layout.disk_guid.as_deref(),
            Some("01234567-89AB-CDEF-0123-456789ABCDEF")
        );
        assert_eq!(layout.partitions[0].unique_guid, None, "uboot pins nothing");
        assert_eq!(
            layout.partitions[1].unique_guid.as_deref(),
            Some("0FC63DAF-8483-4772-8E79-3D69D8477DE4")
        );
        assert_eq!(layout.partitions[1].kind.as_deref(), Some("linux"));
        // The type and the attribute in the other order still read right.
        assert_eq!(layout.partitions[2].kind.as_deref(), Some("esp"));
        assert_eq!(
            layout.partitions[2].unique_guid.as_deref(),
            Some("11111111-2222-3333-4444-555555555555")
        );
    }

    /// A malformed directive or attribute is refused with its line, rather than
    /// half-read. The refused cases are these:
    ///
    /// - A `disk-guid` with no value, with two values, or given twice
    /// - An unknown attribute
    /// - Two types, or two `uuid=` attributes, on one partition
    #[test]
    fn a_malformed_override_is_refused() {
        let refuses = |text: &str| {
            matches!(
                Layout::parse_native(text, FLASH_SECTORS),
                Err(Error::InvalidRequest(_))
            )
        };

        assert!(refuses("disk-guid\nboot 0x4000 0x100\n"), "no value");
        assert!(refuses("disk-guid A B\nboot 0x4000 0x100\n"), "two values");
        assert!(
            refuses("disk-guid A\ndisk-guid B\nboot 0x4000 0x100\n"),
            "given twice"
        );
        assert!(
            refuses("boot 0x4000 0x100 type=linux\n"),
            "an unknown attribute"
        );
        assert!(
            refuses("boot 0x4000 0x100 uuid=A uuid=B\n"),
            "two uuid attributes"
        );
        assert!(refuses("boot 0x4000 0x100 linux ext4\n"), "two type tokens");
        assert!(
            !refuses("boot 0x4000 0x100 linux uuid=A\n"),
            "a type and one uuid parse"
        );
    }

    /// Comments and blank lines are not partitions, and a layout with no partition
    /// in it is refused rather than returned empty.
    #[test]
    fn comments_and_blank_lines_are_not_partitions() {
        let error = Layout::parse_native("# only a comment\n\n   \n", FLASH_SECTORS)
            .expect_err("nothing but comments and blanks");
        assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
    }

    /// A malformed line is refused with the line number, rather than half-parsed.
    /// Too few columns, a bad number and too many columns are each malformed.
    #[test]
    fn a_malformed_native_line_is_refused() {
        let refuses = |text: &str| {
            matches!(
                Layout::parse_native(text, FLASH_SECTORS),
                Err(Error::InvalidRequest(_))
            )
        };

        assert!(refuses("uboot 0x4000\n"), "no size");
        assert!(refuses("uboot zzz 0x2000\n"), "first sector not a number");
        assert!(refuses("uboot 0x4000 zzz\n"), "size not a number");
        assert!(
            refuses("uboot 0x4000 0x2000 linux extra\n"),
            "a fifth column"
        );
        assert!(refuses("uboot 0x4000 0\n"), "a zero-length partition");
        assert!(
            !refuses("uboot 0x4000 0x2000\n"),
            "the well-formed line parses"
        );
    }

    /// The mtdparts front-end shares the board's own parser. An `mtdparts` line
    /// resolves to the same absolute layout the board's table does, with the base
    /// added to each offset. The two front-ends therefore produce the same
    /// [`Layout`] for the same partitions.
    #[test]
    fn the_mtdparts_front_end_resolves_to_absolute_sectors() {
        // The same partitions, one way as mtdparts against the eMMC base, one way
        // as the native format in the absolute sectors that base resolves to.
        let from_mtdparts = Layout::parse_mtdparts(
            "mtdparts=rk29xxnand:0x2000@0x2000(uboot),0x2000@0x4000(trust)",
            rkparam::EMMC_BASE_LBA,
            FLASH_SECTORS,
        )
        .expect("a well-formed mtdparts line");

        assert_eq!(from_mtdparts.partitions[0].name, "uboot");
        // 0x2000 offset + 0x2000 base = 0x4000, absolute.
        assert_eq!(from_mtdparts.partitions[0].first_lba, 0x4000);
        assert_eq!(from_mtdparts.partitions[1].first_lba, 0x6000);

        let from_native =
            Layout::parse_native("uboot 0x4000 0x2000\ntrust 0x6000 0x2000\n", FLASH_SECTORS)
                .unwrap();

        // The two front-ends agree, but for the type token the native format can
        // carry and mtdparts cannot.
        let names_and_ranges = |layout: &Layout| {
            layout
                .partitions
                .iter()
                .map(|p| (p.name.clone(), p.first_lba, p.sectors))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names_and_ranges(&from_mtdparts),
            names_and_ranges(&from_native)
        );
    }

    /// A layout read for a GPT grows its `-` partition to the GPT's last usable
    /// sector, in both formats. The table authored from it is accepted, where the
    /// same layout grown to the last sector runs into the backup and is refused.
    #[test]
    fn a_growing_partition_stops_where_a_gpt_backup_begins() {
        use crate::codec::gpt;
        let grow_end = gpt::grow_end(FLASH_SECTORS, 512).expect("room for a table");
        assert_eq!(grow_end, FLASH_SECTORS - 33, "the backup array and header");

        let native = Layout::parse_native_growing_to(
            "boot 0x4000 0x2000\nrootfs 0x8000 -\n",
            FLASH_SECTORS,
            grow_end,
        )
        .expect("a well-formed layout");
        let mtdparts = Layout::parse_mtdparts_growing_to(
            "rk29xxnand:0x2000@0x4000(boot),-@0x8000(rootfs)",
            0,
            FLASH_SECTORS,
            grow_end,
        )
        .expect("a well-formed mtdparts line");
        for layout in [&native, &mtdparts] {
            let rootfs = &layout.partitions[1];
            assert_eq!(rootfs.first_lba + rootfs.sectors, grow_end);
            crate::verbs::author_gpt(layout, FLASH_SECTORS, 512).expect("a GPT fits it");
        }

        let to_the_end =
            Layout::parse_native("boot 0x4000 0x2000\nrootfs 0x8000 -\n", FLASH_SECTORS).unwrap();
        assert!(
            crate::verbs::author_gpt(&to_the_end, FLASH_SECTORS, 512).is_err(),
            "grown to the last sector, it overlaps the backup"
        );
    }

    /// A partition marked to grow that begins at or past the end it grows to has
    /// nothing to grow into, and is refused with its line.
    #[test]
    fn a_growing_partition_with_nothing_to_grow_into_is_refused() {
        let error = Layout::parse_native_growing_to("rootfs 0x1000 -\n", FLASH_SECTORS, 0x1000)
            .expect_err("it begins where the growth ends");
        assert!(
            matches!(&error, Error::InvalidRequest(why) if why.contains("line 1")),
            "{error:?}"
        );
    }

    /// A whole `CMDLINE:` line, or a bare `rk29xxnand:` list, is accepted too. The
    /// front-end finds the `mtdparts` token wherever it is, or takes the input as
    /// the list itself.
    #[test]
    fn the_mtdparts_front_end_accepts_a_bare_list_or_a_whole_cmdline() {
        let bare = Layout::parse_mtdparts("rk29xxnand:0x100@0x200(boot)", 0, FLASH_SECTORS)
            .expect("a bare list");
        assert_eq!(bare.partitions[0].first_lba, 0x200);

        let cmdline = Layout::parse_mtdparts(
            "CMDLINE: console=ttyFIQ0 mtdparts=rk29xxnand:0x100@0x200(boot) root=/dev/mmcblk0p1",
            0,
            FLASH_SECTORS,
        )
        .expect("a whole command line");
        assert_eq!(cmdline.partitions[0].first_lba, 0x200);
    }

    /// A layout whose partitions overlap is refused: a fresh table cannot have two
    /// partitions on one sector, and the error names the two that collide.
    #[test]
    fn an_overlapping_layout_is_refused() {
        let layout = Layout::parse_native(
            "uboot 0x4000 0x2000\ntrust 0x5000 0x2000\n", // trust starts inside uboot
            FLASH_SECTORS,
        )
        .unwrap();

        let error = layout.validate(FLASH_SECTORS).expect_err("they overlap");
        let Error::InvalidRequest(detail) = error else {
            panic!("{error:?}");
        };
        assert!(
            detail.contains("uboot") && detail.contains("trust"),
            "{detail}"
        );
    }

    /// A partition of zero sectors is refused at the layout, where both authoring
    /// paths get the check. `gpt::author` refuses one on its own. A parameter
    /// block would take `0x0@0x...(name)` without complaint.
    #[test]
    fn a_zero_length_partition_is_refused() {
        let layout = Layout {
            partitions: vec![LayoutPartition {
                name: "empty".to_string(),
                first_lba: 0x1000,
                sectors: 0,
                kind: None,
                unique_guid: None,
            }],
            disk_guid: None,
        };
        let error = layout
            .validate(FLASH_SECTORS)
            .expect_err("a partition of no length is not a partition");
        let Error::InvalidRequest(detail) = &error else {
            panic!("{error:?}");
        };
        assert!(detail.contains("empty"), "{detail}");
    }

    /// Two partitions with one name would author a table whose partitions cannot
    /// be aimed at by name. Aiming by name is the only form of a write that knows
    /// where a partition ends. The duplicate is refused before the write.
    /// Otherwise it is discovered on the board, where `PartitionTable::find`
    /// refuses the name as ambiguous.
    #[test]
    fn two_partitions_with_the_same_name_are_refused() {
        let layout = Layout {
            partitions: vec![
                LayoutPartition {
                    name: "boot".to_string(),
                    first_lba: 0x1000,
                    sectors: 0x1000,
                    kind: None,
                    unique_guid: None,
                },
                LayoutPartition {
                    name: "boot".to_string(),
                    first_lba: 0x2000,
                    sectors: 0x1000,
                    kind: None,
                    unique_guid: None,
                },
            ],
            disk_guid: None,
        };
        let error = layout
            .validate(FLASH_SECTORS)
            .expect_err("a table cannot answer to an ambiguous name");
        let Error::InvalidRequest(detail) = &error else {
            panic!("{error:?}");
        };
        assert!(detail.contains("boot"), "{detail}");
    }

    /// A layout that runs off the end of the part is refused: a partition the
    /// device cannot hold.
    #[test]
    fn a_layout_past_the_end_of_the_part_is_refused() {
        let layout = Layout {
            partitions: vec![LayoutPartition {
                name: "rootfs".to_string(),
                first_lba: FLASH_SECTORS - 100,
                sectors: 200,
                kind: None,
                unique_guid: None,
            }],
            disk_guid: None,
        };
        assert!(matches!(
            layout.validate(FLASH_SECTORS),
            Err(Error::InvalidRequest(_))
        ));

        // Exactly to the end fits.
        let exact = Layout {
            partitions: vec![LayoutPartition {
                name: "rootfs".to_string(),
                first_lba: FLASH_SECTORS - 100,
                sectors: 100,
                kind: None,
                unique_guid: None,
            }],
            disk_guid: None,
        };
        exact
            .validate(FLASH_SECTORS)
            .expect("to the last sector fits");
    }

    /// Partitions flush against one another do not overlap. An off-by-one in the
    /// check would refuse the ordinary layout, where one partition begins exactly
    /// where the previous one ends.
    #[test]
    fn partitions_flush_against_each_other_do_not_overlap() {
        let layout =
            Layout::parse_native("a 0x1000 0x1000\nb 0x2000 0x1000\n", FLASH_SECTORS).unwrap();
        layout
            .validate(FLASH_SECTORS)
            .expect("b begins where a ends");
    }
}
