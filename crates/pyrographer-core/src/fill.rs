//! Detection of reads that come back as constant fill.
//!
//! Some loaders, on some boards, answer a read they cannot serve with a buffer they
//! never filled. Every sector holds the same repeated byte, and the command reports
//! *success*. A dump taken across such a region looks complete. Nothing in the
//! transfer marks the bytes as invalid. A second read returns the same bytes, so
//! reading the region again does not reveal the failure.
//!
//! Every verb that reads flash from a device therefore scans what it reads:
//! [`dump`](crate::verbs::dump), [`verify`](crate::verbs::verify) and
//! [`clone`](crate::verbs::clone). A clone scans its *source*. A clone across a
//! silent read failure writes the fill byte onto the destination's flash.
//!
//! As each window comes off the device, it is fed to a [`FillScanner`]. The scanner
//! finds maximal runs of sectors that all read back as one byte, and returns them
//! in a [`FillReport`]. It is pure, moving no bytes and reading no clock, so it is
//! unit-tested against scripted windows without a device.
//!
//! ## Blank and suspicious runs
//!
//! A run of any byte other than `0x00` or `0xff` is
//! [suspicious](FillRun::is_blank). Apart from those two values, flash does not
//! ordinarily hold megabytes of one repeated byte. A long run of any other byte is
//! what a silent read failure looks like.
//!
//! A run of `0x00` or `0xff` is *blank*, because that is how unallocated or erased
//! flash ordinarily reads. A front-end warns about suspicious runs only. A warning
//! on every zeroed region would teach a person to ignore the warning.
//!
//! A silent read failure whose fill byte is `0x00` or `0xff` is therefore
//! **indistinguishable from blank flash**, and is not flagged. The classification
//! is a host-side heuristic, not a claim the device made. **\[WEAK\]** The one
//! board that has shown this behavior fills with `0xcc`.

/// Minimum length, in bytes, of a constant-fill run worth reporting.
///
/// Below this length, a run of one byte is ordinary, such as a zeroed structure or
/// a stretch of padding. The read failures this module detects run far longer: the
/// one measured wall is tens of megabytes. The threshold is one window
/// (`WINDOW_BYTES` in [`crate::verbs`]), the unit every read verb moves.
const FILL_MIN_BYTES: u64 = 1 << 20;

/// A maximal run of sectors that all read back as a single repeated byte.
///
/// A run is maximal and contiguous. It stops at the first sector that holds a
/// different byte, is not uniform, or does not follow the previous sector. A run
/// is kept only once it reaches the report threshold, one mebibyte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FillRun {
    /// The device LBA of the run's first sector.
    first_lba: u64,
    /// How many sectors the run spans.
    sectors: u64,
    /// The byte every one of those sectors read back as.
    byte: u8,
    /// The device's sector size, so the run can report its own byte extents.
    sector_size: u32,
}

impl FillRun {
    /// The device LBA of the run's first sector.
    pub fn first_lba(&self) -> u64 {
        self.first_lba
    }

    /// One past the run's last sector, as a device LBA.
    pub fn end_lba(&self) -> u64 {
        self.first_lba.saturating_add(self.sectors)
    }

    /// How many sectors the run spans.
    pub fn sectors(&self) -> u64 {
        self.sectors
    }

    /// The byte every sector in the run read back as.
    pub fn byte(&self) -> u8 {
        self.byte
    }

    /// How many bytes the run spans.
    pub fn bytes(&self) -> u64 {
        self.sectors.saturating_mul(u64::from(self.sector_size))
    }

    /// The byte offset of the run's first sector from the start of the device.
    ///
    /// It gives a position in the units a person reads. The read wall on the one
    /// measured board begins at sector 65536, which is 32 MiB into the flash.
    pub fn first_byte(&self) -> u64 {
        self.first_lba.saturating_mul(u64::from(self.sector_size))
    }

    /// Whether the run is the ordinary look of blank flash rather than a suspected
    /// silent read failure.
    ///
    /// `0x00` (unallocated or zeroed) and `0xff` (erased) are blank. Any other
    /// byte is suspicious. The [module docs](self) explain the limitation this
    /// line accepts.
    pub fn is_blank(&self) -> bool {
        self.byte == 0x00 || self.byte == 0xff
    }
}

/// Every constant-fill run a single read verb found.
///
/// The verb is a [`dump`](crate::verbs::dump), a [`verify`](crate::verbs::verify),
/// or a [`clone`](crate::verbs::clone) watching the source it copied.
///
/// Empty is the ordinary case: a read of real, varied data has no runs at all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FillReport {
    runs: Vec<FillRun>,
}

impl FillReport {
    /// Whether nothing was found, the ordinary outcome of a healthy read.
    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }

    /// Every run, blank and suspicious alike, in the order they were read.
    pub fn runs(&self) -> &[FillRun] {
        &self.runs
    }

    /// The runs that look like a silent read failure rather than blank flash.
    ///
    /// This is the set a caller warns about. [`FillRun::is_blank`] draws the line.
    pub fn suspicious(&self) -> impl Iterator<Item = &FillRun> {
        self.runs.iter().filter(|run| !run.is_blank())
    }

    /// The runs that are the ordinary look of erased or unallocated flash.
    pub fn blank(&self) -> impl Iterator<Item = &FillRun> {
        self.runs.iter().filter(|run| run.is_blank())
    }

    /// Whether any run looks like a silent read failure.
    pub fn has_suspicious(&self) -> bool {
        self.suspicious().next().is_some()
    }
}

/// A run in progress, before it is long enough to keep or a boundary ends it.
struct Open {
    byte: u8,
    first_lba: u64,
    sectors: u64,
}

/// Finds constant-fill runs in the windows of a streaming read.
///
/// It is fed one window at a time as the read produces them. It carries only the
/// run currently open across window boundaries, never an image. So it costs the
/// same on a 4 KiB dump and a 116 GiB one. [`observe`](Self::observe) takes each
/// window and its starting LBA. [`finish`](Self::finish) closes the last run and
/// hands back the [`FillReport`].
pub struct FillScanner {
    sector_size: u32,
    threshold_sectors: u64,
    open: Option<Open>,
    runs: Vec<FillRun>,
}

impl FillScanner {
    /// A scanner for a device with the given sector size.
    ///
    /// The report threshold is one mebibyte rounded down to whole sectors, and at
    /// least one sector. So no sector size, however large, can make it zero.
    pub fn new(sector_size: u32) -> Self {
        let sector_size = sector_size.max(1);
        let threshold_sectors = (FILL_MIN_BYTES / u64::from(sector_size)).max(1);
        Self::with_threshold(sector_size, threshold_sectors)
    }

    /// The shared constructor, taking the threshold directly so a test can pin
    /// short runs without moving a megabyte of scripted bytes.
    fn with_threshold(sector_size: u32, threshold_sectors: u64) -> Self {
        Self {
            sector_size,
            threshold_sectors,
            open: None,
            runs: Vec::new(),
        }
    }

    /// Observe one window of freshly-read bytes.
    ///
    /// `first_lba` is the device LBA of the window's first sector. `window` is a
    /// whole number of sectors. A trailing partial sector, which the streaming
    /// verbs never produce, is ignored.
    pub fn observe(&mut self, first_lba: u64, window: &[u8]) {
        let sector_size = self.sector_size as usize;
        // `first_lba..` is infinite and `chunks_exact` is finite, so the zip stops
        // with the sectors and each carries its own device LBA.
        for (lba, sector) in (first_lba..).zip(window.chunks_exact(sector_size)) {
            match uniform_byte(sector) {
                Some(byte) => self.extend(lba, byte),
                None => self.close(),
            }
        }
    }

    /// Add one uniform sector to the open run, or start a fresh one.
    fn extend(&mut self, lba: u64, byte: u8) {
        if let Some(open) = &mut self.open
            && open.byte == byte
            && open.first_lba + open.sectors == lba
        {
            open.sectors += 1;
            return;
        }
        // A different byte, a gap in the LBA, or nothing open: close what was
        // there and begin again from this sector.
        self.close();
        self.open = Some(Open {
            byte,
            first_lba: lba,
            sectors: 1,
        });
    }

    /// End the open run. A run that reached the threshold is kept.
    fn close(&mut self) {
        if let Some(open) = self.open.take()
            && open.sectors >= self.threshold_sectors
        {
            self.runs.push(FillRun {
                first_lba: open.first_lba,
                sectors: open.sectors,
                byte: open.byte,
                sector_size: self.sector_size,
            });
        }
    }

    /// Close the last open run and return everything found.
    pub fn finish(mut self) -> FillReport {
        self.close();
        FillReport { runs: self.runs }
    }
}

/// The one byte a sector holds throughout, or `None` for a sector that is mixed or
/// empty. No real sector is empty.
fn uniform_byte(sector: &[u8]) -> Option<u8> {
    let first = *sector.first()?;
    sector.iter().all(|&byte| byte == first).then_some(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A window of `sectors` sectors, each a whole sector of `byte`.
    fn filled(sectors: u64, byte: u8, sector_size: usize) -> Vec<u8> {
        vec![byte; sectors as usize * sector_size]
    }

    #[test]
    fn a_read_of_nothing_finds_nothing() {
        let report = FillScanner::new(512).finish();
        assert!(report.is_empty());
        assert_eq!(report.runs(), &[]);
    }

    #[test]
    fn varied_data_has_no_runs() {
        // Every sector a different byte: nothing uniform runs for long.
        let mut scanner = FillScanner::with_threshold(512, 4);
        let window: Vec<u8> = (0..16u16)
            .flat_map(|i| std::iter::repeat_n(i as u8, 512))
            .collect();
        scanner.observe(0, &window);
        assert!(scanner.finish().is_empty());
    }

    #[test]
    fn a_run_shorter_than_the_threshold_is_dropped() {
        let mut scanner = FillScanner::with_threshold(512, 4);
        // Three uniform sectors, then a non-uniform one to close the run.
        scanner.observe(0, &filled(3, 0xcc, 512));
        scanner.observe(3, &[0xaa, 0xbb].repeat(256)); // one mixed sector
        assert!(scanner.finish().is_empty());
    }

    #[test]
    fn a_run_at_the_threshold_is_kept() {
        let mut scanner = FillScanner::with_threshold(512, 4);
        scanner.observe(10, &filled(4, 0xcc, 512));
        let report = scanner.finish();

        assert_eq!(report.runs().len(), 1);
        let run = report.runs()[0];
        assert_eq!(run.first_lba(), 10);
        assert_eq!(run.sectors(), 4);
        assert_eq!(run.end_lba(), 14);
        assert_eq!(run.byte(), 0xcc);
        assert_eq!(run.bytes(), 4 * 512);
        assert_eq!(run.first_byte(), 10 * 512);
        assert!(report.has_suspicious());
    }

    #[test]
    fn a_run_spanning_two_windows_is_one_run() {
        let mut scanner = FillScanner::with_threshold(512, 4);
        // Two sectors, then three more contiguous with them: one run of five.
        scanner.observe(0, &filled(2, 0xcc, 512));
        scanner.observe(2, &filled(3, 0xcc, 512));
        let report = scanner.finish();

        assert_eq!(report.runs().len(), 1);
        assert_eq!(report.runs()[0].sectors(), 5);
        assert_eq!(report.runs()[0].first_lba(), 0);
    }

    #[test]
    fn a_different_byte_breaks_the_run() {
        let mut scanner = FillScanner::with_threshold(512, 4);
        scanner.observe(0, &filled(4, 0xcc, 512));
        scanner.observe(4, &filled(4, 0xdd, 512));
        let report = scanner.finish();

        assert_eq!(report.runs().len(), 2);
        assert_eq!(report.runs()[0].byte(), 0xcc);
        assert_eq!(report.runs()[1].byte(), 0xdd);
        assert_eq!(report.runs()[1].first_lba(), 4);
    }

    #[test]
    fn a_non_uniform_sector_breaks_the_run() {
        let mut scanner = FillScanner::with_threshold(512, 4);
        scanner.observe(0, &filled(4, 0xcc, 512));
        let mut mixed = vec![0xcc; 512];
        mixed[0] = 0x00;
        scanner.observe(4, &mixed);
        scanner.observe(5, &filled(4, 0xcc, 512));
        let report = scanner.finish();

        assert_eq!(report.runs().len(), 2);
        assert_eq!(report.runs()[0].first_lba(), 0);
        assert_eq!(report.runs()[1].first_lba(), 5);
    }

    #[test]
    fn a_gap_in_the_lba_breaks_the_run() {
        // The verbs feed contiguous windows, but the contiguity guard is what
        // makes a run mean "these sectors, next to each other on the device".
        let mut scanner = FillScanner::with_threshold(512, 4);
        scanner.observe(0, &filled(4, 0xcc, 512));
        scanner.observe(100, &filled(4, 0xcc, 512));
        let report = scanner.finish();

        assert_eq!(report.runs().len(), 2);
        assert_eq!(report.runs()[0].first_lba(), 0);
        assert_eq!(report.runs()[1].first_lba(), 100);
    }

    #[test]
    fn zero_and_ff_are_blank_not_suspicious() {
        let mut scanner = FillScanner::with_threshold(512, 4);
        scanner.observe(0, &filled(4, 0x00, 512));
        scanner.observe(4, &filled(4, 0xff, 512));
        let report = scanner.finish();

        assert_eq!(report.runs().len(), 2);
        assert!(!report.has_suspicious());
        assert_eq!(report.blank().count(), 2);
        assert_eq!(report.suspicious().count(), 0);
    }

    #[test]
    fn a_blank_run_and_a_suspicious_run_are_told_apart() {
        let mut scanner = FillScanner::with_threshold(512, 4);
        scanner.observe(0, &filled(4, 0x00, 512)); // blank
        scanner.observe(4, &filled(4, 0xcc, 512)); // suspicious
        let report = scanner.finish();

        assert_eq!(report.suspicious().count(), 1);
        assert_eq!(report.suspicious().next().unwrap().byte(), 0xcc);
        assert_eq!(report.blank().count(), 1);
        assert_eq!(report.blank().next().unwrap().byte(), 0x00);
    }

    #[test]
    fn the_default_threshold_is_one_mebibyte_of_sectors() {
        assert_eq!(FillScanner::new(512).threshold_sectors, 2048);
        assert_eq!(FillScanner::new(4096).threshold_sectors, 256);
        // A sector larger than the whole threshold still reports a single sector.
        assert_eq!(FillScanner::new(1 << 21).threshold_sectors, 1);
    }
}
