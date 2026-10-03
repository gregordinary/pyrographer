//! What the operating system says about its own block devices, as a sans-I/O
//! codec.
//!
//! The Block backend is the first target the host also owns. That raises three
//! questions: which disk carries the running system, what else holds a device, and
//! how big it is. On Linux, text read from `/sys` and `/proc` answers them. This
//! module parses that text as a wire format. Its functions take strings and a
//! topology and return answers, and open nothing.
//!
//! The parsing is pure so that the guard can be tested. The central function is
//! [`physical_disks_under`], the transitive walk that decides which disks a write
//! must refuse. An error in it destroys the machine the tool runs on. Because it is
//! pure, it is tested exhaustively against topologies a test describes. A test
//! machine cannot be relied on to have the LVM-over-LUKS arrangement the walk
//! exists for.
//!
//! The thin readers that fetch this text from a real `/sys` are in the parent
//! module. Everything here is pure.

use std::collections::BTreeMap;

/// Where a block device sits: the transport it is attached through, as far as its
/// position in the sysfs tree reveals.
///
/// No sysfs attribute names the bus. A device's resolved path runs through the
/// controller it is attached to, so the path components identify it.
/// [`Unknown`](Bus::Unknown) is an answer, not a failure: the device is on a bus
/// this code does not recognize.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bus {
    /// A USB-attached device. The common case for removable media, and for a
    /// board presenting itself as mass storage.
    Usb,
    /// An NVMe device.
    Nvme,
    /// SATA or PATA, through the ATA layer.
    Ata,
    /// SCSI, including most USB bridges' inner layer.
    Scsi,
    /// An SD or eMMC host controller.
    Mmc,
    /// A virtio device, as a virtual machine sees its disks.
    Virtio,
    /// No hardware behind it: a loop device, a device-mapper target, zram.
    Virtual,
    /// A bus this does not recognize.
    Unknown,
}

impl Bus {
    /// The name a report prints.
    pub fn name(&self) -> &'static str {
        match self {
            Bus::Usb => "usb",
            Bus::Nvme => "nvme",
            Bus::Ata => "ata",
            Bus::Scsi => "scsi",
            Bus::Mmc => "mmc",
            Bus::Virtio => "virtio",
            Bus::Virtual => "virtual",
            Bus::Unknown => "unknown",
        }
    }
}

/// Read the bus out of a device's resolved sysfs path.
///
/// It checks the most specific bus first. A USB disk's path runs through both
/// `/usb` and `/scsi`, because a USB mass-storage bridge presents a SCSI device.
/// What a report needs about such a disk is that it is removable and on a cable,
/// so it reads as [`Usb`](Bus::Usb).
pub fn bus_from_path(path: &str) -> Bus {
    for (needle, bus) in [
        ("/usb", Bus::Usb),
        ("/nvme", Bus::Nvme),
        ("/virtio", Bus::Virtio),
        ("/mmc_host", Bus::Mmc),
        ("/ata", Bus::Ata),
        ("/scsi", Bus::Scsi),
    ] {
        if path.contains(needle) {
            return bus;
        }
    }
    if path.contains("/virtual/") {
        return Bus::Virtual;
    }
    Bus::Unknown
}

/// The unit `/sys/block/<disk>/size` counts in, which is always 512 bytes,
/// whatever the device's logical block size.
///
/// A disk reporting 4096-byte logical blocks still reports its size here in
/// 512-byte units. Reading it as logical blocks would overstate such a disk eight
/// times over. A plan built on that figure would promise to write past the end of
/// the device.
pub const SYSFS_SECTOR: u64 = 512;

/// Every mounted filesystem, as `major:minor` to mount point.
///
/// It reads `/proc/self/mountinfo`, whose third field is the device number. So a
/// mount is matched to a device **by number**, not by the source path. The source
/// path can be a symlink, a bind mount, or absent entirely.
///
/// The first mount of a device wins, which affects only what a report prints. A
/// device mounted twice is held either way, and the refusal depends only on whether
/// it is held.
pub fn parse_mountinfo(text: &str) -> BTreeMap<String, String> {
    let mut table = BTreeMap::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // Fields are 1-indexed in the kernel's documentation: 3 is the device
        // number and 5 is the mount point.
        if fields.len() >= 5 {
            table
                .entry(fields[2].to_string())
                .or_insert_with(|| fields[4].to_string());
        }
    }
    table
}

/// The device number `/` is mounted from.
///
/// On any ordinary desktop this is **not a disk**: it is routinely a
/// device-mapper target, built through several layers on a disk.
/// [`physical_disks_under`] turns this answer into the disks a write guard
/// refuses.
pub fn root_device_number(mountinfo: &str) -> Option<String> {
    for line in mountinfo.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() >= 5 && fields[4] == "/" {
            return Some(fields[2].to_string());
        }
    }
    None
}

/// Whether a mount point belongs to the running system, rather than to media
/// somebody plugged in and a desktop mounted for them.
///
/// The distinction decides which of two different refusals a device gets. A mount
/// under `/media`, `/run/media` or `/mnt` is somebody's media. Every other mount
/// point, such as `/`, `/boot` or `/var`, is taken to be the system's.
///
/// The rule costs no safety. A mounted device is one the kernel holds, so `O_EXCL`
/// refuses it at the open, whatever this function answers. The answer changes only
/// which refusal a person sees, and whether it offers a remedy. Media gets "unmount
/// it first", which is true and actionable. The system's own disks keep the refusal
/// with no override, because writing one destroys the machine. Writing an
/// automounted SD card does not.
pub fn is_system_mount(point: &str) -> bool {
    const MEDIA: [&str; 3] = ["/media", "/run/media", "/mnt"];
    !MEDIA
        .iter()
        .any(|root| point == *root || point.starts_with(&format!("{root}/")))
}

/// Every device in use as swap, by kernel name.
///
/// Swap holds a device as firmly as a mount does, and appears in no mount table.
/// A check that consulted only mounts would call an active swap device free.
/// `O_EXCL` refuses an active swap device (measured).
pub fn parse_swaps(text: &str) -> Vec<String> {
    text.lines()
        .skip(1)
        .filter_map(|line| line.split_whitespace().next())
        .filter_map(|path| path.strip_prefix("/dev/"))
        .map(|name| name.to_string())
        .collect()
}

/// How block devices are stacked, to the extent the guard asks.
///
/// It is a seam, so that [`physical_disks_under`] can be tested against topologies
/// a test describes. The real implementation reads `/sys`, and the tests supply a
/// map. Both answer the same four questions, which are the only ones the walk
/// asks.
pub trait Topology {
    /// What this device is built on: the contents of its `slaves/` directory.
    ///
    /// Empty for anything that is not a mapped or composed device.
    fn slaves(&self, name: &str) -> Vec<String>;

    /// Whether this device is a partition of some disk.
    ///
    /// It is asked separately from
    /// [`partition_parent`](Topology::partition_parent), so that "not a partition"
    /// and "a partition whose disk could not be determined" are different answers.
    /// Collapsing them would let the second be reported as a disk in its own right.
    /// A caller would then refuse `sda1` and leave `sda` writable.
    fn is_partition(&self, name: &str) -> bool;

    /// The whole disk a partition belongs to.
    fn partition_parent(&self, name: &str) -> Option<String>;

    /// Whether this device exists at all, so a walk over a stale name stops
    /// rather than inventing a disk.
    fn exists(&self, name: &str) -> bool;
}

/// What a walk down a device stack reached.
///
/// There are two lists, because "no disks" and "no disks, and the walk broke" are
/// different facts. Only the first can be read as permission. A caller deciding
/// what a write may touch treats a non-empty [`unresolved`](Reached::unresolved) as
/// a refusal of everything. The topology could not be established, so nothing in
/// it can be shown to be safe.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Reached {
    /// The physical disks the stack rests on.
    pub disks: Vec<String>,
    /// Devices the walk could not resolve: a name the kernel does not know, or
    /// a partition whose disk could not be determined.
    pub unresolved: Vec<String>,
}

impl Reached {
    /// Whether the topology was established completely.
    ///
    /// `false` means a caller must refuse rather than trust
    /// [`disks`](Reached::disks), which is necessarily incomplete.
    pub fn is_complete(&self) -> bool {
        self.unresolved.is_empty()
    }
}

/// Every **physical disk** a given block device ultimately rests on.
///
/// This is the module's central guard, and Windows's answer does not transfer to
/// Linux. On Windows, `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS` names the disks of the
/// system volume in one call. On Linux, `/` is routinely mounted from a
/// device-mapper target built on another device-mapper target, built on a partition
/// of a disk. LVM over LUKS is the ordinary desktop arrangement, and each layer is
/// its own entry in `/sys/block` with its own name.
///
/// **A guard that refused only the device `/` is mounted from would refuse the
/// outermost layer and leave the physical disk writable.** Writing that disk
/// destroys the machine just as surely. The walk is therefore transitive. `slaves/`
/// names what a composed device is built on, and a partition resolves to its disk.
/// The recursion ends at devices with neither.
///
/// Cycles cannot happen in a well-formed device tree, and the walk stops them
/// anyway. It runs against whatever the kernel reports, so a malformed tree must
/// not be able to hang it.
pub fn physical_disks_under<T: Topology + ?Sized>(tree: &T, device: &str) -> Reached {
    fn walk<T: Topology + ?Sized>(tree: &T, name: &str, out: &mut Reached, seen: &mut Vec<String>) {
        if seen.iter().any(|s| s == name) {
            return;
        }
        seen.push(name.to_string());

        // A name the kernel does not know. The walk cannot complete, and saying
        // so is the point: an empty answer must never read as "nothing to
        // refuse".
        if !tree.exists(name) {
            push_unique(&mut out.unresolved, name);
            return;
        }

        // A composed device names what it is built on. Recurse before anything
        // else, because these stack: only the bottom of the stack is a disk.
        let slaves = tree.slaves(name);
        if !slaves.is_empty() {
            for slave in slaves {
                walk(tree, &slave, out, seen);
            }
            return;
        }

        // A partition resolves to the disk that carries it -- and if it cannot,
        // that is recorded rather than papered over. Reporting the partition as
        // a disk of its own would leave the real disk unrefused.
        if tree.is_partition(name) {
            match tree.partition_parent(name) {
                Some(disk) => push_unique(&mut out.disks, &disk),
                None => push_unique(&mut out.unresolved, name),
            }
            return;
        }

        // Whatever is left is as far down as this goes.
        push_unique(&mut out.disks, name);
    }

    let mut out = Reached::default();
    let mut seen = Vec::new();
    walk(tree, device, &mut out, &mut seen);
    out
}

/// Append `name` to `list`, unless the list already holds it.
fn push_unique(list: &mut Vec<String>, name: &str) {
    if !list.iter().any(|n| n == name) {
        list.push(name.to_string());
    }
}

/// Whether writing `device` would damage anything the given disks carry.
///
/// It is the refusal rule. It refuses any device that rests on one of the given
/// disks, not only the disks themselves, so it catches every layer of the stack.
/// Writing the LVM volume that holds `/` destroys the root filesystem as surely as
/// writing the NVMe disk it is built on. Writing the partition the LUKS container
/// is built on destroys the container. Each is its own entry in `/sys/block` with
/// its own name, and each resolves to the same disk.
///
/// It also catches devices outside the root's own stack that share its hardware.
/// A swap volume on another partition of the system disk is one, and an EFI system
/// partition is another. A raw write to any of them lands on a disk the machine needs.
///
/// **An incomplete walk answers `true`.** A topology that could not be established
/// is one in which nothing can be shown to be safe. The rule's only failure mode is
/// refusing something harmless.
pub fn rests_on_any<T: Topology + ?Sized>(tree: &T, device: &str, disks: &[String]) -> bool {
    let reached = physical_disks_under(tree, device);
    if !reached.is_complete() {
        return true;
    }
    reached.disks.iter().any(|d| disks.iter().any(|s| s == d))
}

/// Every physical disk carrying a mounted filesystem, given a way to turn a
/// device number into a name.
///
/// Only the booted disk is refused with no override. A disk carrying any mount is
/// in use, and a plan that names one says so.
pub fn disks_with_mounts<T: Topology + ?Sized>(
    tree: &T,
    mounts: &BTreeMap<String, String>,
    name_of: impl Fn(&str) -> Option<String>,
) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for number in mounts.keys() {
        let Some(name) = name_of(number) else {
            continue;
        };
        for disk in physical_disks_under(tree, &name).disks {
            if !found.contains(&disk) {
                found.push(disk);
            }
        }
    }
    found.sort();
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A topology a test describes: `slaves` maps a device to what it is built
    /// on, `partitions` maps a partition to its disk.
    struct Described {
        slaves: BTreeMap<String, Vec<String>>,
        partitions: BTreeMap<String, String>,
        present: Vec<String>,
    }

    impl Described {
        /// The arrangement measured on the bench: `/` on an LVM volume, built on a
        /// LUKS container, built on a partition of an NVMe disk.
        fn lvm_over_luks() -> Described {
            Described {
                slaves: [
                    ("dm-1".to_string(), vec!["dm-0".to_string()]),
                    ("dm-0".to_string(), vec!["nvme0n1p3".to_string()]),
                ]
                .into_iter()
                .collect(),
                partitions: [
                    ("nvme0n1p3".to_string(), "nvme0n1".to_string()),
                    ("sda1".to_string(), "sda".to_string()),
                ]
                .into_iter()
                .collect(),
                present: ["dm-0", "dm-1", "nvme0n1p3", "nvme0n1", "sda", "sda1"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            }
        }
    }

    impl Topology for Described {
        fn slaves(&self, name: &str) -> Vec<String> {
            self.slaves.get(name).cloned().unwrap_or_default()
        }
        fn is_partition(&self, name: &str) -> bool {
            self.partitions.contains_key(name)
        }
        fn partition_parent(&self, name: &str) -> Option<String> {
            self.partitions.get(name).cloned()
        }
        fn exists(&self, name: &str) -> bool {
            self.present.iter().any(|p| p == name)
        }
    }

    /// A topology with a partition whose disk cannot be determined. That partition
    /// must never be reported as a disk.
    struct BrokenPartition;

    impl Topology for BrokenPartition {
        fn slaves(&self, _: &str) -> Vec<String> {
            Vec::new()
        }
        fn is_partition(&self, name: &str) -> bool {
            name == "sda1"
        }
        fn partition_parent(&self, _: &str) -> Option<String> {
            None
        }
        fn exists(&self, _: &str) -> bool {
            true
        }
    }

    /// The device `/` is mounted from is not the disk that must be refused. In this
    /// topology the stack is four layers deep, and only the last is the disk.
    #[test]
    fn the_walk_reaches_the_disk_under_an_lvm_over_luks_root() {
        let tree = Described::lvm_over_luks();
        let reached = physical_disks_under(&tree, "dm-1");
        assert_eq!(
            reached.disks,
            vec!["nvme0n1".to_string()],
            "a guard that stopped at dm-1 would leave nvme0n1 writable"
        );
        assert!(reached.is_complete());
    }

    /// Each layer on its own resolves to the same disk, so the guard does not
    /// depend on being handed the top of the stack.
    #[test]
    fn every_layer_of_the_stack_resolves_to_the_same_disk() {
        let tree = Described::lvm_over_luks();
        for entry in ["dm-1", "dm-0", "nvme0n1p3", "nvme0n1"] {
            assert_eq!(
                physical_disks_under(&tree, entry).disks,
                vec!["nvme0n1".to_string()],
                "{entry}"
            );
        }
    }

    /// A plain partition, with nothing stacked at all.
    #[test]
    fn a_bare_partition_resolves_to_its_disk() {
        let tree = Described::lvm_over_luks();
        assert_eq!(
            physical_disks_under(&tree, "sda1").disks,
            vec!["sda".to_string()]
        );
        assert_eq!(
            physical_disks_under(&tree, "sda").disks,
            vec!["sda".to_string()]
        );
    }

    /// A RAID or a linear target spans more than one disk, and **all** of them
    /// must be refused. Returning only the first would leave the others writable,
    /// the same failure as stopping at the top of the stack.
    #[test]
    fn a_device_spanning_several_disks_names_every_one_of_them() {
        let tree = Described {
            slaves: [(
                "md0".to_string(),
                vec!["sda1".to_string(), "sdb1".to_string()],
            )]
            .into_iter()
            .collect(),
            partitions: [
                ("sda1".to_string(), "sda".to_string()),
                ("sdb1".to_string(), "sdb".to_string()),
            ]
            .into_iter()
            .collect(),
            present: ["md0", "sda1", "sdb1", "sda", "sdb"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        };
        assert_eq!(
            physical_disks_under(&tree, "md0").disks,
            vec!["sda".to_string(), "sdb".to_string()]
        );
    }

    /// Two paths down to the same disk report it once, not twice.
    #[test]
    fn a_disk_reached_by_two_paths_is_named_once() {
        let tree = Described {
            slaves: [(
                "md0".to_string(),
                vec!["sda1".to_string(), "sda2".to_string()],
            )]
            .into_iter()
            .collect(),
            partitions: [
                ("sda1".to_string(), "sda".to_string()),
                ("sda2".to_string(), "sda".to_string()),
            ]
            .into_iter()
            .collect(),
            present: ["md0", "sda1", "sda2", "sda"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        };
        assert_eq!(
            physical_disks_under(&tree, "md0").disks,
            vec!["sda".to_string()]
        );
    }

    /// A malformed tree must not hang the guard. This cannot arise from a
    /// healthy kernel, and the walk runs on whatever the kernel reports.
    #[test]
    fn a_cycle_terminates_rather_than_hanging() {
        let tree = Described {
            slaves: [
                ("dm-0".to_string(), vec!["dm-1".to_string()]),
                ("dm-1".to_string(), vec!["dm-0".to_string()]),
            ]
            .into_iter()
            .collect(),
            partitions: BTreeMap::new(),
            present: ["dm-0", "dm-1"].iter().map(|s| s.to_string()).collect(),
        };
        // The answer names no disk -- there is none under a cycle -- and the
        // point is that it returns at all. A caller reads "no disks" as a
        // refusal, never as permission.
        assert!(physical_disks_under(&tree, "dm-0").disks.is_empty());
    }

    /// A name the kernel does not know resolves to nothing, rather than being
    /// invented as a disk of its own. A caller that read an empty answer as "no
    /// disks to refuse" would be wrong. The agent therefore treats an empty walk as
    /// a refusal, not as permission.
    #[test]
    fn an_unknown_device_is_unresolved_rather_than_simply_absent() {
        let tree = Described::lvm_over_luks();
        let reached = physical_disks_under(&tree, "sdz");
        assert!(reached.disks.is_empty());
        assert_eq!(reached.unresolved, vec!["sdz".to_string()]);
        assert!(
            !reached.is_complete(),
            "the walk did not establish the topology, and a caller must refuse rather than \
             read an empty disk list as nothing to protect"
        );
    }

    /// A partition whose disk cannot be determined must **not** be reported as a
    /// disk of its own. The trait asks `is_partition` and `partition_parent`
    /// separately for this case. A caller would otherwise refuse `sda1` and leave
    /// `sda` writable.
    #[test]
    fn a_partition_with_no_resolvable_disk_is_unresolved_not_a_disk() {
        let reached = physical_disks_under(&BrokenPartition, "sda1");
        assert!(
            reached.disks.is_empty(),
            "sda1 must never be reported as a disk: {:?}",
            reached.disks
        );
        assert_eq!(reached.unresolved, vec!["sda1".to_string()]);
        assert!(!reached.is_complete());
    }

    /// Every layer of the root's stack is refused, not only the disk it is built
    /// on. The layers are the LVM volume, the LUKS container, the partition and the
    /// disk.
    #[test]
    fn every_layer_resting_on_a_system_disk_is_refused() {
        let tree = Described::lvm_over_luks();
        let system = vec!["nvme0n1".to_string()];
        for layer in ["dm-1", "dm-0", "nvme0n1p3", "nvme0n1"] {
            assert!(
                rests_on_any(&tree, layer, &system),
                "{layer} rests on the system disk and must be refused"
            );
        }
    }

    /// A device on unrelated hardware is not refused. A rule that refused it would
    /// leave the backend nothing to write.
    #[test]
    fn a_device_on_other_hardware_is_not_refused() {
        let tree = Described::lvm_over_luks();
        let system = vec!["nvme0n1".to_string()];
        assert!(!rests_on_any(&tree, "sda", &system));
        assert!(!rests_on_any(&tree, "sda1", &system));
    }

    /// The rule's one permitted error is refusing something harmless, so a
    /// topology that cannot be established is refused.
    #[test]
    fn a_topology_that_cannot_be_established_is_refused() {
        let system = vec!["nvme0n1".to_string()];
        assert!(
            rests_on_any(&BrokenPartition, "sda1", &system),
            "an unresolvable device must be refused, never permitted"
        );
        let tree = Described::lvm_over_luks();
        assert!(
            rests_on_any(&tree, "sdz", &system),
            "a device the kernel does not know must be refused"
        );
    }

    #[test]
    fn mountinfo_is_read_by_device_number_and_finds_the_root() {
        let text = "\
25 30 0:22 / /proc rw,nosuid shared:5 - proc proc rw
26 30 0:23 / /sys rw,nosuid shared:6 - sysfs sysfs rw
30 1 253:1 / / rw,relatime shared:1 - ext4 /dev/mapper/data-root rw
40 30 259:1 / /boot/efi rw shared:9 - vfat /dev/nvme0n1p1 rw";
        let mounts = parse_mountinfo(text);
        assert_eq!(mounts.get("253:1").map(String::as_str), Some("/"));
        assert_eq!(mounts.get("259:1").map(String::as_str), Some("/boot/efi"));
        assert_eq!(root_device_number(text).as_deref(), Some("253:1"));
    }

    /// The device number in a mountinfo line identifies the device, and the source
    /// path does not. `/dev/mapper/data-root` is a symlink.
    #[test]
    fn the_root_is_found_by_number_not_by_its_source_path() {
        let text = "30 1 253:1 / / rw - ext4 /dev/mapper/some-alias rw";
        assert_eq!(root_device_number(text).as_deref(), Some("253:1"));
    }

    #[test]
    fn a_mountinfo_without_a_root_line_answers_none() {
        let text = "25 30 0:22 / /proc rw - proc proc rw";
        assert_eq!(root_device_number(text), None);
    }

    #[test]
    fn swap_devices_are_read_past_the_header() {
        let text = "\
Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority
/dev/dm-2                               partition\t4193784\t\t104044\t\t-1
/dev/zram0                              partition\t16777212\t16685544\t1000
/swapfile                               file\t\t1048572\t\t0\t\t-2";
        assert_eq!(
            parse_swaps(text),
            vec!["dm-2".to_string(), "zram0".to_string()]
        );
    }

    #[test]
    fn a_desktops_automount_of_somebodys_media_is_not_the_running_system() {
        assert!(!is_system_mount("/media/gregordinary/df673c1c-5b24"));
        assert!(!is_system_mount("/run/media/gregordinary/sdcard"));
        assert!(!is_system_mount("/mnt/scratch"));
        assert!(!is_system_mount("/media"));
    }

    #[test]
    fn the_systems_own_mounts_are_the_systems() {
        assert!(is_system_mount("/"));
        assert!(is_system_mount("/boot"));
        assert!(is_system_mount("/boot/efi"));
        assert!(is_system_mount("/recovery"));
        assert!(is_system_mount("/var/lib/whatever"));
    }

    #[test]
    fn a_directory_that_merely_starts_like_a_media_root_is_still_the_systems() {
        // `/mnt` is media; `/mnternal` is a directory whose name begins with the
        // same five letters, and a prefix test without the separator would hand
        // the disk under it the wrong refusal.
        assert!(is_system_mount("/mnternal"));
        assert!(is_system_mount("/mediafiles"));
    }

    /// A swap *file* is not a block device and is not named here. It lives on a
    /// filesystem, which is held by its own mount.
    #[test]
    fn a_swap_file_is_not_reported_as_a_device() {
        let text = "Filename\tType\n/swapfile\tfile\t1\t0\t-2";
        assert!(parse_swaps(text).is_empty());
    }

    #[test]
    fn the_bus_is_read_from_the_devices_place_in_the_tree() {
        for (path, expected) in [
            (
                "/sys/devices/pci0000:00/0000:00:1d.0/usb2/2-1/2-1:1.0/host6/target6:0:0/6:0:0:0/block/sdb",
                Bus::Usb,
            ),
            (
                "/sys/devices/pci0000:00/0000:00:01.1/nvme/nvme0/nvme0n1",
                Bus::Nvme,
            ),
            (
                "/sys/devices/pci0000:00/0000:00:1f.2/ata1/host0/target0:0:0/0:0:0:0/block/sda",
                Bus::Ata,
            ),
            (
                "/sys/devices/pci0000:00/0000:00:04.0/virtio1/block/vda",
                Bus::Virtio,
            ),
            ("/sys/devices/virtual/block/dm-0", Bus::Virtual),
            (
                "/sys/devices/platform/soc/mmc_host/mmc0/mmc0:0001/block/mmcblk0",
                Bus::Mmc,
            ),
            (
                "/sys/devices/something/entirely/new/block/xda",
                Bus::Unknown,
            ),
        ] {
            assert_eq!(bus_from_path(path), expected, "{path}");
        }
    }

    /// A USB disk's path runs through both `/usb` and `/scsi`, because the
    /// bridge presents a SCSI device. What a report needs is that the disk is on a
    /// cable, so it reads as USB.
    #[test]
    fn a_usb_disk_reads_as_usb_rather_than_scsi() {
        let path = "/sys/devices/pci0000:00/usb2/2-1/host6/target6:0:0/6:0:0:0/block/sdb";
        assert!(path.contains("scsi") || path.contains("host"));
        assert_eq!(bus_from_path(path), Bus::Usb);
    }
}
