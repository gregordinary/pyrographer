//! A job: one verb, run as a task separate from the frame loop.
//!
//! egui is immediate mode. `App::ui` runs every frame and must not block. A verb
//! is an async fn that something has to drive, and the native USB transport
//! blocks the thread that drives it. That blocking is by design, and
//! `transport/usb.rs` gives the reason. A verb therefore runs as a task of its
//! own, and the frame loop reads what the task has published so far.
//!
//! Core already provides the three things a running job needs:
//!
//! - **Progress** is a closure rather than a channel.
//!   [`ProgressSink`](pyrographer_core::progress::ProgressSink) is a
//!   `&mut dyn FnMut(Progress)`, so a job writes each event into a slot the next
//!   frame reads. It then asks the frame loop for a repaint through [`Wake`].
//! - **Cancellation** is [`Cancel`], a shared flag checked at every window
//!   boundary. A cancel button needs nothing more.
//! - **The agent** goes out with the job and comes back when it ends, including
//!   when it ends in an error. An agent must outlive the errors it reports. A
//!   desynchronized agent refuses every command after the error, and that refusal
//!   means something only to a consumer that still holds the agent.
//!
//! [`Wake`]: crate::platform::Wake

use pyrographer_core::Error;
use pyrographer_core::agent::{FlashAgent, FlashInfo, ReadBack};
use pyrographer_core::bootstrap::ingenic::IngenicLoader;
use pyrographer_core::bootstrap::starfive;
use pyrographer_core::codec::console as console_codec;
use pyrographer_core::codec::ingenic_boot::CpuInfo;
use pyrographer_core::codec::rkboot::LoaderImage;
use pyrographer_core::codec::rockusb::{Capability, ResetMode, StorageMedium};
use pyrographer_core::console::{self, ConsoleSink, Watched};
use pyrographer_core::fill::FillReport;
use pyrographer_core::firmware;
use pyrographer_core::image::ImageReader;
use pyrographer_core::layout::Layout;
use pyrographer_core::partition::{PartitionTable, TableFormat};
use pyrographer_core::progress::{Cancel, Progress};
use pyrographer_core::recovery::{self, ConfirmedRecovery};
use pyrographer_core::soc::Soc;
use pyrographer_core::transport::{Serial, Transport};
use pyrographer_core::uboot::{BootPlan, ConfirmedBoot, Gadget, GadgetDevice, UBoot};
use pyrographer_core::verbs::{
    self, ClonePlan, ConfirmedClone, ConfirmedFirmwareWrite, ConfirmedSegmentedWrite,
    ConfirmedWrite, FirmwarePlan, FirmwareWrite, ParamAuthorSource, ParamMedium, SegmentedPlan,
    TableAction, WritePlan,
};

use crate::platform::{BoxedReader, BoxedWriter, Handle, Shared, Wake, lock, share};

/// Where a write is aimed.
///
/// The two forms are not equally safe, and both exist for that reason. An LBA is
/// arithmetic a person did, and nothing can check it. A name is resolved against
/// the device's own table, which records where the partition *ends* as well as
/// where it begins. A name is therefore the only form that can refuse an image
/// too big to fit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Aim {
    /// A raw logical block address.
    Lba(u64),
    /// A partition, by the name the device's table gives it.
    Partition(String),
}

/// Where a table authoring reads its partition layout.
///
/// Each variant holds the text a person picked, not a parsed [`Layout`]. Parsing
/// needs the device's own sector count, to resolve a `-` extent and to validate
/// the fit. That count is read on the job thread when the plan runs, as the CLI
/// parses inside its async block.
///
/// [`Native`](Self::Native) and [`Mtdparts`](Self::Mtdparts) each build a
/// [`Layout`]. [`Text`](Self::Text) is an existing parameter block's whole text,
/// framed verbatim so that its `FIRMWARE_VER` and every other key survive. It
/// serves a parameter authoring only, because a GPT is not built from parameter
/// text.
#[derive(Clone)]
pub enum LayoutSource {
    /// A native-format layout: `name first_lba sectors [type] [uuid=<GUID>]` lines.
    Native(String),
    /// A board's own `mtdparts=` line.
    Mtdparts(String),
    /// An existing parameter's whole text, kept verbatim. Parameter authoring only.
    Text(String),
}

/// What a job does to a board.
///
/// The destructive tasks carry a [`ConfirmedWrite`], a [`ConfirmedClone`] or a
/// [`ConfirmedSegmentedWrite`], which only a plan can produce. A job that overwrites
/// a board without a person's agreement therefore cannot be constructed.
pub enum Task {
    /// Read the flash geometry.
    Info,
    /// Read the partition table.
    Partitions,
    /// Ask the loader which SoC it is on.
    ChipVersion,
    /// Ask the loader what it says it can do.
    ///
    /// This is the device's account of itself. [`FlashAgent::caps`] is
    /// pyrographer's account of what the backend implements. The command only
    /// reads, and nothing is gated on the answer.
    ///
    /// [`FlashAgent::caps`]: pyrographer_core::agent::FlashAgent::caps
    Capability,
    /// Ask which storage medium the backend is currently addressing.
    ///
    /// Every LBA the other verbs take is an offset into one medium. On a board
    /// with more than one medium populated, one sector number names a place on
    /// each. The answer says which medium a sector number refers to. This task
    /// reports the medium and never switches it.
    StorageMedium,
    /// End the board's session.
    ///
    /// The board reboots, reboots into mass storage or into maskrom, or powers
    /// off, as `mode` says.
    Reset {
        /// Which ending to ask for.
        mode: ResetMode,
    },
    /// Stream sectors off the board into a sink.
    Dump {
        /// The first sector to read.
        lba: u64,
        /// How many.
        sectors: u64,
        /// Where they go.
        sink: BoxedWriter,
        /// What to call the sink on screen.
        name: String,
    },
    /// Compare the flash against an image.
    Verify {
        /// The first sector to compare.
        lba: u64,
        /// The image to compare it against.
        image: BoxedReader,
        /// How much of the image there is, and so how far the comparison runs.
        image_bytes: u64,
    },
    /// The dry run: ask the board what a write would touch, and stop.
    ///
    /// It is the write's own call, ended one step early, rather than a separate
    /// path. Its report therefore describes what the write itself would do.
    PlanWrite {
        /// Where the write would land.
        aim: Aim,
        /// How many bytes it would put there.
        image_bytes: u64,
        /// The image, read only as far as its first bytes.
        ///
        /// A firmware package or a loader container written raw does not boot, and
        /// [`verbs::image_refusal`] judges an image by its first bytes. The plan
        /// asks it here, before the board is asked anything, so a person is not
        /// shown a plan for a write that will be refused. The write opens the image
        /// again from its first byte.
        image: BoxedReader,
        /// The SoC the person says this board is, for the wrong-loader gate.
        soc: Option<Soc>,
    },
    /// Write an image a person agreed to, and read every window of it back.
    ///
    /// The backend sets the read-back's timing: window by window, or once the
    /// region is committed. [`Task::read_back`] gives the answer for this write.
    Write {
        /// The plan they agreed to.
        confirmed: ConfirmedWrite,
        /// The bytes, which are exactly as many as the plan says.
        image: BoxedReader,
    },
    /// The dry run for a clone.
    PlanClone {
        /// The SoC the person says the *destination* is. The destination is the
        /// board that is written, so the gate asks about it.
        soc: Option<Soc>,
    },
    /// Copy one board onto another, reading back every window on the destination.
    Clone {
        /// The plan a person agreed to.
        confirmed: ConfirmedClone,
    },
    /// The dry run for a GPT repair.
    ///
    /// It reads both GPT copies. If one is damaged and the other intact, it
    /// describes the copy that would be rewritten.
    PlanRepairTable {
        /// The SoC the person says this board is, for the wrong-loader gate. A
        /// repair writes flash, so it is gated like any other write.
        soc: Option<Soc>,
    },
    /// The dry run for a Rockchip parameter repair.
    ///
    /// It reads every parameter copy. If one is damaged and another intact, it
    /// describes the copies that would be rewritten from the intact one.
    PlanRepairParam {
        /// The SoC the person says this board is, for the wrong-loader gate.
        soc: Option<Soc>,
    },
    /// The dry run for authoring a fresh Rockchip parameter table from a layout or
    /// an existing block's text.
    PlanAuthorParam {
        /// The layout or block text to author from, parsed on the job thread.
        source: LayoutSource,
        /// Where the copies go, and what a layout's offsets count from.
        medium: ParamMedium,
        /// The SoC the person says this board is, for the wrong-loader gate.
        soc: Option<Soc>,
    },
    /// The dry run for authoring a fresh GPT from a layout.
    PlanAuthorGpt {
        /// The layout to author from, parsed on the job thread. A GPT is not built
        /// from parameter text, so [`LayoutSource::Text`] is refused here.
        source: LayoutSource,
        /// The SoC the person says this board is, for the wrong-loader gate.
        soc: Option<Soc>,
    },
    /// Write a table a person agreed to, and read the written sectors back.
    ///
    /// The table is a repair that rewrites damaged copies, or a freshly authored
    /// table.
    WriteTable {
        /// The plan a person agreed to.
        confirmed: ConfirmedSegmentedWrite,
    },
    /// The dry run for writing a firmware package: read it and check it, then
    /// plan every partition image, the GPT and the ID block.
    ///
    /// The package is read once, front to back, on the job thread. Its checksum
    /// covers gigabytes, and the window keeps drawing while it is checked.
    PlanFirmware {
        /// The package, from its first byte.
        package: BoxedReader,
        /// How long it is.
        package_bytes: u64,
        /// The SoC the person says this board is, for the wrong-loader gate.
        soc: Option<Soc>,
    },
    /// The dry run for writing a loader's ID block alone.
    PlanIdBlock {
        /// The loader file: a loader container, or a firmware package whose loader
        /// is used.
        loader: BoxedReader,
        /// How long the file is.
        loader_bytes: u64,
        /// The SoC the person says this board is, for the wrong-loader gate.
        soc: Option<Soc>,
    },
    /// Write a firmware plan a person agreed to, and read every window back.
    WriteFirmware {
        /// The plan a person agreed to.
        confirmed: ConfirmedFirmwareWrite,
        /// The package again, from its first byte, for a plan that streams its
        /// partition images from it. `None` for an ID block alone.
        package: Option<BoxedReader>,
    },
}

impl Task {
    /// What to call this on screen while it runs.
    pub fn label(&self) -> &'static str {
        match self {
            Task::Info => "Reading flash info",
            Task::Partitions => "Reading the partition table",
            Task::ChipVersion => "Asking the loader",
            Task::Capability => "Asking what the loader can do",
            Task::StorageMedium => "Asking which medium is addressed",
            Task::Reset { .. } => "Resetting",
            Task::Dump { .. } => "Reading",
            Task::Verify { .. } => "Verifying",
            Task::PlanWrite { .. }
            | Task::PlanClone { .. }
            | Task::PlanRepairTable { .. }
            | Task::PlanRepairParam { .. }
            | Task::PlanAuthorParam { .. }
            | Task::PlanAuthorGpt { .. }
            | Task::PlanIdBlock { .. } => "Planning",
            Task::PlanFirmware { .. } => "Checking the firmware package",
            Task::Write { .. } => "Writing",
            Task::Clone { .. } => "Cloning",
            Task::WriteTable { .. } => "Writing the table",
            Task::WriteFirmware { .. } => "Writing the firmware",
        }
    }

    /// When this task's write is proved to have landed, or `None` for a task that
    /// overwrites nothing.
    ///
    /// Most tasks overwrite nothing. The answer is read from the confirmed plan
    /// rather than decided here. The backend owns the answer, and the plan a
    /// person agreed to already states it.
    pub fn read_back(&self) -> Option<ReadBack> {
        match self {
            Task::Write { confirmed, .. } => Some(confirmed.plan().read_back),
            Task::Clone { confirmed } => Some(confirmed.plan().destination.read_back),
            Task::WriteTable { confirmed } => Some(confirmed.plan().read_back),
            Task::WriteFirmware { confirmed, .. } => Some(confirmed.plan().read_back),
            _ => None,
        }
    }

    /// Whether this task needs the source board as well as the target one.
    pub fn needs_source(&self) -> bool {
        matches!(self, Task::PlanClone { .. } | Task::Clone { .. })
    }
}

/// What a job produced.
pub enum Report {
    /// The flash geometry.
    Info(FlashInfo),
    /// The partition table, or the finding that there is none. A table that is
    /// present and fails its checksum is not reported here. It is an
    /// [`Error::CorruptTable`], because reporting damage as an empty list would
    /// hide it.
    Partitions(Option<PartitionTable>),
    /// What the loader answered when asked which SoC it is on, uninterpreted.
    ChipVersion(Vec<u8>),
    /// The loader's own account of what it can do. `None` from a backend that
    /// answers no capability query.
    Capability(Option<Capability>),
    /// The medium the backend is currently addressing. `None` from a backend that
    /// addresses no single one.
    StorageMedium(Option<StorageMedium>),
    /// The board has taken the reset it was given, and left.
    Reset {
        /// Which ending it was asked for. The mode owns the sentence that describes
        /// what happens next, so the report carries the mode rather than the wording.
        mode: ResetMode,
    },
    /// A dump landed.
    Dumped {
        /// How many bytes.
        bytes: u64,
        /// Where they went.
        name: String,
        /// Any region that read back as constant fill, which can be a silent
        /// read failure reported with a success status. Empty on a healthy read.
        fill: FillReport,
    },
    /// The flash holds what the image holds.
    Verified {
        /// How much of it was compared.
        bytes: u64,
        /// Constant-fill regions seen while reading the device. A verify that
        /// passes over one shows that two copies agree. It does not confirm the
        /// data.
        fill: FillReport,
    },
    /// The dry run's answer: what a write would touch. This is the screen a
    /// person reads before they agree to anything.
    Planned(WritePlan),
    /// The same, for a clone.
    PlannedClone(ClonePlan),
    /// The same, for a table write, whether a repair or an authoring.
    ///
    /// It names the copies that would be written and the partitions the resulting
    /// table holds. This is the screen a person reads before they agree to
    /// overwrite the table.
    PlannedTable(SegmentedPlan),
    /// A write landed, and every window of it was read back.
    Wrote {
        /// How many bytes.
        bytes: u64,
    },
    /// A clone landed, and every window of it was read back on the destination.
    Cloned {
        /// How many bytes.
        bytes: u64,
        /// Constant-fill regions seen while reading the *source*. A clone across
        /// a silent read failure copies the fill byte onto the destination, so
        /// this is a finding about the board just written. Empty on a healthy
        /// read.
        fill: FillReport,
    },
    /// A loader was uploaded to a maskrom board, which is expected to re-enumerate
    /// in loader mode.
    Bootstrapped {
        /// The container's own claim about which SoC it was built for, raw, or
        /// `None` for bare stage files that carry no container.
        ///
        /// The maskrom gate does its work through this field. With one SoC pinned,
        /// only `rk3576` can be named. The gate therefore refuses only a file that
        /// claims another SoC while `rk3576` is named. For every other upload, the
        /// gate's work is to make the claim visible, and that requires something to
        /// render it. It is shown uninterpreted, as a `chipver` reply is.
        chip: Option<Vec<u8>>,
    },
    /// An Ingenic boot-ROM board was bootstrapped to DFU.
    ///
    /// The two-stage loader was uploaded, and the board is expected to re-enumerate
    /// as a DFU gadget. The variant carries the SoC's own [`CpuInfo`] magic, so the
    /// report can show it raw for a person to read and record. This is the one
    /// point in the flow where the identity is readable. A `soc` gate entry is
    /// pinned from this value.
    IngenicBootstrapped(CpuInfo),
    /// A console session ended: what a watch saw, what a command printed, or the
    /// gadget that was started.
    ///
    /// A planned boot override is not reported here. It is a screen a person agrees
    /// to rather than something that happened, so it goes to the session's pending
    /// plan.
    Console(ConsoleReport),
    /// A StarFive board was recovered over serial.
    ///
    /// The agent was uploaded, and the SPL, the U-Boot or both were written through
    /// its menu. The agent reported each write as done, but the write was **not**
    /// read back, because the recovery protocol cannot read flash.
    Recovered,
    /// A StarFive board was booted into U-Boot in RAM over serial, and U-Boot is
    /// stopped at its prompt. Nothing was written.
    UartBooted,
    /// A table write landed, and every window of it was read back.
    ///
    /// The write was a repair that rewrote damaged copies, or a freshly authored
    /// table. The variant carries the format and whether the write authored or
    /// repaired, so the report states which happened.
    TableWritten {
        /// The format written: GPT or Rockchip parameter.
        format: TableFormat,
        /// Whether a fresh table was authored (`true`) or damaged copies of an
        /// existing one repaired (`false`).
        authored: bool,
    },
    /// The dry run's answer for a firmware package or an ID block: every run the
    /// write lays down. This is the screen a person reads before they agree.
    PlannedFirmware(FirmwarePlan),
    /// A firmware write landed, and every window of it was read back.
    FirmwareWritten {
        /// Whether it was an ID block alone (`true`) or a whole package (`false`).
        id_block_only: bool,
        /// How many runs it laid down.
        runs: usize,
        /// How many bytes, across every run.
        bytes: u64,
    },
}

/// What a job ended as.
pub type Outcome = Result<Report, Error>;

/// What a running job publishes and the frame loop reads.
pub struct Published<T: Transport> {
    /// The most recent progress event, in bytes and never percentages. Only the
    /// caller has a clock, and the caller is the frame loop.
    progress: Option<Progress>,
    /// Set exactly once, when the job ends, however it ends.
    ended: Option<Ended<T>>,
}

impl<T: Transport> Default for Published<T> {
    fn default() -> Self {
        Self {
            progress: None,
            ended: None,
        }
    }
}

/// A job that has finished.
///
/// A job finishes in one of two ways, because a native job runs on a thread that
/// can panic. On the ordinary path the verb returns, the agents come back, and
/// the connection is restored. If the verb, a progress sink or a reader panics,
/// the unwind drops the future and the agents with it. There is then nothing to
/// restore, and the `EndOnDrop` guard publishes [`Died`](Ended::Died) instead.
/// The frame loop learns that the job is over, and can land the connection
/// somewhere other than a permanent [`Busy`](crate::state::Connection::Busy).
//
// The `Returned` variant is far larger than `Died` -- it carries the agents home,
// where `Died` carries a bool -- and clippy would have the big one boxed. Boxing
// would pessimize the common path (every job that does not panic) with an
// allocation and an indirection, to save memory on a value that exists once per
// job and is taken the next frame. The size difference is deliberate.
#[allow(clippy::large_enum_variant)]
pub enum Ended<T: Transport> {
    /// The verb ran to a conclusion and handed its agents back.
    Returned {
        /// The board the job acted on.
        target: FlashAgent<T>,
        /// The board a clone was reading, if there was one.
        source: Option<FlashAgent<T>>,
        /// What it produced, or what went wrong.
        outcome: Outcome,
    },
    /// The job's thread died before it could hand anything back. The agents were
    /// dropped in the unwind, so the connections they came from cannot be restored
    /// and must be reopened.
    Died {
        /// Whether the job also held a source board (a clone). If it did, both
        /// connections are landed, not only the target's.
        had_source: bool,
    },
}

/// Stopping a job, written once for every kind of job.
///
/// Four kinds of job carry different things back, so four slots hold a job. Three
/// carry an agent, a [`CpuInfo`] and a transcript, and the fourth carries nothing.
/// None of them collapses into the others. What they share is the flag.
/// [`Cancel`] is a shared bool that core checks at every window, block and read
/// boundary. Stopping is therefore one field and two methods, written here once
/// rather than in four copies.
pub trait Stoppable {
    /// The flag the running work checks.
    fn cancel_flag(&self) -> &Cancel;

    /// Ask the work to stop at its next boundary.
    ///
    /// **Canceling is not an undo.** What a write has already put on the flash
    /// stays there. Stopping between windows only means that the device is never
    /// left partway through a command. A console session differs: canceling stops
    /// only the reading, and anything already typed has been sent.
    fn cancel(&self) {
        self.cancel_flag().cancel();
    }

    /// Whether a person has asked it to stop.
    fn is_canceling(&self) -> bool {
        self.cancel_flag().is_canceled()
    }
}

/// A job that moves bytes, and the arithmetic every panel does over its progress.
///
/// [`Progress`] reports bytes and never percentages or rates, because only the
/// caller has a clock. Turning bytes into a bar and a throughput figure is the
/// front-end's work. It is the same six lines whichever job published the event,
/// so it is written once here. Each handle supplies the two things only it knows:
/// the event it last published, and when it began.
///
/// The clock is the caller's. `now` is egui's frame time, because
/// `std::time::Instant::now()` **panics on `wasm32`**. One call to it would crash
/// the web flasher on the first progress event.
pub trait Measured: Stoppable {
    /// The most recent progress event, if the work reports any.
    fn progress(&self) -> Option<Progress>;

    /// When it began, on the caller's frame clock.
    fn started_at(&self) -> f64;

    /// How far along, between zero and one, if there is a total to measure by.
    ///
    /// `None` for a transfer of no bytes, which has no meaningful fraction.
    fn fraction(&self) -> Option<f32> {
        match self.progress()? {
            Progress::Started { total_bytes } if total_bytes > 0 => Some(0.0),
            Progress::Advanced {
                done_bytes,
                total_bytes,
            } if total_bytes > 0 => Some(done_bytes as f32 / total_bytes as f32),
            Progress::Finished { .. } => Some(1.0),
            _ => None,
        }
    }

    /// Bytes moved so far.
    fn done_bytes(&self) -> u64 {
        match self.progress() {
            Some(Progress::Advanced { done_bytes, .. } | Progress::Finished { done_bytes }) => {
                done_bytes
            }
            _ => 0,
        }
    }

    /// Bytes the work said it would move in all.
    ///
    /// A recovery sends several files in turn: the agent, then one stage per menu
    /// option. For a recovery this is the *current* transfer's total, which is what
    /// a per-transfer bar needs. The recovery's plan counts the whole.
    fn total_bytes(&self) -> Option<u64> {
        match self.progress()? {
            Progress::Started { total_bytes } | Progress::Advanced { total_bytes, .. } => {
                Some(total_bytes)
            }
            Progress::Finished { done_bytes } => Some(done_bytes),
        }
    }

    /// How long it has been running, against the frame clock.
    fn elapsed(&self, now: f64) -> f64 {
        (now - self.started_at()).max(0.0)
    }

    /// How fast it is going, in bytes a second, against the frame clock.
    ///
    /// Under a per-window read-back, a write counts a window only once it has been
    /// both written and read back. A rate rendered from such a write is therefore
    /// roughly half the raw bus throughput. That is the intended figure, because
    /// the work is the write and the read-back together. A write read back after
    /// its commit counts each window as it is sent.
    fn rate(&self, now: f64) -> Option<f64> {
        let elapsed = self.elapsed(now);
        if elapsed <= 0.0 {
            return None;
        }
        Some(self.done_bytes() as f64 / elapsed)
    }
}

/// A job in flight, from the frame loop's side.
pub struct Job<T: Transport> {
    /// What to call it on screen.
    pub label: &'static str,
    /// When it began, on the caller's clock.
    ///
    /// Core has no clock, so the GUI supplies one, and it is not `std`'s.
    /// [`Progress`] reports bytes and never rates, because only the caller has a
    /// clock. This value comes from egui's frame input rather than from
    /// `std::time::Instant::now()`, which **panics on `wasm32`**. One call to
    /// `Instant::now()` would crash the web flasher on the first progress event.
    pub started_at: f64,
    /// When this job's write can be proved to have landed, from the plan a person
    /// agreed to, or `None` for a job that writes nothing.
    ///
    /// The field carries this rather than a bare `destructive: bool`. The panel
    /// that draws a running write has to say when the write is checked, and the
    /// answer differs by backend. rockusb reads each window back before the next
    /// goes out. A DFU device cannot be read until the region is committed.
    /// [`ReadBack::describe`] words that fact once, and the plan screen already
    /// renders it. A second wording here would be a second place to get it wrong.
    pub read_back: Option<ReadBack>,
    cancel: Cancel,
    shared: Shared<Published<T>>,
}

impl<T: Transport> Stoppable for Job<T> {
    fn cancel_flag(&self) -> &Cancel {
        &self.cancel
    }
}

impl<T: Transport> Measured for Job<T> {
    fn progress(&self) -> Option<Progress> {
        lock(&self.shared).progress
    }

    fn started_at(&self) -> f64 {
        self.started_at
    }
}

impl<T: Transport> Job<T> {
    /// Take the job's ending, if it has ended.
    pub fn take_ended(&self) -> Option<Ended<T>> {
        lock(&self.shared).ended.take()
    }
}

/// A job that has been made but not yet started.
///
/// It separates making a job from running one. The app hands this to
/// [`platform::spawn`](crate::platform::spawn), and a test drives it to completion
/// inline, with no thread. A test can therefore cover a job that completes, a job
/// canceled mid-window, and an agent that becomes desynchronized. None of those
/// tests needs a board or a window.
pub struct Work<T: Transport> {
    target: FlashAgent<T>,
    source: Option<FlashAgent<T>>,
    task: Task,
    shared: Shared<Published<T>>,
    cancel: Cancel,
    wake: Wake,
}

impl<T: Transport> Work<T> {
    /// Make a job, and the handle the frame loop watches it through.
    pub(crate) fn new(
        target: FlashAgent<T>,
        source: Option<FlashAgent<T>>,
        task: Task,
        started_at: f64,
        wake: Wake,
    ) -> (Self, Job<T>) {
        let shared = share(Published::default());
        let cancel = Cancel::new();

        let job = Job {
            label: task.label(),
            started_at,
            read_back: task.read_back(),
            cancel: cancel.clone(),
            shared: shared.clone(),
        };

        let work = Self {
            target,
            source,
            task,
            shared,
            cancel,
            wake,
        };

        (work, job)
    }

    /// Run the verb, publish what it did, and hand the agents back.
    ///
    /// The agents come back whatever the verb returns. A desynchronized agent is
    /// still the one that records the desynchronization, and the connection it
    /// belonged to has to be told. A panic in the verb, a progress sink or a reader
    /// drops the future mid-flight instead. The agents are dropped in the unwind,
    /// and there is nothing to hand back. The `EndOnDrop` guard then publishes
    /// [`Ended::Died`], so the frame loop learns that the job is over rather than
    /// reading it as [`Busy`](crate::state::Connection::Busy) forever.
    pub async fn run(self) {
        let Work {
            mut target,
            mut source,
            task,
            shared,
            cancel,
            wake,
        } = self;
        let had_source = source.is_some();

        // The ending is a drop-obligation. On the ordinary path the closure below
        // publishes `Ended::Returned` and clears the fallback; if the future is
        // dropped first, the guard's own drop publishes `Ended::Died`.
        let guard = EndOnDrop::new(
            shared.clone(),
            wake,
            Box::new(move |published: &mut Published<T>| {
                published.ended = Some(Ended::Died { had_source });
            }),
        );

        let outcome = {
            let mut publish = |event| {
                lock(&shared).progress = Some(event);
                guard.wake();
            };
            execute(&mut target, source.as_mut(), task, &mut publish, &cancel).await
        };

        guard.finish(move |published| {
            published.ended = Some(Ended::Returned {
                target,
                source,
                outcome,
            });
        });
    }
}

/// A running job's obligation to publish an ending, held so that a dropped future
/// still meets it.
///
/// It is the job counterpart of [`Answer`](crate::platform), and exists for the
/// same reason. Every job ends by writing into a shared cell the frame loop reads,
/// and a cell that is still empty reads as *still running*. A verb, a progress sink
/// or a reader can panic. Natively, the unwind drops the driving future and
/// everything it owns. Without this guard, the cell would stay empty and the
/// connection would stay [`Busy`](crate::state::Connection::Busy) forever.
///
/// The guard owns a `fallback` that writes a "died" ending, and its own drop
/// delivers it. [`finish`](Self::finish) publishes the real ending first and clears
/// the fallback, so the drop then does nothing. On the web a panic halts the whole
/// instance, so a drop has nothing left to rescue. The guard costs nothing there,
/// and keeps the two builds the same shape.
struct EndOnDrop<S> {
    shared: Shared<S>,
    wake: Wake,
    fallback: Option<Fallback<S>>,
}

/// The closure an [`EndOnDrop`] fires to write its "died" ending. `Send` natively,
/// because it is held across the `.await` a job thread drives. A tab needs no such
/// bound.
#[cfg(not(target_arch = "wasm32"))]
type Fallback<S> = Box<dyn FnOnce(&mut S) + Send>;

/// The closure an [`EndOnDrop`] fires to write its "died" ending.
#[cfg(target_arch = "wasm32")]
type Fallback<S> = Box<dyn FnOnce(&mut S)>;

/// A published slot whose ending is a plain `Result`, and which can therefore be
/// failed with a sentence.
///
/// Three of the four job kinds hand back a bare outcome: a bootstrap, a recovery
/// and a console session. Their drop obligation is the same two lines with a
/// different sentence in each. This trait lets [`EndOnDrop::armed`] write those
/// two lines once. The fourth kind, [`Published`], carries agents back and fails
/// as [`Ended::Died`] instead. That is a different shape, and it keeps its own
/// fallback.
trait Failable {
    /// Set the ending to a failure, with `why` as its words.
    fn fail(&mut self, why: &'static str);
}

impl<P> Failable for BootstrapPublished<P> {
    fn fail(&mut self, why: &'static str) {
        self.ended = Some(Err(Error::Internal(why.to_string())));
    }
}

impl Failable for RecoveryPublished {
    fn fail(&mut self, why: &'static str) {
        self.ended = Some(Err(Error::Internal(why.to_string())));
    }
}

impl Failable for ConsolePublished {
    fn fail(&mut self, why: &'static str) {
        self.ended = Some(Err(Error::Internal(why.to_string())));
    }
}

impl<S: Failable> EndOnDrop<S> {
    /// Arm the obligation with the ending owed by a job that dies before
    /// finishing: a reported fault, in `why`'s words.
    fn armed(shared: Shared<S>, wake: Wake, why: &'static str) -> Self {
        Self::new(
            shared,
            wake,
            Box::new(move |published: &mut S| published.fail(why)),
        )
    }
}

impl<S> EndOnDrop<S> {
    /// Arm the obligation over a shared cell, with the ending to write if the job
    /// dies before delivering its own.
    fn new(shared: Shared<S>, wake: Wake, fallback: Fallback<S>) -> Self {
        Self {
            shared,
            wake,
            fallback: Some(fallback),
        }
    }

    /// Wake the frame loop, so that a progress event the job published is read
    /// this frame.
    fn wake(&self) {
        (self.wake)();
    }

    /// Publish the real ending and disarm the fallback, so the drop that follows
    /// has nothing to do. Consumes the obligation.
    fn finish(mut self, write: impl FnOnce(&mut S)) {
        self.fallback = None;
        write(&mut lock(&self.shared));
        (self.wake)();
    }
}

impl<S> Drop for EndOnDrop<S> {
    fn drop(&mut self) {
        if let Some(fallback) = self.fallback.take() {
            fallback(&mut lock(&self.shared));
            (self.wake)();
        }
    }
}

/// Run one verb.
///
/// This match is the whole of a job, because core makes every verb reachable the
/// same way. Nothing here prints, retries or decides anything. The decision was
/// made when a person confirmed a plan.
async fn execute<T: Transport>(
    target: &mut FlashAgent<T>,
    source: Option<&mut FlashAgent<T>>,
    task: Task,
    progress: &mut dyn FnMut(Progress),
    cancel: &Cancel,
) -> Outcome {
    match task {
        Task::Info => verbs::info(target).await.map(Report::Info),

        Task::Partitions => verbs::partitions(target).await.map(Report::Partitions),

        Task::ChipVersion => verbs::chip_version(target).await.map(Report::ChipVersion),

        Task::Capability => verbs::capability(target).await.map(Report::Capability),

        Task::StorageMedium => verbs::storage_medium(target)
            .await
            .map(Report::StorageMedium),

        Task::Reset { mode } => verbs::reset(target, mode)
            .await
            .map(|()| Report::Reset { mode }),

        Task::Dump {
            lba,
            sectors,
            mut sink,
            name,
        } => {
            let fill = verbs::dump(target, lba, sectors, &mut *sink, progress, cancel).await?;
            Ok(Report::Dumped {
                // Saturating: the dump has already run, so this multiplication
                // fits -- `dump` refuses a sector count whose byte count does
                // not. Only the report could be made to overflow, and a report
                // is not a thing to panic over.
                bytes: sectors.saturating_mul(u64::from(target.sector_size())),
                name,
                fill,
            })
        }

        Task::Verify {
            lba,
            mut image,
            image_bytes,
        } => {
            let fill =
                verbs::verify(target, lba, &mut *image, image_bytes, progress, cancel).await?;
            Ok(Report::Verified {
                bytes: image_bytes,
                fill,
            })
        }

        Task::PlanWrite {
            aim,
            image_bytes,
            mut image,
            soc,
        } => {
            // The image's first bytes, judged by the function `flash` enforces, so a
            // container is refused before the board is asked anything.
            let head_len = image_bytes.min(verbs::CONTAINER_MAGIC_LEN as u64) as usize;
            let mut head = [0u8; verbs::CONTAINER_MAGIC_LEN];
            image.read_exact(&mut head[..head_len]).await?;
            if let Some(refusal) = verbs::image_refusal(&head[..head_len]) {
                return Err(refusal);
            }
            match aim {
                Aim::Lba(lba) => verbs::plan_write(target, lba, image_bytes, soc)
                    .await
                    .map(Report::Planned),
                Aim::Partition(name) => {
                    verbs::plan_write_partition(target, &name, image_bytes, soc)
                        .await
                        .map(Report::Planned)
                }
            }
        }

        Task::Write {
            confirmed,
            mut image,
        } => {
            let bytes = confirmed.plan().image_bytes;
            verbs::flash(target, confirmed, &mut *image, progress, cancel).await?;
            Ok(Report::Wrote { bytes })
        }

        Task::PlanClone { soc } => {
            let source = source.ok_or_else(no_source)?;
            verbs::plan_clone(source, target, soc)
                .await
                .map(Report::PlannedClone)
        }

        Task::Clone { confirmed } => {
            let source = source.ok_or_else(no_source)?;
            let bytes = confirmed.plan().destination.image_bytes;
            let fill = verbs::clone(source, target, confirmed, progress, cancel).await?;
            Ok(Report::Cloned { bytes, fill })
        }

        Task::PlanRepairTable { soc } => verbs::plan_repair_table(target, soc)
            .await
            .map(Report::PlannedTable),

        Task::PlanRepairParam { soc } => verbs::plan_repair_param(target, soc)
            .await
            .map(Report::PlannedTable),

        // The layout is parsed here, on the job thread, because it needs the
        // device's own sector count -- read a step before the verb reads it again,
        // idempotently, exactly as the CLI parses inside its async block. The parsed
        // `layout` outlives the borrow the verb takes of it.
        Task::PlanAuthorParam {
            source,
            medium,
            soc,
        } => {
            let flash = target.info().await?;
            let flash_sectors = flash_sectors(&flash);
            let layout;
            let param_source = match &source {
                LayoutSource::Native(text) => {
                    layout = Layout::parse_native(text, flash_sectors)?;
                    ParamAuthorSource::Layout(&layout)
                }
                LayoutSource::Mtdparts(text) => {
                    layout = Layout::parse_mtdparts(text, medium.base_lba(), flash_sectors)?;
                    ParamAuthorSource::Layout(&layout)
                }
                LayoutSource::Text(text) => ParamAuthorSource::Text(text),
            };
            verbs::plan_author_param(target, param_source, medium, soc)
                .await
                .map(Report::PlannedTable)
        }

        Task::PlanAuthorGpt { source, soc } => {
            let flash = target.info().await?;
            let flash_sectors = flash_sectors(&flash);
            let layout = match &source {
                LayoutSource::Native(text) => Layout::parse_native(text, flash_sectors)?,
                // A GPT is absolute, so mtdparts offsets count from zero.
                LayoutSource::Mtdparts(text) => Layout::parse_mtdparts(text, 0, flash_sectors)?,
                LayoutSource::Text(_) => {
                    return Err(Error::InvalidRequest(
                        "a GPT is authored from a partition layout, not from parameter text"
                            .to_string(),
                    ));
                }
            };
            verbs::plan_author_gpt(target, &layout, soc)
                .await
                .map(Report::PlannedTable)
        }

        Task::WriteTable { confirmed } => {
            // Read off what the plan wrote before it is consumed, so the report can
            // name the format and say whether it authored or repaired.
            let format = confirmed.plan().format;
            let authored = matches!(confirmed.plan().action, TableAction::Author);
            verbs::write_table(target, confirmed, progress, cancel).await?;
            Ok(Report::TableWritten { format, authored })
        }

        Task::PlanFirmware {
            mut package,
            package_bytes,
            soc,
        } => {
            let package = firmware::read(&mut *package, package_bytes, progress, cancel).await?;
            verbs::plan_firmware(target, &package, soc)
                .await
                .map(Report::PlannedFirmware)
        }

        Task::PlanIdBlock {
            mut loader,
            loader_bytes,
            soc,
        } => {
            let found = firmware::read_loader(&mut *loader, loader_bytes).await?;
            verbs::plan_write_id_block(target, &found.loader, soc)
                .await
                .map(Report::PlannedFirmware)
        }

        Task::WriteFirmware { confirmed, package } => {
            // Read off what the plan writes before it is consumed, for the report.
            let id_block_only = matches!(confirmed.plan().what, FirmwareWrite::IdBlock);
            let runs = confirmed.plan().runs.len();
            let bytes = confirmed.plan().total_bytes();
            match package {
                Some(mut package) => {
                    let package: &mut dyn ImageReader = &mut *package;
                    verbs::write_firmware(target, confirmed, Some(package), progress, cancel)
                        .await?;
                }
                None => verbs::write_firmware(target, confirmed, None, progress, cancel).await?,
            }
            Ok(Report::FirmwareWritten {
                id_block_only,
                runs,
                bytes,
            })
        }
    }
}

/// How many sectors a flash holds, from a geometry that can be invalid.
///
/// **The sector size is guarded against zero**, and the case is reachable.
/// `block::list` reads `queue/logical_block_size` from sysfs, and a file holding
/// `0` yields a zero here. Core guards the same way at its own two call sites. A
/// divide by zero panics, and on the job thread that panic lands the connection
/// in [`Desynchronized`](crate::state::Connection::Desynchronized) over a number
/// nobody has to trust.
fn flash_sectors(flash: &FlashInfo) -> u64 {
    flash.size_bytes / u64::from(flash.sector_size.max(1))
}

/// A clone with no board to copy.
///
/// [`Session::start`](crate::state::Session::start) does not build one, because
/// it takes both agents or neither. Reaching this is therefore a bug rather than
/// a person's mistake. It returns an error rather than unwrapping.
fn no_source() -> Error {
    Error::InvalidRequest("a clone needs a board to copy, and this job was given none".to_string())
}

/// What a bootstrap publishes and the frame loop reads.
///
/// The counterpart of [`Published`] for a job that has no agent to hand back. A
/// Rockchip board in maskrom and an Ingenic board in its boot ROM are not
/// [`FlashAgent`]s. The transport the bootstrap uploads over is gone once the board
/// re-enumerates. The ending is therefore the outcome alone, and there is no board
/// to restore.
///
/// It is generic over `P`, the value a successful upload yields. `P` is `()` for a
/// Rockchip maskrom bootstrap, which re-enumerates and has nothing to report. It
/// is [`CpuInfo`] for an Ingenic bootstrap, which reads the SoC's magic during the
/// upload and hands it back to be shown and later pinned. The default keeps the
/// common Rockchip case spelled [`BootstrapPublished`] with no parameter.
struct BootstrapPublished<P = ()> {
    /// The most recent progress event.
    progress: Option<Progress>,
    /// Set exactly once, when the upload ends, however it ends.
    ended: Option<Result<P, Error>>,
}

// Not `#[derive(Default)]`: that would demand `P: Default`, and an Ingenic
// bootstrap's `P` is `CpuInfo`, which has no meaningful empty value. A published
// slot starts empty regardless of what a success will eventually carry.
impl<P> Default for BootstrapPublished<P> {
    fn default() -> Self {
        Self {
            progress: None,
            ended: None,
        }
    }
}

/// A bootstrap upload in flight, from the frame loop's side.
///
/// Unlike [`Job`], it is not generic over a transport. It holds nothing to hand
/// back, because the board it uploaded to re-enumerates as a different device. It
/// is generic over the success value `P`: `()` for the Rockchip maskrom flow, and
/// [`CpuInfo`] for the Ingenic one. That value is the one thing the two bootstraps
/// differ by once running. The progress, the rate and the cancellation are
/// identical.
pub struct BootstrapJob<P = ()> {
    /// When it began, on the frame clock. The clock is egui's rather than `std`'s,
    /// which panics on `wasm32`.
    pub started_at: f64,
    /// The board this upload is speaking to.
    ///
    /// The board selected when the upload started is not necessarily the board
    /// selected when it ends. A person can point the target slot at another device
    /// while the upload runs. If the target is still *this* board, the harvest
    /// ([`Session::harvest_bootstrap`]) forgets it, and otherwise keeps it.
    /// Forgetting it unconditionally would discard a second board's identity while
    /// its agent is still held. That is the one cross-attach the typed-coordinate
    /// gate cannot catch.
    ///
    /// `None` where the acquisition seam has no handle to keep.
    ///
    /// [`Session::harvest_bootstrap`]: crate::state::Session::harvest_bootstrap
    pub board: Option<Handle>,
    /// What the file claims about which SoC it is for, raw.
    ///
    /// An RKBOOT container names the SoC it was built for in a four-byte field.
    /// With one SoC pinned, the gate refuses only a file that claims another SoC
    /// while `rk3576` is named. For every other upload, the gate's work is to make
    /// the claim visible. The CLI prints the claim before it opens anything, and
    /// this field is where the window shows it. `None` for bare stage files, which
    /// carry no container and so claim nothing. Also `None` for the Ingenic
    /// bootstrap, whose stages are bare blobs either way.
    pub chip: Option<Vec<u8>>,
    cancel: Cancel,
    shared: Shared<BootstrapPublished<P>>,
}

impl<P> Stoppable for BootstrapJob<P> {
    fn cancel_flag(&self) -> &Cancel {
        &self.cancel
    }
}

impl<P> Measured for BootstrapJob<P> {
    fn progress(&self) -> Option<Progress> {
        lock(&self.shared).progress
    }

    fn started_at(&self) -> f64 {
        self.started_at
    }
}

impl<P> BootstrapJob<P> {
    /// Take the upload's ending, if it has ended.
    pub fn take_ended(&self) -> Option<Result<P, Error>> {
        lock(&self.shared).ended.take()
    }
}

/// A maskrom bootstrap made but not yet started.
///
/// The counterpart of [`Work`] for the bootstrap. It holds a bare transport
/// rather than an agent, and a parsed loader. The app spawns it, and a test drives
/// it inline against a scripted transport, as with [`Work`] and for the same
/// reason.
pub struct BootstrapWork<T: Transport> {
    transport: T,
    loader: LoaderImage,
    /// The SoC the upload is aimed at, for the loader-file gate. `None` leaves the
    /// upload an explicit, ungated act.
    soc: Option<Soc>,
    shared: Shared<BootstrapPublished>,
    cancel: Cancel,
    wake: Wake,
}

impl<T: Transport> BootstrapWork<T> {
    /// Make a bootstrap job, and the handle the frame loop watches it through.
    pub(crate) fn new(
        transport: T,
        loader: LoaderImage,
        soc: Option<Soc>,
        board: Option<Handle>,
        started_at: f64,
        wake: Wake,
    ) -> (Self, BootstrapJob) {
        let shared = share(BootstrapPublished::default());
        let cancel = Cancel::new();

        let job = BootstrapJob {
            started_at,
            board,
            // The container's own claim about which SoC it was built for, kept so
            // the window can show it while the upload runs and in the report
            // afterwards. Bare stage files carry no container and claim nothing.
            chip: loader.chip.map(|chip| chip.to_vec()),
            cancel: cancel.clone(),
            shared: shared.clone(),
        };

        let work = Self {
            transport,
            loader,
            soc,
            shared,
            cancel,
            wake,
        };

        (work, job)
    }

    /// Run the upload and publish what it did.
    ///
    /// The transport is dropped when this returns, because the board it spoke to
    /// is gone. On success the board re-enumerated in loader mode, and this job
    /// has nothing to keep.
    pub async fn run(self) {
        let BootstrapWork {
            mut transport,
            loader,
            soc,
            shared,
            cancel,
            wake,
        } = self;

        // The same drop-obligation the block jobs carry: a panic in the upload
        // must not leave the bootstrap wedged, because a wedged bootstrap keeps
        // `wants_rescan` false and freezes the device list.
        let guard = EndOnDrop::armed(
            shared.clone(),
            wake,
            "the bootstrap thread stopped before it finished. Rescan the device list to see the board",
        );

        let outcome = {
            let mut publish = |event| {
                lock(&shared).progress = Some(event);
                guard.wake();
            };
            verbs::download_boot(&mut transport, &loader, soc, &mut publish, &cancel).await
        };

        guard.finish(move |published| {
            published.ended = Some(outcome);
        });
    }
}

/// An Ingenic boot-ROM bootstrap made but not yet started.
///
/// The Ingenic counterpart of [`BootstrapWork`]. It holds a bare transport and a
/// two-stage [`IngenicLoader`] rather than an RKBOOT container. On success it
/// publishes the [`CpuInfo`] the boot ROM reported instead of a bare `()`. The app
/// spawns it, and a test drives it inline against a scripted transport, as with
/// [`Work`] and for the same reason.
pub struct IngenicBootstrapWork<T: Transport> {
    transport: T,
    loader: IngenicLoader,
    shared: Shared<BootstrapPublished<CpuInfo>>,
    cancel: Cancel,
    wake: Wake,
}

impl<T: Transport> IngenicBootstrapWork<T> {
    /// Make an Ingenic bootstrap job, and the handle the frame loop watches it
    /// through.
    pub(crate) fn new(
        transport: T,
        loader: IngenicLoader,
        board: Option<Handle>,
        started_at: f64,
        wake: Wake,
    ) -> (Self, BootstrapJob<CpuInfo>) {
        let shared = share(BootstrapPublished::default());
        let cancel = Cancel::new();

        let job = BootstrapJob {
            started_at,
            board,
            // Ingenic stages are bare blobs with no container around them, so
            // there is no claim to make visible: the SoC magic comes back from
            // the ROM at the end of the run instead, which is the report's.
            chip: None,
            cancel: cancel.clone(),
            shared: shared.clone(),
        };

        let work = Self {
            transport,
            loader,
            shared,
            cancel,
            wake,
        };

        (work, job)
    }

    /// Run the upload and publish what it did, including the SoC magic on success.
    ///
    /// The transport is dropped when this returns, because the board it spoke to
    /// is gone. On success the `VR_PROGRAM_START2` jump removed the boot-ROM
    /// device, and a DFU gadget re-enumerated in its place. The [`CpuInfo`] the
    /// upload returns is carried on the published ending, so it survives the
    /// transport's drop.
    pub async fn run(self) {
        let IngenicBootstrapWork {
            mut transport,
            loader,
            shared,
            cancel,
            wake,
        } = self;

        // The same drop-obligation the block jobs carry: a panic in the upload
        // must not leave the bootstrap wedged, because a wedged bootstrap keeps
        // `wants_rescan` false and freezes the device list.
        let guard = EndOnDrop::new(
            shared.clone(),
            wake,
            Box::new(|published: &mut BootstrapPublished<CpuInfo>| {
                published.ended = Some(Err(Error::Internal(
                    "the bootstrap thread stopped before it finished. Rescan the device list to see the board"
                        .to_string(),
                )));
            }),
        );

        let outcome = {
            let mut publish = |event| {
                lock(&shared).progress = Some(event);
                guard.wake();
            };
            verbs::ingenic_download_boot(&mut transport, &loader, &mut publish, &cancel).await
        };

        guard.finish(move |published| {
            published.ended = Some(outcome);
        });
    }
}

/// Everything a StarFive recovery writes, owned so that a job can hold it.
///
/// [`RecoveryRequest`](recovery::RecoveryRequest) borrows its bytes. A job
/// outlives the frame that started it and runs on a thread of its own, so this
/// type owns them. [`as_request`](Self::as_request) borrows them back into the
/// shape [`recovery::recover`] takes, for the length of the call. A
/// [`PickedImage`](crate::platform::PickedImage) makes the same split: it is held
/// as a file and read when it is needed.
#[derive(Clone)]
pub struct OwnedRecoveryRequest {
    /// The recovery agent (`jh7110-recovery-*.bin`), uploaded into SRAM first.
    pub agent: Vec<u8>,
    /// The SPL, raw or headered. Optional, but the plan has already refused a
    /// recovery that named neither this nor [`uboot`](Self::uboot).
    pub spl: Option<Vec<u8>>,
    /// A U-Boot FIT payload, sent as-is. Optional, on the same terms as `spl`.
    pub uboot: Option<Vec<u8>>,
}

impl OwnedRecoveryRequest {
    /// Borrow the owned bytes into the shape [`recovery::recover`] takes.
    pub fn as_request(&self) -> recovery::RecoveryRequest<'_> {
        recovery::RecoveryRequest {
            agent: &self.agent,
            spl: self.spl.as_deref(),
            uboot: self.uboot.as_deref(),
        }
    }
}

/// Everything a StarFive RAM boot sends, owned so that a job can hold it.
///
/// The RAM boot's counterpart of [`OwnedRecoveryRequest`], with the prompt of the
/// U-Boot being sent, because the job stops that U-Boot at its prompt.
#[derive(Clone)]
pub struct OwnedUartBoot {
    /// The SPL, raw or headered, built to load U-Boot by YMODEM.
    pub spl: Vec<u8>,
    /// The `u-boot.itb` the SPL loads.
    pub uboot: Vec<u8>,
    /// The prompt that U-Boot presents.
    pub prompt: String,
}

impl OwnedUartBoot {
    /// Borrow the owned bytes into the shape [`starfive::uart_boot`] takes.
    pub fn as_request(&self) -> starfive::UartBootRequest<'_> {
        starfive::UartBootRequest {
            spl: &self.spl,
            uboot: &self.uboot,
        }
    }
}

/// What a StarFive job does over an opened serial line.
///
/// Both start the same way, by sending a file to the BootROM, and both report a
/// transfer's progress and the board's text. They differ in what is sent and in
/// whether a person had to agree to it.
pub enum StarfiveTask {
    /// Write the boot flash through the recovery agent, as a person agreed to.
    Recover {
        /// The agent, and whichever of the SPL and U-Boot payload were chosen.
        request: OwnedRecoveryRequest,
        /// The plan, confirmed.
        confirmed: ConfirmedRecovery,
    },
    /// Boot U-Boot in RAM, and stop it at its prompt. It writes nothing, so no
    /// confirmation stands in front of it.
    RamBoot(OwnedUartBoot),
}

impl StarfiveTask {
    /// What to call this on screen while it runs.
    pub fn label(&self) -> &'static str {
        match self {
            StarfiveTask::Recover { .. } => "Recovering over serial",
            StarfiveTask::RamBoot(_) => "Booting U-Boot in RAM over serial",
        }
    }
}

/// What a running StarFive job publishes and the frame loop reads.
///
/// The serial counterpart of [`BootstrapPublished`]. It holds no agent to hand
/// back either, because it drives a serial line the port owns rather than a
/// [`FlashAgent`]. It publishes the current transfer's progress, and the text the
/// board printed between transfers, because the recovery agent prints its writes
/// as they happen and a long flash write that showed nothing would look hung.
#[derive(Default)]
struct RecoveryPublished {
    /// The most recent progress event.
    progress: Option<Progress>,
    /// The end of the transcript so far.
    transcript: Vec<u8>,
    /// How many bytes have come off the line in all. See [`ConsolePublished`].
    seen: u64,
    /// Set exactly once, when the job ends, however it ends.
    ended: Option<Result<Report, Error>>,
}

impl RecoveryPublished {
    /// Take bytes off the line, keeping the tail.
    fn push(&mut self, bytes: &[u8]) {
        self.transcript.extend_from_slice(bytes);
        self.seen = self.seen.saturating_add(bytes.len() as u64);
        if self.transcript.len() > TRANSCRIPT_TAIL {
            let over = self.transcript.len() - TRANSCRIPT_TAIL;
            self.transcript.drain(..over);
        }
    }
}

/// A StarFive job in flight, from the frame loop's side.
///
/// The serial counterpart of [`BootstrapJob`]. Like it, it is not generic over a
/// transport, because it holds nothing to hand back. Its progress is the current
/// XMODEM or YMODEM transfer's, and beside it is the board's transcript. A
/// recovery's ending is a bare outcome, because **the write is not read back**:
/// there is nothing to compare and no agent to restore.
pub struct RecoveryJob {
    /// What to call it on screen.
    pub label: &'static str,
    /// Whether it writes the board's flash. A recovery does, and a RAM boot does
    /// not, and the panel says which.
    pub writes: bool,
    /// When it began, on the frame clock. The clock is egui's rather than `std`'s,
    /// which panics on `wasm32`.
    pub started_at: f64,
    cancel: Cancel,
    shared: Shared<RecoveryPublished>,
}

impl Stoppable for RecoveryJob {
    fn cancel_flag(&self) -> &Cancel {
        &self.cancel
    }
}

impl Measured for RecoveryJob {
    fn progress(&self) -> Option<Progress> {
        lock(&self.shared).progress
    }

    fn started_at(&self) -> f64 {
        self.started_at
    }
}

impl RecoveryJob {
    /// The end of the transcript, rendered for a person to read.
    pub fn transcript(&self) -> String {
        console_codec::text(&lock(&self.shared).transcript)
    }

    /// How many bytes have come off the line in all. See
    /// [`ConsoleJob::bytes_seen`].
    pub fn bytes_seen(&self) -> u64 {
        lock(&self.shared).seen
    }

    /// Take the job's ending, if it has ended.
    pub fn take_ended(&self) -> Option<Result<Report, Error>> {
        lock(&self.shared).ended.take()
    }
}

/// A StarFive job made but not yet started.
///
/// The counterpart of [`BootstrapWork`] for the serial flow. It holds an opened
/// [`Serial`] transport and the task, which carries the owned bytes and, for a
/// recovery, the consent to write them. The app spawns it, and a test drives it
/// inline against a scripted serial, as with [`Work`]. That split makes the wiring
/// testable with neither a board nor a window.
///
/// It is generic over the seam. The app instantiates it with the native
/// [`SerialWire`](crate::platform::SerialWire), and a test with a scripted serial.
pub struct RecoveryWork<S: Serial> {
    serial: S,
    task: StarfiveTask,
    shared: Shared<RecoveryPublished>,
    cancel: Cancel,
    wake: Wake,
}

impl<S: Serial> RecoveryWork<S> {
    /// Make a StarFive job, and the handle the frame loop watches it through.
    pub(crate) fn new(
        serial: S,
        task: StarfiveTask,
        started_at: f64,
        wake: Wake,
    ) -> (Self, RecoveryJob) {
        let shared = share(RecoveryPublished::default());
        let cancel = Cancel::new();

        let job = RecoveryJob {
            label: task.label(),
            writes: matches!(task, StarfiveTask::Recover { .. }),
            started_at,
            cancel: cancel.clone(),
            shared: shared.clone(),
        };

        let work = Self {
            serial,
            task,
            shared,
            cancel,
            wake,
        };

        (work, job)
    }

    /// Run the job and publish what it did.
    ///
    /// The serial transport is dropped when this returns, closing the port. A
    /// RAM-booted U-Boot keeps waiting at its prompt after the host releases the
    /// line, and the console flow opens it again from there.
    pub async fn run(self) {
        let RecoveryWork {
            mut serial,
            task,
            shared,
            cancel,
            wake,
        } = self;

        // The same drop-obligation, so a panic in the job leaves a reported fault
        // rather than a job that never ends.
        let guard = EndOnDrop::armed(
            shared.clone(),
            wake,
            "the serial job's thread stopped before it finished",
        );

        let outcome = {
            let mut progress = |event| {
                lock(&shared).progress = Some(event);
                guard.wake();
            };
            let mut transcript = |bytes: &[u8]| {
                lock(&shared).push(bytes);
                guard.wake();
            };
            match task {
                StarfiveTask::Recover { request, confirmed } => {
                    let request = request.as_request();
                    recovery::recover(
                        &mut serial,
                        &request,
                        confirmed,
                        &mut progress,
                        &mut transcript,
                        &cancel,
                    )
                    .await
                    .map(|()| Report::Recovered)
                }
                StarfiveTask::RamBoot(boot) => starfive::uart_boot(
                    &mut serial,
                    &boot.as_request(),
                    &boot.prompt,
                    &mut progress,
                    &mut transcript,
                    &cancel,
                )
                .await
                .map(|()| Report::UartBooted),
            }
        };

        guard.finish(move |published| {
            published.ended = Some(outcome);
        });
    }
}

/// How much of a console transcript a running job keeps.
///
/// A console session is unbounded, because a watch on a booting board lasts as
/// long as the board prints. A window shows the end of it. Once the transcript
/// passes this length, bytes are dropped from the front. That bounds memory and
/// the amount of text the frame loop re-renders. The codec's own transcript and
/// the tail an error carries are bounded separately, for their own reasons.
const TRANSCRIPT_TAIL: usize = 16 * 1024;

/// What a console job does over an opened serial line.
///
/// The five tasks are one enum rather than five jobs, because they differ only in
/// what happens once the line is open. Every U-Boot form interrupts the autoboot
/// first, and the watch needs no prompt at all.
pub enum ConsoleTask {
    /// Watch for the text that says a board worked, and the text that says it did
    /// not. It needs no prompt and no echo handling, and types nothing.
    Watch {
        /// Patterns that mean it worked.
        expect: Vec<Vec<u8>>,
        /// Patterns that mean the far end reported a failure.
        fail: Vec<Vec<u8>>,
    },
    /// Start a U-Boot gadget on a block device, handing the board's flash to the
    /// USB side.
    Gadget(Gadget, GadgetDevice),
    /// Type one line at the prompt.
    ///
    /// **This task is ungated**, and the screen that offers it says so. U-Boot runs
    /// whatever is typed.
    Command(String),
    /// Ask the board what it currently boots from, and what an override would set.
    ///
    /// This is the dry run, and it produces the screen a person agrees to.
    PlanBoot(String),
    /// Set the boot order and boot, for this boot only.
    Boot(ConfirmedBoot),
}

impl ConsoleTask {
    /// What to call this on screen while it runs.
    pub fn label(&self) -> &'static str {
        match self {
            ConsoleTask::Watch { .. } => "Watching the console",
            ConsoleTask::Gadget(Gadget::Rockusb, _) => "Starting the rockusb gadget",
            ConsoleTask::Gadget(Gadget::Ums, _) => "Starting the mass-storage gadget",
            ConsoleTask::Command(_) => "Running a command at the prompt",
            ConsoleTask::PlanBoot(_) => "Asking what the board boots from",
            ConsoleTask::Boot(_) => "Setting the boot order and booting",
        }
    }
}

/// What a console job produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsoleReport {
    /// A watch saw one of its patterns, and the report records which list the
    /// pattern came from. A failure pattern is reported rather than raised as an
    /// error, because only the person watching decides what a board printing
    /// `FAIL` means.
    Watched(Watched),
    /// A gadget was started, and the prompt did not come back. A running gadget
    /// owns the console, so the prompt's absence is the sign of success.
    GadgetStarted {
        /// Which gadget, because it decides where the board's flash appears: in the
        /// device list for rockusb, and among the disks for mass storage.
        gadget: Gadget,
        /// The exact line that was typed.
        command: String,
    },
    /// A command ran, and this is what it printed.
    Answered(String),
    /// The dry run's answer: what the board currently boots from, and what an
    /// override would set. This becomes the screen a person agrees to rather than
    /// a report.
    BootPlanned(BootPlan),
    /// The boot order was set and the board told to boot, for this boot only.
    /// Nothing was saved.
    Booted(String),
}

/// What a running console session publishes and the frame loop reads.
///
/// A console's progress is its transcript. Byte counts do not describe a console
/// session, and a session that shows nothing while it waits is indistinguishable
/// from a hang. This slot therefore holds the bytes, bounded to a tail.
#[derive(Default)]
struct ConsolePublished {
    /// The end of the transcript so far.
    transcript: Vec<u8>,
    /// How many bytes have come off the line in all, which the tail's own length
    /// cannot say once the tail is full.
    ///
    /// A front-end uses it to tell whether the text it rendered last frame is still
    /// current. Rendering a transcript runs the codec over up to
    /// [`TRANSCRIPT_TAIL`] bytes and lays out every glyph. The text changes only
    /// when the board prints, so doing that at frame rate is wasted work. The
    /// tail's length stops changing once the tail is full, so it cannot serve as
    /// the key. This count only grows.
    seen: u64,
    /// Set exactly once, when the session ends, however it ends.
    ended: Option<Result<ConsoleReport, Error>>,
}

impl ConsolePublished {
    /// Take bytes off the line, keeping the tail.
    fn push(&mut self, bytes: &[u8]) {
        self.transcript.extend_from_slice(bytes);
        self.seen = self.seen.saturating_add(bytes.len() as u64);
        if self.transcript.len() > TRANSCRIPT_TAIL {
            let over = self.transcript.len() - TRANSCRIPT_TAIL;
            self.transcript.drain(..over);
        }
    }
}

/// A console session in flight, from the frame loop's side.
///
/// The serial counterpart of [`BootstrapJob`] for the console flow. It holds no
/// agent to hand back, because a console is a serial line rather than a
/// [`FlashAgent`]. It publishes text rather than progress, so it has a transcript
/// where the other jobs have a fraction.
pub struct ConsoleJob {
    /// What to call it on screen.
    pub label: &'static str,
    /// When it began, on the frame clock.
    pub started_at: f64,
    cancel: Cancel,
    shared: Shared<ConsolePublished>,
}

impl ConsoleJob {
    /// The end of the transcript, rendered for a person to read.
    ///
    /// The codec renders away carriage returns and an autoboot countdown's
    /// backspaces. The text a window draws is therefore what the board said, rather
    /// than control characters for a terminal the window does not have.
    pub fn transcript(&self) -> String {
        console_codec::text(&lock(&self.shared).transcript)
    }

    /// Whether anything has come back yet.
    pub fn is_quiet(&self) -> bool {
        lock(&self.shared).transcript.is_empty()
    }

    /// How many bytes have come off the line in all.
    ///
    /// It answers whether anything has changed, cheaply and monotonically. A
    /// front-end can therefore keep a rendered transcript between frames, instead
    /// of decoding and laying out the tail sixty times a second. The tail's own
    /// length stops changing once the tail is full, so it cannot serve as the key.
    /// This count only grows.
    pub fn bytes_seen(&self) -> u64 {
        lock(&self.shared).seen
    }

    /// Take the session's ending, if it has ended.
    pub fn take_ended(&self) -> Option<Result<ConsoleReport, Error>> {
        lock(&self.shared).ended.take()
    }
}

/// [`Stoppable`] and not [`Measured`].
///
/// A console session moves no counted bytes. It publishes text, so a fraction and
/// a rate would be numbers with nothing behind them. Stopping it uses the same
/// flag as every other job, and it stops only the reading. Anything already typed
/// has been sent, and a board told to boot is booting.
impl Stoppable for ConsoleJob {
    fn cancel_flag(&self) -> &Cancel {
        &self.cancel
    }
}

/// A console session made but not yet started.
///
/// The counterpart of [`RecoveryWork`] for the console flow, generic over the same
/// seam for the same reason. The app instantiates it with the native
/// [`SerialWire`](crate::platform::SerialWire), and a test with a scripted serial.
/// The whole flow is therefore pinned with neither a board nor a window.
pub struct ConsoleWork<S: Serial> {
    serial: S,
    prompt: String,
    reads: u32,
    task: ConsoleTask,
    shared: Shared<ConsolePublished>,
    cancel: Cancel,
    wake: Wake,
}

impl<S: Serial> ConsoleWork<S> {
    /// Make a console job, and the handle the frame loop watches it through.
    pub(crate) fn new(
        serial: S,
        prompt: String,
        reads: u32,
        task: ConsoleTask,
        started_at: f64,
        wake: Wake,
    ) -> (Self, ConsoleJob) {
        let shared = share(ConsolePublished::default());
        let cancel = Cancel::new();

        let job = ConsoleJob {
            label: task.label(),
            started_at,
            cancel: cancel.clone(),
            shared: shared.clone(),
        };

        let work = Self {
            serial,
            prompt,
            reads,
            task,
            shared,
            cancel,
            wake,
        };

        (work, job)
    }

    /// Run the session and publish what it did.
    ///
    /// The serial transport is dropped when this returns, closing the port. A
    /// gadget started here keeps running on the board after the host releases the
    /// line. The next step, finding the board on the USB bus, depends on that.
    pub async fn run(self) {
        let ConsoleWork {
            serial,
            prompt,
            reads,
            task,
            shared,
            cancel,
            wake,
        } = self;

        let guard = EndOnDrop::armed(
            shared.clone(),
            wake,
            "the console thread stopped before it finished",
        );

        let outcome = {
            let mut publish = |bytes: &[u8]| {
                lock(&shared).push(bytes);
                guard.wake();
            };
            run_console(serial, &prompt, reads, task, &mut publish, &cancel).await
        };

        guard.finish(move |published| {
            published.ended = Some(outcome);
        });
    }
}

/// The console session itself, with the transcript sink already wired up.
///
/// It is a free function rather than a method. The borrow of the shared
/// transcript and the borrow of the serial line then need not live in the same
/// struct.
async fn run_console<S: Serial>(
    serial: S,
    prompt: &str,
    reads: u32,
    task: ConsoleTask,
    sink: ConsoleSink<'_>,
    cancel: &Cancel,
) -> Result<ConsoleReport, Error> {
    // A watch needs no prompt and types nothing, so it never takes the line
    // through the U-Boot driver at all.
    let task = match task {
        ConsoleTask::Watch { expect, fail } => {
            let mut serial = serial;
            let watched = console::watch(&mut serial, &expect, &fail, reads, sink, cancel).await?;
            return Ok(ConsoleReport::Watched(watched));
        }
        other => other,
    };

    let mut uboot = UBoot::new(serial).with_prompt(prompt).with_reads(reads);
    uboot.interrupt_autoboot(sink, cancel).await?;

    match task {
        // Handled above; the compiler does not know that.
        ConsoleTask::Watch { .. } => unreachable!("a watch never reaches the U-Boot driver"),

        ConsoleTask::Gadget(gadget, device) => Ok(ConsoleReport::GadgetStarted {
            gadget,
            command: uboot.start_gadget(gadget, &device, sink, cancel).await?,
        }),

        ConsoleTask::Command(line) => {
            let output = uboot.run(&line, sink, cancel).await?;
            Ok(ConsoleReport::Answered(console_codec::text(&output)))
        }

        ConsoleTask::PlanBoot(targets) => Ok(ConsoleReport::BootPlanned(
            uboot.plan_boot_override(&targets, sink, cancel).await?,
        )),

        ConsoleTask::Boot(confirmed) => {
            let targets = confirmed.plan().targets.clone();
            uboot.boot_override(confirmed, sink, cancel).await?;
            // What the board says as it comes up. Best-effort: the override has
            // already gone out, so a line that goes quiet here is the board
            // booting, not a failure of what was asked for.
            let _ = uboot.drain(BOOT_WATCH_READS, sink, cancel).await;
            Ok(ConsoleReport::Booted(targets))
        }
    }
}

/// How many reads to spend watching a board start up after `boot`.
///
/// The budget is long enough for the first banners to reach the transcript, and
/// short enough that the job ends rather than becoming a terminal.
const BOOT_WATCH_READS: u32 = 6;
