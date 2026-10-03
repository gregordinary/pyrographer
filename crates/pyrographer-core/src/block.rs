//! The Block backend: a raw block device the host operating system also owns.
//!
//! Every other backend in this set drives a board with one host. A bootloader in
//! maskrom, a DFU gadget and a ROM on a serial line each run one conversation with
//! that host. A write read back from one of them is what the device holds.
//!
//! A block device is different, because the operating system also uses it. It
//! mounts filesystems on it, caches its sectors, and writes its own bytes back over
//! them on its own schedule. **A verified write is not a durable one.**
//!
//! This is measured on Windows. A raw write to a mounted ext4's superblock was
//! confirmed through an uncached read-back. The kernel's cached copy then partly
//! overwrote it, leaving a mixture of the two on the device.
//!
//! This module implements three mechanisms in response:
//!
//! - **Ask the kernel to exclude everything else.** `O_EXCL` on a block device
//!   fails with `EBUSY` while the kernel holds it, and the kernel enforces it. It
//!   is broader than any check this code can make. On the bench, it refused a LUKS
//!   container with a volume stacked on it, and two active swap devices, none of
//!   which carry a mount.
//! - **Bypass the cache**, with `O_DIRECT`, so the device answers the read-back,
//!   not a page the host is holding.
//! - **Refuse the disks the running system rests on**, transitively, with no
//!   override, as [`sysfs::physical_disks_under`] describes.
//!
//! It is native and Linux only. The parsing and the guard are pure, and are in
//! [`sysfs`]. This module holds the open and the I/O.

pub mod sysfs;

use crate::agent::{Caps, FlashInfo};
use crate::{Error, Result};
use std::io::{Read, Seek, SeekFrom, Write};

pub use sysfs::Bus;

/// The default window a read or a write moves in one call, in bytes.
///
/// It is large enough that the per-call cost disappears, and small enough that the
/// aligned bounce buffer it needs is no burden. A verb chooses its own window. The
/// agent sizes its buffer from this one at the start.
pub const DEFAULT_WINDOW: usize = 1 << 20;

/// One block device, as the operating system describes it, with nothing opened.
///
/// Everything here is readable **without privilege** on Linux, so a listing of
/// disks, each with its capacity and its refusal, is drawn for anybody. On Windows,
/// capacity alone costs administrator. A plan needs privilege on both, because it
/// reads the device's own partition table through the exclusive handle the write
/// then uses.
#[derive(Debug, Clone)]
pub struct BlockDevice {
    /// The kernel name: `sda`, `nvme0n1`, `loop0`.
    pub name: String,
    /// The path the device is opened through.
    pub node: String,
    /// Capacity in bytes.
    pub bytes: u64,
    /// The logical block size: the unit every read and write is a multiple of.
    pub logical_block: u32,
    /// The physical block size, where the device reports one. A 512e disk reports
    /// 512 logical and 4096 physical. Windows cannot obtain this at all over USB,
    /// and on Linux it is an ordinary attribute.
    pub physical_block: u32,
    /// The removable bit, which is **independent of the bus**: a USB-attached
    /// disk can report `removable false`, and a board in mass-storage mode can
    /// be one. It decides whether the operating system creates a volume for
    /// unpartitioned media, not what the device is attached by.
    pub removable: bool,
    /// Whether the kernel marks the device read-only.
    pub read_only: bool,
    /// The bus the device is attached through.
    pub bus: Bus,
    /// Vendor and model, where the device carries them.
    pub model: Option<String>,
    /// The mount point of each of this device's partitions that is mounted.
    pub mounts: Vec<String>,
    /// Whether this device is one the running system rests on, transitively.
    ///
    /// It drives the no-override refusal. It is `true` for the disk an
    /// LVM-over-LUKS root is built on, as it is for a disk mounted directly.
    /// [`sysfs::physical_disks_under`] walks the stack to the hardware.
    pub carries_running_system: bool,
}

impl BlockDevice {
    /// Whether anything is mounted on this device or its partitions.
    ///
    /// It serves a report, not a gate. The gate is [`open`], which asks the
    /// kernel with `O_EXCL` rather than deciding for itself. This lets a plan
    /// state what is mounted before a person confirms it.
    pub fn is_mounted(&self) -> bool {
        !self.mounts.is_empty()
    }
}

/// What a caller intends to do with a block device, decided at the open.
///
/// The host operating system owns this device, and the two intentions carry
/// different risks. A read cannot change the device. An SD card with its lock
/// switch on cannot be changed at all, and imaging one is a typical use of
/// pyrographer. A write carries the risk, and the refusals in [`write_refusal`]
/// apply to it.
///
/// The open therefore takes the intention, rather than assuming the larger one. An
/// open that assumed a write would take `O_RDWR` whatever the verb, and the kernel
/// refuses that outright on read-only media. Every read of a write-protected card
/// would then be refused, with a sentence about a write nobody asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Read the device and nothing else: `dump`, `verify`, `partitions`.
    ///
    /// The write refusals are not asked, because there is no write. The kernel is
    /// still asked for the device exclusively, so the device does not change during
    /// the read.
    Read,
    /// Read and write it: `flash`, `clone`'s destination, a table write.
    ///
    /// [`write_refusal`] is asked before the device is opened at all, so the disks
    /// the running system rests on are refused pre-emptively and with no override.
    Write,
}

impl Access {
    /// Whether this access may write.
    pub fn writes(self) -> bool {
        matches!(self, Access::Write)
    }

    /// The widest access `device` allows: [`Write`](Access::Write), unless
    /// something would refuse a write to it.
    ///
    /// A front-end that opens a device once and then offers several verbs on it
    /// asks for this, and both front-ends do. A device refused a write still opens
    /// for reading, and the write is refused where a person can be told why.
    /// [`BlockAgent::write_refusal`] passes the sentence to
    /// [`verbs::write_refusal`](crate::verbs::write_refusal), and a front-end grays
    /// the button out with it.
    pub fn widest_for(device: &BlockDevice) -> Access {
        match write_refusal(device) {
            None => Access::Write,
            Some(_) => Access::Read,
        }
    }
}

/// A raw block device, opened for exclusive use.
///
/// Only [`open`] and [`open_for`] construct one outside the `testing` feature, and
/// both make the refusals. Holding one means the kernel granted the device
/// exclusively.
///
/// An agent opened for [`Access::Write`] has also passed every refusal in
/// [`write_refusal`], so it is not open on a disk the running system rests on. An
/// agent opened for [`Access::Read`] has not asked them, because there is no write
/// to refuse. Its [`write_refusal`](BlockAgent::write_refusal) says so.
pub struct BlockAgent {
    /// The open device. `O_EXCL` so nothing else holds it, `O_DIRECT` so a
    /// read-back is answered by the device.
    file: std::fs::File,
    /// What the device is, read before it was opened.
    device: BlockDevice,
    /// An aligned scratch buffer. `O_DIRECT` requires the buffer, the offset and
    /// the length all to be multiples of the logical block size. A `Vec<u8>` a
    /// verb passes in has no such alignment.
    bounce: AlignedBuf,
    /// Set after a command fails in a way that leaves the device's state
    /// unknown to this agent.
    desynchronized: bool,
    /// What this agent was opened for. A [`Read`](Access::Read) agent's file has
    /// no write permission at all. A write through it would fail with the operating
    /// system's error, not with the sentence that says why.
    access: Access,
}

impl BlockAgent {
    /// The device this agent was opened on.
    pub fn device(&self) -> &BlockDevice {
        &self.device
    }

    /// What this agent was opened for.
    pub fn access(&self) -> Access {
        self.access
    }

    /// Why a write through this agent would be refused, or `None`.
    ///
    /// It is the Block backend's part of [`verbs::write_refusal`]. A device that
    /// will not take a write opens for reading, and reports the refusal here. A
    /// front-end then grays the write out with the sentence [`write_refusal`]
    /// produced, and still offers the device's reads.
    ///
    /// `None` means the agent was opened for writing. Every refusal in
    /// [`write_refusal`] was then already asked and passed, and the kernel granted
    /// the device exclusively. It does not promise that the write lands.
    ///
    /// [`verbs::write_refusal`]: crate::verbs::write_refusal
    pub fn write_refusal(&self) -> Option<String> {
        if self.access.writes() {
            return None;
        }
        Some(write_refusal(&self.device).unwrap_or_else(|| {
            format!(
                "`{}` was opened for reading only, so this agent cannot write to it",
                self.device.name
            )
        }))
    }

    /// The logical block size, which is the sector size this backend addresses
    /// in. Unlike every other backend here, it is a property of the device
    /// rather than a constant of the protocol.
    pub fn sector_size(&self) -> u32 {
        self.device.logical_block
    }

    /// Whether this agent is desynchronized from its device.
    pub fn is_desynchronized(&self) -> bool {
        self.desynchronized
    }

    /// The geometry, which was known before the device was opened.
    ///
    /// Unlike every other backend here, this needs no command. sysfs answered it
    /// unprivileged, before anything was opened. [`open`] refuses a description
    /// whose numbers have gone stale.
    pub fn info(&self) -> FlashInfo {
        FlashInfo {
            size_bytes: self.device.bytes,
            sector_size: self.device.logical_block,
            medium: None,
            // A block device has no flash chip to identify. What identifies one
            // here is its model string, which is not a chip id and is not
            // offered as one.
            chip_id: None,
        }
    }

    /// What this backend supports.
    ///
    /// It reads, so it verifies. It addresses the whole device by LBA, so a raw
    /// dump and a clone both cover the device. It does not erase. A block device has
    /// no erase pyrographer drives, and a write already overwrites.
    pub fn caps(&self) -> Caps {
        Caps {
            can_erase: false,
            has_partitions: true,
            medium: None,
            can_verify: true,
            can_address_raw_lba: true,
        }
    }
}

/// A buffer aligned to a block boundary, which `O_DIRECT` requires.
///
/// It over-allocates and takes an aligned window from inside the allocation. Every
/// allocator API that gives alignment is unsafe, and this crate forbids unsafe code
/// outright. The cost is one extra block of memory and a `memcpy` per window, and
/// the I/O costs far more than either.
struct AlignedBuf {
    storage: Vec<u8>,
    start: usize,
    len: usize,
    align: usize,
}

impl AlignedBuf {
    /// A buffer of at least `len` bytes, aligned to `align`.
    ///
    /// The compiler reports it as dead code on any target with no [`open`], such
    /// as a browser or a platform with no block layer built. Nothing there can
    /// construct the agent that holds one. The allow is scoped to this function,
    /// not the type, so that an unused field still produces a warning.
    #[cfg_attr(
        not(all(target_os = "linux", not(target_arch = "wasm32"))),
        allow(dead_code)
    )]
    fn new(len: usize, align: usize) -> AlignedBuf {
        let mut buf = AlignedBuf {
            storage: Vec::new(),
            start: 0,
            len: 0,
            align,
        };
        buf.ensure(len);
        buf
    }

    /// Grow to hold at least `len` bytes, re-finding the aligned window.
    ///
    /// The window is recomputed on every growth. A `Vec`'s buffer moves on
    /// reallocation, and an offset cached across that move would point at the
    /// wrong byte.
    fn ensure(&mut self, len: usize) {
        if self.len >= len && !self.storage.is_empty() {
            return;
        }
        self.storage = vec![0u8; len + self.align];
        let addr = self.storage.as_ptr() as usize;
        self.start = (self.align - (addr % self.align)) % self.align;
        self.len = len;
    }

    fn as_slice(&self, len: usize) -> &[u8] {
        &self.storage[self.start..self.start + len]
    }

    fn as_mut_slice(&mut self, len: usize) -> &mut [u8] {
        &mut self.storage[self.start..self.start + len]
    }
}

/// Check a request against the device's geometry, before any I/O.
///
/// It is pure, and separate from the agent, so it can be tested without a block
/// device. This arithmetic decides whether a write runs off the end of a disk. The
/// disks of the machine running the tests must never be written.
///
/// It refuses three things, each with [`Error::InvalidRequest`]:
///
/// - A device that reports a zero logical block size, to which no request can be
///   aligned.
/// - A length that is not a whole number of sectors. Uncached access requires
///   every transfer to be a multiple of the logical block size. A partial one
///   fails at the syscall with nothing useful said.
/// - A range that ends past the device. That is computed in `u64` with checked
///   arithmetic, because a length that wrapped would turn a refusal into a
///   permission.
pub fn check_range(name: &str, lba: u64, len: usize, block: usize, size_bytes: u64) -> Result<()> {
    if block == 0 {
        return Err(Error::InvalidRequest(format!(
            "`{name}` reports a zero logical block size, so no request can be aligned to it"
        )));
    }
    if !len.is_multiple_of(block) {
        return Err(Error::InvalidRequest(format!(
            "{len} bytes is not a whole number of {block}-byte sectors, and uncached access \
             requires every transfer to be a multiple of the logical block size"
        )));
    }
    let sectors = (len / block) as u64;
    let end = lba.checked_add(sectors).ok_or_else(|| {
        Error::InvalidRequest(format!(
            "a {len}-byte request at sector {lba} overflows a 64-bit sector count"
        ))
    })?;
    let total = size_bytes / block as u64;
    if end > total {
        return Err(Error::InvalidRequest(format!(
            "sectors {lba}..{end} run past the end of `{name}`, which has {total}"
        )));
    }
    Ok(())
}

/// Why a write to this device would be refused before anything is opened.
///
/// It is the Block backend's counterpart of the wrong-loader gate, for a different
/// danger. There is no loader to be wrong here. What destroys a machine is writing
/// the disk it is running on. A front-end asks this before any plan, so it can gray
/// a button and say why. It never collects a confirmation for a write that will be
/// refused.
///
/// `None` does not promise the write goes through. The kernel still has to agree
/// that nothing else holds the device, and only [`open`] can ask that.
pub fn write_refusal(device: &BlockDevice) -> Option<String> {
    if device.carries_running_system {
        return Some(format!(
            "`{}` holds the running system, so pyrographer will not write to it. A write would \
             corrupt this machine without reporting an error at the time. There is no override",
            device.name
        ));
    }
    if device.read_only {
        return Some(format!(
            "`{}` is marked read-only by the kernel, so it cannot be written",
            device.name
        ));
    }
    None
}

impl BlockAgent {
    /// Fill `buf` from logical block `lba`.
    pub fn read_at(&mut self, lba: u64, buf: &mut [u8]) -> Result<()> {
        let block = self.device.logical_block as usize;
        self.check(lba, buf.len(), block)?;

        self.bounce.ensure(buf.len());
        self.seek(lba, block)?;
        let len = buf.len();
        match self.file.read_exact(self.bounce.as_mut_slice(len)) {
            Ok(()) => {
                buf.copy_from_slice(self.bounce.as_slice(len));
                Ok(())
            }
            Err(e) => {
                self.desynchronized = true;
                Err(Error::Io(format!(
                    "reading {len} bytes at sector {lba} of `{}`: {e}",
                    self.device.name
                )))
            }
        }
    }

    /// Write `data` at logical block `lba`.
    ///
    /// This is the raw write, with **no gate and no read-back**. A caller writing
    /// an image uses `verbs::flash`.
    pub fn write_at(&mut self, lba: u64, data: &[u8]) -> Result<()> {
        // An agent opened for reading holds a file with no write permission, so
        // this would fail in the operating system's words -- a bad file
        // descriptor -- rather than in the ones that say what is actually wrong.
        if let Some(why) = self.write_refusal() {
            return Err(Error::InvalidRequest(why));
        }
        let block = self.device.logical_block as usize;
        self.check(lba, data.len(), block)?;

        self.bounce.ensure(data.len());
        let len = data.len();
        self.bounce.as_mut_slice(len).copy_from_slice(data);
        self.seek(lba, block)?;
        match self.file.write_all(self.bounce.as_slice(len)) {
            Ok(()) => Ok(()),
            Err(e) => {
                self.desynchronized = true;
                Err(Error::Io(format!(
                    "writing {len} bytes at sector {lba} of `{}`: {e}",
                    self.device.name
                )))
            }
        }
    }

    /// Push everything through to the device.
    ///
    /// `O_DIRECT` bypasses the page cache but not the device's own write
    /// cache. This call makes a write durable, not merely submitted.
    pub fn flush(&mut self) -> Result<()> {
        self.file
            .sync_data()
            .map_err(|e| Error::Io(format!("flushing `{}`: {e}", self.device.name)))
    }

    /// Refuse a request that is misaligned or runs past the end.
    ///
    /// The range check is against the geometry sysfs reported. It is made in
    /// `u64` with checked arithmetic, because a length that wrapped would turn a
    /// refusal into a permission.
    fn check(&self, lba: u64, len: usize, block: usize) -> Result<()> {
        if self.desynchronized {
            return Err(Error::Desynchronized);
        }
        check_range(&self.device.name, lba, len, block, self.device.bytes)
    }

    /// Seek to a sector, in bytes.
    fn seek(&mut self, lba: u64, block: usize) -> Result<()> {
        let offset = lba
            .checked_mul(block as u64)
            .ok_or_else(|| Error::InvalidRequest("the offset overflows".to_string()))?;
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(|e| Error::Io(format!("seeking to sector {lba}: {e}")))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The native half: reading sysfs, and opening a device
// ---------------------------------------------------------------------------

/// Linux, natively. Every other target compiles this file's portable types but can
/// build no agent. A browser has no block layer. macOS and Windows answer this
/// backend's questions in their own ways, and this module implements neither.
#[cfg(all(target_os = "linux", not(target_arch = "wasm32")))]
mod linux {
    use super::sysfs::{self, Bus, Topology};
    use super::{Access, AlignedBuf, BlockAgent, BlockDevice, DEFAULT_WINDOW};
    use crate::{Error, Result};
    use std::collections::BTreeMap;
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::{Path, PathBuf};

    /// `O_EXCL`. On a **block device**, Linux gives this flag its own meaning,
    /// unrelated to the file-creation flag of the same name. The open fails with
    /// `EBUSY` in either case:
    ///
    /// - The kernel holds the device: it is mounted, another device is built on
    ///   it, or it is serving as swap.
    /// - Another opener holds it `O_EXCL`.
    ///
    /// This flag alone provides the exclusive open, and it is broader than any
    /// check this code can make. Measured: it refused a LUKS container with a
    /// volume stacked on it, and two active swap devices. None of them carries a
    /// mount that any code here could have consulted.
    const O_EXCL: i32 = 0o200;

    /// `O_DIRECT`, which bypasses the page cache.
    ///
    /// Without it, a read-back can be answered from a page the host is holding,
    /// which proves nothing about what reached the device. That failure makes a
    /// Windows verify meaningless. The value is architecture-specific. x86 has
    /// its own, and every architecture on `asm-generic` shares another.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    const O_DIRECT: i32 = 0o40000;
    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
    const O_DIRECT: i32 = 0o200000;

    /// Read a sysfs attribute, trimmed.
    fn attr(base: &Path, name: &str) -> Option<String> {
        std::fs::read_to_string(base.join(name))
            .ok()
            .map(|s| s.trim().to_string())
    }

    /// Read a sysfs attribute as a number.
    fn attr_u64(base: &Path, name: &str) -> Option<u64> {
        attr(base, name)?.parse().ok()
    }

    /// `/sys/class/block`, which covers whole disks and partitions alike.
    /// `/sys/block` lists only whole disks.
    fn class_block(name: &str) -> PathBuf {
        PathBuf::from("/sys/class/block").join(name)
    }

    /// The real `/sys`, answering the four questions the guard asks.
    struct Sysfs;

    impl Topology for Sysfs {
        fn slaves(&self, name: &str) -> Vec<String> {
            let mut found: Vec<String> = std::fs::read_dir(class_block(name).join("slaves"))
                .map(|entries| {
                    entries
                        .flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
            found.sort();
            found
        }

        fn is_partition(&self, name: &str) -> bool {
            class_block(name).join("partition").exists()
        }

        fn partition_parent(&self, name: &str) -> Option<String> {
            let base = class_block(name);
            if !base.join("partition").exists() {
                return None;
            }
            std::fs::canonicalize(&base).ok().and_then(|real| {
                real.parent()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
            })
        }

        fn exists(&self, name: &str) -> bool {
            class_block(name).exists()
        }
    }

    /// The kernel name for a `major:minor`, through `/sys/dev/block`.
    fn name_for_devnum(number: &str) -> Option<String> {
        std::fs::canonicalize(PathBuf::from("/sys/dev/block").join(number))
            .ok()
            .and_then(|real| real.file_name().map(|n| n.to_string_lossy().into_owned()))
    }

    /// Every physical disk the running system rests on.
    ///
    /// It covers more than `/`. A disk that `/boot`, `/boot/efi` or any other
    /// system mount comes from is a disk the machine needs. Writing it is as
    /// destructive as writing the root. It also covers more than mounts. An active
    /// **swap** device holds a disk as firmly, and appears in no mount table. The
    /// walk is transitive through every one of them.
    ///
    /// **An empty answer is a refusal, not permission.** If the root cannot be
    /// resolved at all, nothing can be shown to be safe. [`refused_disks`] then
    /// returns `None`, not an empty list.
    fn refused_disks() -> Option<Vec<String>> {
        let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
        let root = sysfs::root_device_number(&mountinfo)?;
        let root_name = name_for_devnum(&root)?;

        let reached = sysfs::physical_disks_under(&Sysfs, &root_name);
        // An incomplete walk is a refusal of everything, not a short list. The
        // topology could not be established, so nothing in it can be shown to be
        // safe -- and the one thing this must never do is hand back a partial
        // list that reads as permission for whatever is missing from it.
        if !reached.is_complete() || reached.disks.is_empty() {
            return None;
        }
        let mut refused = reached.disks;

        // Every other disk **the system itself** has mounted, for the same
        // reason: `/boot` and `/boot/efi` are as load-bearing as `/`, and on a
        // machine that keeps them on a second disk the walk from `/` never
        // reaches it.
        //
        // Somebody's media is not that, and is deliberately left out. A disk
        // mounted at `/media/<user>/<uuid>` is held by the kernel, so `O_EXCL`
        // refuses it at the open with a remedy attached; calling it the running
        // system would refuse it here instead, with no override and a sentence
        // about destroying the machine that is not true of an SD card. See
        // [`sysfs::is_system_mount`].
        let mounts = sysfs::parse_mountinfo(&mountinfo);
        let system: BTreeMap<String, String> = mounts
            .iter()
            .filter(|(_, point)| sysfs::is_system_mount(point))
            .map(|(number, point)| (number.clone(), point.clone()))
            .collect();
        for disk in sysfs::disks_with_mounts(&Sysfs, &system, name_for_devnum) {
            if !refused.contains(&disk) {
                refused.push(disk);
            }
        }

        // And every disk carrying the system's **swap**, which holds a device as
        // firmly as a mount does and appears in no mount table -- so a check that
        // consulted only mounts above would call an active swap device free. It is
        // load-bearing in the same way `/boot` is: overwrite it and the machine
        // comes down, and on a machine that keeps swap on a second physical disk
        // the walk from `/` never reaches it. `O_EXCL` refuses it either way; what
        // this decides is *which* refusal a person is shown, and the honest one
        // here is the pre-emptive one with no override rather than the generic
        // "something holds it".
        //
        // A swap **file** is not a swap device and is skipped: `parse_swaps` keeps
        // only the `/dev/` entries, because a file lives on a filesystem that is
        // already mounted and already accounted for above.
        //
        // An unreadable `/proc/swaps` is an empty one here rather than a refusal of
        // everything. Unlike the walk from `/`, this is an addition to a list that
        // was already established, and turning the whole listing into "every disk
        // carries the system" over a file that is absent on a machine with no swap
        // would be a guard that fires where there is nothing to guard.
        let swaps = std::fs::read_to_string("/proc/swaps").unwrap_or_default();
        for name in sysfs::parse_swaps(&swaps) {
            for disk in sysfs::physical_disks_under(&Sysfs, &name).disks {
                if !refused.contains(&disk) {
                    refused.push(disk);
                }
            }
        }

        refused.sort();
        Some(refused)
    }

    /// Every whole block device the kernel knows about.
    ///
    /// It needs **no privilege**, and opens nothing. This separates Linux from
    /// Windows, where capacity costs administrator rights, and therefore so does
    /// any plan.
    pub fn list() -> Result<Vec<BlockDevice>> {
        let entries = std::fs::read_dir("/sys/block")
            .map_err(|e| Error::Io(format!("cannot list /sys/block: {e}")))?;
        let mut names: Vec<String> = entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();

        let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
        let mounts = sysfs::parse_mountinfo(&mountinfo);
        // `None` means the running system's disks could not be determined. Every
        // device is then marked as carrying it, because a guard that cannot tell
        // must refuse rather than permit.
        let refused = refused_disks();

        let mut devices = Vec::new();
        for name in names {
            let base = PathBuf::from("/sys/block").join(&name);
            devices.push(BlockDevice {
                bytes: attr_u64(&base, "size").unwrap_or(0) * sysfs::SYSFS_SECTOR,
                logical_block: attr_u64(&base, "queue/logical_block_size").unwrap_or(512) as u32,
                physical_block: attr_u64(&base, "queue/physical_block_size").unwrap_or(512) as u32,
                removable: attr_u64(&base, "removable").is_some_and(|v| v != 0),
                read_only: attr_u64(&base, "ro").is_some_and(|v| v != 0),
                bus: bus_of(&base),
                model: model_of(&base),
                mounts: mounts_of(&base, &mounts),
                // Not "is this one of the system's disks" but "does this rest
                // on one", which catches every layer of the stack: the LVM
                // volume holding `/`, the LUKS container under it, the
                // partition under that, and the disk under that are four names
                // for four devices, and writing any of them destroys the
                // system. `None` -- the walk failed -- refuses everything.
                carries_running_system: match &refused {
                    Some(list) => sysfs::rests_on_any(&Sysfs, &name, list),
                    None => true,
                },
                node: format!("/dev/{name}"),
                name,
            });
        }
        Ok(devices)
    }

    /// Where this disk, or any of its partitions, is mounted.
    fn mounts_of(base: &Path, mounts: &BTreeMap<String, String>) -> Vec<String> {
        let mut found = Vec::new();
        let mut note = |dir: &Path| {
            if let Some(point) = attr(dir, "dev").and_then(|num| mounts.get(&num)) {
                found.push(point.clone());
            }
        };
        note(base);
        if let Ok(children) = std::fs::read_dir(base) {
            let mut names: Vec<String> = children
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|child| base.join(child).join("partition").exists())
                .collect();
            names.sort();
            for child in names {
                note(&base.join(child));
            }
        }
        found
    }

    /// The bus, from the device's resolved position in the sysfs tree.
    fn bus_of(base: &Path) -> Bus {
        match std::fs::canonicalize(base) {
            Ok(real) => sysfs::bus_from_path(&real.to_string_lossy()),
            Err(_) => Bus::Unknown,
        }
    }

    /// Vendor and model, where the device carries them.
    fn model_of(base: &Path) -> Option<String> {
        let device = base.join("device");
        let vendor = attr(&device, "vendor").unwrap_or_default();
        let model = attr(&device, "model")
            .or_else(|| attr(&device, "name"))
            .unwrap_or_default();
        let joined = format!("{vendor} {model}").trim().to_string();
        (!joined.is_empty()).then_some(joined)
    }

    /// List the disk at this node again, and refuse it unless it is the one that
    /// was described.
    ///
    /// A listing goes stale. A window draws rows and leaves them on screen while
    /// somebody reads them. A command lists and opens milliseconds apart. In either
    /// case, a card can be pulled and another inserted, and the kernel gives the new
    /// one the old one's name. `sdb` is as unstable across a replug as Windows'
    /// `PhysicalDriveN` is across a reboot.
    ///
    /// It also keeps the guards in [`open_for`] from depending on caller
    /// discipline. A [`BlockDevice`] is a plain struct with public fields. Without
    /// this step, a caller could pass [`open_for`] a description of a disk that
    /// does not carry the running system. It would then get a handle on one that
    /// does. The refusals are made against the description this returns, which
    /// comes from the kernel and not the caller.
    ///
    /// The comparison covers what the kernel can say about a device without
    /// opening it: capacity, geometry, bus, model and removability. Two identical
    /// cards swapped for each other would pass it. It exists to catch a plan
    /// whose capacity, sector size and mounts belong to a device that is no longer
    /// there.
    ///
    /// The exclusive open is what ties a plan to this device. It is held from
    /// before the plan is made until after the write, so the device cannot be
    /// swapped in that time. That guarantee starts at the open, and this check is
    /// the step before it.
    fn still_the_same_device(picked: &BlockDevice) -> Result<BlockDevice> {
        let listed = list()?;
        let Some(found) = listed.into_iter().find(|disk| disk.node == picked.node) else {
            return Err(Error::InvalidRequest(format!(
                "`{}` is not a block device on this machine any more. List the disks again and \
                 pick it again",
                picked.node
            )));
        };
        if found.bytes != picked.bytes
            || found.logical_block != picked.logical_block
            || found.physical_block != picked.physical_block
            || found.bus != picked.bus
            || found.model != picked.model
            || found.removable != picked.removable
        {
            return Err(Error::InvalidRequest(format!(
                "`{}` is not the device that was listed under that name. It now reports {} on \
                 {}, and the listing said {} on {}. List the disks again and pick it again",
                picked.node,
                found.bytes,
                found.bus.name(),
                picked.bytes,
                picked.bus.name(),
            )));
        }
        Ok(found)
    }

    /// Open a block device for exclusive, uncached use, for a named `access`.
    ///
    /// It makes four refusals, cheapest first:
    ///
    /// 1. **A device that is not the one the caller described.** `device` is a
    ///    description the caller holds, and every field of it is `pub`. The device
    ///    is therefore listed again and compared on capacity, geometry, bus, model
    ///    and removability. The later refusals then judge what the kernel reports
    ///    at the open, not data the caller supplied. Two identical cards swapped
    ///    for each other pass this check. It catches a plan whose capacity, sector
    ///    size and mounts belong to a device that is no longer there.
    /// 2. **The disks the running system rests on**, transitively and with no
    ///    override. This refusal applies to a write only, because a read carries
    ///    no such danger. Unlike the wrong-loader gate, it cannot wait for an
    ///    answer to judge. Writing the booted disk succeeds, reads back correctly,
    ///    and brings the machine down minutes later with no error raised anywhere.
    ///    There is nothing to catch, so the guard is pre-emptive.
    /// 3. **A device the kernel holds**, via `O_EXCL`. The kernel makes this
    ///    refusal, and it is broader than anything this code can ask.
    /// 4. **A device that cannot be opened uncached**, via `O_DIRECT`. A read-back
    ///    served from the page cache proves nothing about the device, and this
    ///    backend does not make a write it cannot check.
    ///
    /// Each failure returns its own error. Windows reports missing elevation, a
    /// held device and the booted disk all as `ERROR_ACCESS_DENIED`, which tells a
    /// person nothing. On Linux the kernel distinguishes them, and so does this
    /// function.
    pub fn open_for(device: &BlockDevice, access: Access) -> Result<BlockAgent> {
        open_fresh(&still_the_same_device(device)?, access)
    }

    /// Open a disk for the widest access it allows: read and write where nothing
    /// refuses a write to it, read alone where something does.
    ///
    /// A caller that opens a device once and then offers several verbs on it uses
    /// this, and both front-ends do. Like [`open_for`], it lists the device again
    /// and refuses one that is no longer the device described.
    ///
    /// It differs from `open_for(device, Access::widest_for(device))` in where the
    /// access is decided. This function decides from the description read at the
    /// moment of the open. A card whose lock switch was flipped since it was listed
    /// therefore opens for reading, rather than being refused for a write nobody
    /// asked for.
    pub fn open(device: &BlockDevice) -> Result<BlockAgent> {
        let fresh = still_the_same_device(device)?;
        let access = Access::widest_for(&fresh);
        open_fresh(&fresh, access)
    }

    /// The open itself, over a description read at the moment of the open.
    fn open_fresh(device: &BlockDevice, access: Access) -> Result<BlockAgent> {
        // Only a write asks these. A read has no counterpart danger -- a card with
        // its lock switch on is the safest thing there is to image -- and asking
        // them here would refuse every read of the one class of media this backend
        // is most obviously for, with a sentence about a write nobody requested.
        if access.writes()
            && let Some(refusal) = super::write_refusal(device)
        {
            return Err(Error::InvalidRequest(refusal));
        }

        let block = device.logical_block.max(1) as usize;
        let file = OpenOptions::new()
            .read(true)
            // Read-only media cannot be opened `O_RDWR` at all, so the write
            // permission is asked for only when a write is what is intended.
            .write(access.writes())
            .custom_flags(O_EXCL | O_DIRECT)
            .open(&device.node)
            .map_err(|e| open_error(device, e))?;

        Ok(BlockAgent {
            file,
            bounce: AlignedBuf::new(DEFAULT_WINDOW, block),
            device: device.clone(),
            desynchronized: false,
            access,
        })
    }

    /// A [`BlockAgent`] over an ordinary file, for tests with no block device and
    /// no privilege.
    ///
    /// The read and write path is otherwise reached through [`open`], which needs
    /// root. A bench can have root, and a test suite does not. A check only a
    /// privileged bench can make is one CI never makes. The verbs' behavior over
    /// this backend (which gates apply to it, and what a plan for a disk promises)
    /// needs pinning on every commit, not on every bench run.
    ///
    /// The agent it returns is the real one, with the same range checks, bounce
    /// buffer, `read_at` and `write_at`. The verbs and their gates run over it
    /// exactly as they run over a disk.
    ///
    /// **It deliberately omits what makes a device a device**: `O_EXCL`,
    /// `O_DIRECT` and the kernel's refusals. None of those three is what a test of
    /// the verb path asks about, and the bench measures them against real media, as
    /// `bench/README.md` describes. The file is opened plainly, because a regular
    /// file on many filesystems does not serve uncached I/O at all. Its geometry
    /// comes from the caller, not from sysfs.
    ///
    /// The device it describes is synthetic and says so. It carries the file's
    /// path as its node, the caller's geometry, and no mounts. `access` is what
    /// the agent was opened for, so tests pin the read-only behavior too: an agent
    /// that refuses a write and says why.
    #[cfg(any(test, feature = "testing"))]
    pub fn agent_over_file(
        path: &Path,
        bytes: u64,
        logical_block: u32,
        access: Access,
    ) -> Result<BlockAgent> {
        let file = OpenOptions::new()
            .read(true)
            .write(access.writes())
            .open(path)
            .map_err(|e| Error::Io(format!("cannot open {} as a device: {e}", path.display())))?;
        let device = BlockDevice {
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".to_string()),
            node: path.display().to_string(),
            bytes,
            logical_block,
            physical_block: logical_block,
            removable: false,
            read_only: false,
            bus: Bus::Unknown,
            model: Some("a file standing in for a device".to_string()),
            mounts: Vec::new(),
            carries_running_system: false,
        };
        Ok(BlockAgent {
            file,
            bounce: AlignedBuf::new(DEFAULT_WINDOW, logical_block.max(1) as usize),
            device,
            desynchronized: false,
            access,
        })
    }

    /// Say which of the three things went wrong, in the kernel's own terms.
    fn open_error(device: &BlockDevice, e: std::io::Error) -> Error {
        let name = &device.name;
        match e.raw_os_error() {
            // EBUSY: `O_EXCL` refused. Something else holds it, and the kernel
            // knows about holders this code cannot enumerate.
            Some(16) => Error::InvalidRequest(format!(
                "`{name}` is in use: the kernel refused an exclusive open. Something holds it, \
                 such as a mounted filesystem, a volume stacked on it, or swap. Unmount or \
                 deactivate it first{}",
                if device.is_mounted() {
                    format!(" (mounted at {})", device.mounts.join(", "))
                } else {
                    ". Nothing is mounted on it, so look for a stacked device or swap".to_string()
                }
            )),
            // EACCES: no privilege. Listing reads sysfs and needs none, but
            // opening the device needs privilege, and planning a write opens it.
            Some(13) => Error::Io(format!(
                "`{name}` cannot be opened without root or membership of the `disk` group. \
                 Listing it needs no privilege, but planning or writing opens it"
            )),
            // EINVAL from an O_DIRECT open: the device or its filesystem will not
            // serve uncached I/O.
            Some(22) => Error::InvalidRequest(format!(
                "`{name}` cannot be opened for uncached access (O_DIRECT). Without it, a \
                 read-back can come from the host's cache rather than the device, and a verify \
                 would prove nothing. This backend does not write to such a device"
            )),
            _ => Error::Io(format!("cannot open `{name}`: {e}")),
        }
    }
}

#[cfg(all(target_os = "linux", not(target_arch = "wasm32")))]
pub use linux::{list, open, open_for};

#[cfg(all(
    target_os = "linux",
    not(target_arch = "wasm32"),
    any(test, feature = "testing")
))]
pub use linux::agent_over_file;

#[cfg(test)]
mod tests {
    use super::*;

    /// A 64 MiB device of 512-byte sectors: 131072 of them.
    const SIZE: u64 = 64 << 20;
    const BLOCK: usize = 512;

    #[test]
    fn a_request_inside_the_device_is_allowed() {
        assert!(check_range("d", 0, BLOCK, BLOCK, SIZE).is_ok());
        assert!(check_range("d", 131_071, BLOCK, BLOCK, SIZE).is_ok());
        // Exactly to the end, which is the boundary an off-by-one lands on.
        assert!(check_range("d", 131_070, BLOCK * 2, BLOCK, SIZE).is_ok());
    }

    #[test]
    fn a_request_ending_one_sector_past_the_end_is_refused() {
        let error = check_range("d", 131_071, BLOCK * 2, BLOCK, SIZE)
            .expect_err("it ends at 131072, one past the last sector");
        assert!(format!("{error}").contains("run past the end"), "{error}");
    }

    #[test]
    fn a_request_starting_past_the_end_is_refused() {
        assert!(check_range("d", 999_999, BLOCK, BLOCK, SIZE).is_err());
    }

    /// A length that is not a whole number of sectors fails at the syscall with no
    /// useful message. It is refused here, where the reason can be given.
    #[test]
    fn a_partial_sector_is_refused_with_the_block_size_named() {
        let error =
            check_range("d", 0, 100, BLOCK, SIZE).expect_err("100 is not a multiple of 512");
        let text = format!("{error}");
        assert!(text.contains("512"), "{text}");
        assert!(text.contains("100"), "{text}");
    }

    /// The end-sector sum must not wrap. A wrapped sum would compare small and
    /// turn a refusal into a permission. On this backend, that means writing
    /// somewhere nobody asked for.
    #[test]
    fn a_range_that_would_overflow_is_refused_rather_than_wrapping() {
        let error = check_range("d", u64::MAX, BLOCK, BLOCK, SIZE)
            .expect_err("the end sector overflows a u64");
        assert!(format!("{error}").contains("overflow"), "{error}");
    }

    /// A 4Kn disk addresses in 4096-byte blocks, and its sector count is an
    /// eighth of what the same capacity gives at 512. Reading sysfs's 512-byte
    /// `size` as logical blocks would overstate such a device eight times over.
    #[test]
    fn a_4kn_device_is_measured_in_its_own_blocks() {
        let block = 4096;
        let sectors = SIZE / block as u64;
        assert_eq!(sectors, 16_384);
        assert!(check_range("d", sectors - 1, block, block, SIZE).is_ok());
        assert!(check_range("d", sectors, block, block, SIZE).is_err());
        // A 512-byte request on a 4Kn device is not a whole sector.
        assert!(check_range("d", 0, 512, block, SIZE).is_err());
    }

    #[test]
    fn a_zero_block_size_is_refused_rather_than_dividing_by_it() {
        assert!(check_range("d", 0, 512, 0, SIZE).is_err());
    }

    /// The pre-emptive refusal, which has no override.
    #[test]
    fn a_device_carrying_the_running_system_is_refused() {
        let mut device = a_device();
        device.carries_running_system = true;
        let why = write_refusal(&device).expect("it refuses");
        assert!(why.contains("running system"), "{why}");
        assert!(why.contains("no override"), "{why}");
    }

    #[test]
    fn a_kernel_read_only_device_is_refused() {
        let mut device = a_device();
        device.read_only = true;
        assert!(write_refusal(&device).is_some());
    }

    /// A mount is reported, not refused here. The kernel settles it at the open
    /// with `O_EXCL`, and the kernel knows about holders this code cannot
    /// enumerate.
    #[test]
    fn a_mounted_device_is_not_refused_here_because_the_kernel_settles_it() {
        let mut device = a_device();
        device.mounts = vec!["/media/scratch".to_string()];
        assert!(device.is_mounted());
        assert!(
            write_refusal(&device).is_none(),
            "the mount is reported for a plan to show; O_EXCL is what refuses it"
        );
    }

    /// `O_DIRECT` requires the buffer to be aligned to the logical block size,
    /// and a misaligned one fails the transfer with `EINVAL`. The alignment is
    /// computed from the allocation's own address, so this checks the arithmetic
    /// against several sizes rather than trusting one lucky allocation.
    #[test]
    fn the_bounce_buffer_is_aligned_to_the_block_size() {
        for align in [512usize, 4096] {
            for len in [512usize, 4096, 1 << 20] {
                let buf = AlignedBuf::new(len, align);
                let addr = buf.as_slice(len).as_ptr() as usize;
                assert_eq!(addr % align, 0, "len={len} align={align} addr={addr:#x}");
                assert_eq!(buf.as_slice(len).len(), len);
            }
        }
    }

    /// Growing reallocates, which moves the buffer, so the aligned offset must be
    /// recomputed. A cached offset would survive the move and point at the wrong
    /// byte. `O_DIRECT` would reject it, and a buffered path would silently
    /// mis-address.
    #[test]
    fn the_bounce_buffer_stays_aligned_after_it_grows() {
        let align = 4096;
        let mut buf = AlignedBuf::new(512, align);
        for len in [4096usize, 8192, 1 << 20, 1 << 21] {
            buf.ensure(len);
            let addr = buf.as_mut_slice(len).as_ptr() as usize;
            assert_eq!(addr % align, 0, "after growing to {len}: {addr:#x}");
            assert_eq!(buf.as_slice(len).len(), len);
        }
    }

    fn a_device() -> BlockDevice {
        BlockDevice {
            name: "loop9".to_string(),
            node: "/dev/loop9".to_string(),
            bytes: SIZE,
            logical_block: 512,
            physical_block: 512,
            removable: false,
            read_only: false,
            bus: Bus::Virtual,
            model: None,
            mounts: Vec::new(),
            carries_running_system: false,
        }
    }
}
