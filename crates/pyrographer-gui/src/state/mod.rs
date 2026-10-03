//! Everything the GUI knows, and none of how it looks.
//!
//! This module holds no `egui` type. It is the part of the front-end where the
//! reasoning lives, and therefore the part worth testing:
//!
//! - The connection state machine
//! - The job lifecycle
//! - The plan-to-confirmation flow
//! - The comparison that decides whether a person agreed to a write
//!
//! It is tested against core's scripted transport. The tests pin completed and
//! canceled jobs, a desynchronized agent and a mistyped confirmation, with
//! neither a board nor a window.
//!
//! [`ui`](crate::ui) renders what is here. It is thin, so the code left untested
//! holds none of the reasoning.

mod job;

pub use job::{
    Aim, BootstrapJob, BootstrapWork, ConsoleJob, ConsoleReport, ConsoleTask, ConsoleWork, Ended,
    IngenicBootstrapWork, Job, LayoutSource, Measured, Outcome, OwnedRecoveryRequest, RecoveryJob,
    RecoveryWork, Report, Stoppable, Task, Work,
};

pub(crate) use crate::platform::lock;

use pyrographer_core::Error;
use pyrographer_core::agent::{Caps, FlashAgent, FlashInfo};
use pyrographer_core::block::BlockDevice;
use pyrographer_core::bootstrap::ingenic::IngenicLoader;
use pyrographer_core::codec::ingenic_boot::CpuInfo;
use pyrographer_core::codec::rkboot::LoaderImage;
use pyrographer_core::discovery::DeviceInfo;
use pyrographer_core::partition::PartitionTable;
use pyrographer_core::recovery::{self, ConfirmedRecovery, RecoveryPlan, RecoveryTarget};
use pyrographer_core::soc::Soc;
use pyrographer_core::transport::{Serial, Transport};
use pyrographer_core::uboot::{BootPlan, ConfirmedBoot};
use pyrographer_core::verbs::{
    self, ClonePlan, ConfirmedClone, ConfirmedSegmentedWrite, ConfirmedWrite, SegmentedPlan,
    WritePlan,
};

use crate::platform::{self, Handle, PickedBlob, PickedImage, Wake};

/// How often the bus is rescanned when nothing is running, in seconds.
///
/// Often enough that a board appears promptly when it is plugged in, and rarely
/// enough that the scan does not run constantly.
const RESCAN_SECONDS: f64 = 1.0;

/// Where a connection to one board stands.
///
/// **Busy is a state, not an absence.** A job borrows the agent and hands it
/// back when it ends, because an agent must outlive the errors it reports. A
/// desynchronized agent refuses every command after the error, and that refusal
/// means something only to a consumer that still holds the agent. A CLI never
/// has to be that consumer, because an error ends the process. A GUI always is.
pub enum Connection<T: Transport> {
    /// Nothing is open.
    Disconnected,
    /// A board is open and answering.
    Idle(FlashAgent<T>),
    /// A job holds the agent. It comes back when the job ends.
    Busy,
    /// The host lost track of where the device is in the conversation, and this
    /// connection is finished. The board must be reopened, as the hint on
    /// [`Error::Desynchronized`] says.
    Desynchronized,
}

impl<T: Transport> Connection<T> {
    /// Whether a job can start on this board at this moment.
    pub fn is_idle(&self) -> bool {
        matches!(self, Connection::Idle(_))
    }

    /// Whether a job holds the agent.
    pub fn is_busy(&self) -> bool {
        matches!(self, Connection::Busy)
    }

    /// Whether this connection is finished and has to be reopened.
    pub fn is_desynchronized(&self) -> bool {
        matches!(self, Connection::Desynchronized)
    }

    /// Whether nothing is open on this connection.
    pub fn is_disconnected(&self) -> bool {
        matches!(self, Connection::Disconnected)
    }

    /// The agent, while nothing is using it.
    pub fn agent(&self) -> Option<&FlashAgent<T>> {
        match self {
            Connection::Idle(agent) => Some(agent),
            _ => None,
        }
    }

    /// Open a board.
    pub fn opened(&mut self, agent: FlashAgent<T>) {
        *self = Connection::Idle(agent);
    }

    /// Close a board, dropping the agent and with it the handle on the device.
    pub fn close(&mut self) {
        *self = Connection::Disconnected;
    }

    /// Lend the agent to a job, and go [`Busy`](Connection::Busy).
    fn take(&mut self) -> Option<FlashAgent<T>> {
        match std::mem::replace(self, Connection::Busy) {
            Connection::Idle(agent) => Some(agent),
            not_idle => {
                *self = not_idle;
                None
            }
        }
    }

    /// Take the agent back from a job that has ended.
    ///
    /// **The agent decides where this lands, and the outcome does not.** An
    /// error says what went wrong, but not which board it went wrong on, and a
    /// clone touches two. The agent that came back records whether it is
    /// finished. A board that left the bus poisons its agent as it goes, so it
    /// also lands in [`Desynchronized`](Connection::Desynchronized). The device
    /// list notices that the board is gone at the next rescan.
    fn restore(&mut self, agent: FlashAgent<T>) {
        *self = if agent.is_desynchronized() {
            Connection::Desynchronized
        } else {
            Connection::Idle(agent)
        };
    }
}

/// What the device answered when it was asked for its partition table.
///
/// The type has three states rather than two. Core already draws one
/// distinction: [`verbs::partitions`] answers `Ok(None)` for a board that has no
/// table, which is a finding rather than a failure. A screen also has to tell an
/// unasked question from an empty table, because the two look identical on it.
///
/// A table that is present and fails its checksum is in none of these states. It
/// is an [`Error::CorruptTable`], and the screen shows it as damage.
#[derive(Default)]
pub enum Table {
    /// Nobody has asked.
    #[default]
    Unknown,
    /// The board has none, which is a valid state for a board.
    Absent,
    /// The board has one, and this is it.
    Read(PartitionTable),
}

impl Table {
    /// The table, if there is one.
    pub fn get(&self) -> Option<&PartitionTable> {
        match self {
            Table::Read(table) => Some(table),
            _ => None,
        }
    }
}

/// What a slot is pointed at: a board on the bus, or a disk the host owns.
///
/// It names the same two things the CLI's own `Chosen` names, for the same reason
/// and with the same weight on the difference. A board in a bootstrap mode
/// belongs to pyrographer alone, and nothing else on the host addresses it. A
/// disk is shared with the operating system, which mounts filesystems on it and
/// caches its sectors. A disk is the only target on which a mistake destroys the
/// machine this runs on rather than the board.
///
/// The two share the agent. [`FlashAgent::Block`] carries no transport, so both
/// open into the same `FlashAgent<T>`, and the whole verb surface is drawn once
/// for both. A disk is therefore not a flow of its own, as StarFive recovery is.
/// A recovery target has no LBAs, no table and no geometry, so it has none of the
/// uniform surface to disable. A disk has all of it.
#[derive(Debug, Clone)]
pub enum Chosen {
    /// A board on the USB bus, classified from its descriptors.
    Usb(DeviceInfo),
    /// A block device the host operating system also owns.
    Block(BlockDevice),
}

impl Chosen {
    /// The board this names, where it names one.
    pub fn usb(&self) -> Option<&DeviceInfo> {
        match self {
            Chosen::Usb(device) => Some(device),
            Chosen::Block(_) => None,
        }
    }

    /// The disk this names, where it names one.
    pub fn disk(&self) -> Option<&BlockDevice> {
        match self {
            Chosen::Block(device) => Some(device),
            Chosen::Usb(_) => None,
        }
    }
}

/// One board: the device, the connection to it, and what it has told us.
pub struct Board<T: Transport> {
    /// What it takes to open this board.
    ///
    /// A place on the bus natively, and the browser's own device object in a tab.
    /// [`Handle`] is the acquisition seam, and the one thing here that differs
    /// between the two builds.
    pub handle: Option<Handle>,
    /// What this slot is pointed at, for the screen. See [`Chosen`].
    ///
    /// For a board, that is its vendor, product and mode, and natively its place
    /// on the bus. For a disk, it is the node path with its capacity, bus and
    /// mounts.
    pub device: Option<Chosen>,
    /// Where the connection stands.
    pub connection: Connection<T>,
    /// What the backend can do, read once when the board was opened.
    ///
    /// It is cached because a job takes the agent away with it. A button that
    /// grays itself out only when nothing is running flickers.
    pub caps: Option<Caps>,
    /// Why an erase is refused, cached at open for the same reason as
    /// [`caps`](Self::caps). The grayed `erase` button gives its reason in the
    /// backend's own words. A reason that vanished whenever a job held the agent
    /// would flicker, as the button would without its cache.
    pub erase_reason: Option<&'static str>,
    /// Why a write that addresses a device-wide LBA space is refused, cached at
    /// open for the same reason as [`caps`](Self::caps).
    ///
    /// It is [`verbs::raw_lba_refusal`]'s sentence. The partition table section
    /// grays its tools on it and draws it, and every table plan refuses on the same
    /// answer.
    pub raw_lba_reason: Option<&'static str>,
    /// The flash geometry, once it has been read.
    pub flash: Option<FlashInfo>,
    /// The partition table, once it has been asked for.
    pub table: Table,
    /// What the loader last answered when asked which SoC it is on, raw.
    pub chip_version: Option<Vec<u8>>,
}

impl<T: Transport> Default for Board<T> {
    fn default() -> Self {
        Self {
            handle: None,
            device: None,
            connection: Connection::Disconnected,
            caps: None,
            erase_reason: None,
            raw_lba_reason: None,
            flash: None,
            table: Table::Unknown,
            chip_version: None,
        }
    }
}

impl<T: Transport> Board<T> {
    /// Point this board at a device, forgetting whatever the last one said.
    ///
    /// Everything cached here was one device's answer. Carrying `uboot at LBA
    /// 16384` over from the previously selected board would aim the next write by a
    /// table that belongs to a different board.
    pub fn select(&mut self, handle: Handle, device: DeviceInfo) {
        *self = Board {
            handle: Some(handle),
            device: Some(Chosen::Usb(device)),
            ..Board::default()
        };
    }

    /// Point this slot at a disk, forgetting whatever the last device said.
    ///
    /// **The slot gets no [`Handle`], by design.** A handle is what it takes to
    /// reopen a board found by scanning a bus, and a disk is not found that way. A
    /// [`BlockDevice`] is both what the row shows and what
    /// [`platform::blocks::open`] takes, so the device chosen and the device
    /// reopened are one value. A second identity carried alongside it would be a
    /// weaker copy of what the exclusive open already guarantees.
    pub fn select_disk(&mut self, disk: BlockDevice) {
        *self = Board {
            device: Some(Chosen::Block(disk)),
            ..Board::default()
        };
    }

    /// Open the board, and read off what the backend can do.
    ///
    /// The caps and both refusal reasons are read here, while the agent is in
    /// hand. A job that borrows the agent later then does not take a button's
    /// explanation with it.
    pub fn opened(&mut self, agent: FlashAgent<T>) {
        self.caps = Some(agent.caps());
        self.erase_reason = verbs::erase_refusal(&agent);
        self.raw_lba_reason = verbs::raw_lba_refusal(&agent);
        self.connection.opened(agent);
    }

    /// Close the board, forgetting what it said.
    ///
    /// The board is still chosen, as the row a person clicked, so what it takes to
    /// reopen it stays. What the board reported is forgotten.
    pub fn close(&mut self) {
        *self = Board {
            handle: self.handle.clone(),
            device: self.device.clone(),
            ..Board::default()
        };
    }

    /// Why a write to this board would be refused before a plan exists, or `None`
    /// if it would not.
    ///
    /// This asks [`verbs::write_refusal`], the half of the wrong-loader gate that
    /// needs only an agent and a name. It refuses when no SoC is named at all.
    /// The comparison itself needs a plan, and [`Pending::refused`] carries that
    /// verdict once a plan exists. The front-end asks both before it collects a
    /// confirmation, so no person confirms a write that the gate will refuse.
    pub fn write_refusal(&self, soc: Option<Soc>) -> Option<String> {
        self.connection
            .agent()
            .and_then(|agent| verbs::write_refusal(agent, soc))
    }

    /// The sector size the backend addresses in.
    ///
    /// It is the backend's sector size, not the board's, so it is known without
    /// asking the device. A caller can therefore check an image against a
    /// partition before it runs a single command. `None` when nothing is open.
    /// There is no sensible default, and a wrong guess of 512 checks an image
    /// against the wrong length.
    pub fn sector_size(&self) -> Option<u32> {
        self.connection.agent().map(FlashAgent::sector_size)
    }
}

/// How this build asks a person to confirm a write.
///
/// The rule is that a confirmation must be an act the UI cannot perform for a
/// person. Reflex must not be able to perform it without the person's attention.
///
/// The two builds apply the rule differently, because the two platforms offer
/// different means. This enum records which build is running. It is data rather
/// than a `cfg`, so both forms are testable in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmBy {
    /// Type the destination's coordinate, in the native window.
    ///
    /// A board's coordinate is its `bus:address`, such as `003:12` on Linux, and a
    /// disk's is its node path. Each is short and already the vocabulary: the
    /// device list prints it, and the CLI aims with it. It varies per device, so
    /// reflex cannot learn it. A clone with its source and destination swapped
    /// names a different board, asks for a different string, and is refused.
    /// That case is the main reason for typing. Careful reading cannot catch a
    /// swapped source and destination, because both boards are real and both
    /// halves of the plan are true.
    Coordinate,

    /// Re-pick the destination from the browser's chooser, in the web flasher.
    ///
    /// This is not because picking is stronger than typing. On the web there is
    /// nothing short and stable to transcribe. `navigator.usb.requestDevice()`
    /// also opens a dialog the page did not draw, and the page cannot script it or
    /// pre-select a row in it. The page cannot open it without a real user gesture
    /// either. Re-picking is strong on the web for the reason it would be worthless
    /// natively.
    ///
    /// Natively, the device list is drawn by the window itself, with the
    /// destination already highlighted under the mouse that chose it. A second
    /// click next to the first is not a confirmation.
    Repick,
}

/// The act that turns a plan into a write, and whether it has been performed.
pub enum Confirmation {
    /// The destination's coordinate on the bus, and what has been typed so far.
    Typed {
        /// What the destination is called, such as `003:12` or `/dev/sdb`.
        expected: String,
        /// What a person has typed.
        typed: String,
    },
    /// Whether the destination has been picked again from the browser's chooser.
    Repicked {
        /// Set when the chooser gave back the same board.
        done: bool,
    },
}

impl Confirmation {
    /// The act this build asks for, against the device that would be overwritten.
    ///
    /// Natively, the string to type is a board's `bus:address` or a disk's node
    /// path. Both are places, both are printed in the list a person is looking at,
    /// and both differ between two devices that are otherwise identical. See
    /// [`coordinate`].
    pub fn asked_of(how: ConfirmBy, destination: &Chosen) -> Self {
        match how {
            ConfirmBy::Coordinate => Confirmation::Typed {
                expected: coordinate(destination),
                typed: String::new(),
            },
            ConfirmBy::Repick => Confirmation::Repicked { done: false },
        }
    }

    /// The act this build asks for before recovering the board on `port`.
    ///
    /// The serial counterpart of [`asked_of`](Self::asked_of). A StarFive recovery
    /// has no bus coordinate, because a serial port is a path a person named
    /// rather than a device on a bus. The string typed is therefore the **port
    /// path itself**, which names the destination of the write. When several
    /// USB-serial adapters are present, the path also says which adapter it is.
    ///
    /// On the web the act is a re-pick of the port, as it is at the USB gate. The
    /// reason is the same: a browser hands back a port object, not a place.
    /// [`ConfirmBy::Repick`] yields the same [`Repicked`](Confirmation::Repicked)
    /// here that it yields for a write.
    ///
    /// The port is trimmed, so a trailing space is not a different port. An empty
    /// port yields an empty expectation, which [`is_satisfied`](Self::is_satisfied)
    /// never accepts. A recovery is not planned with an empty port in any case.
    pub fn for_port(how: ConfirmBy, port: &str) -> Self {
        match how {
            ConfirmBy::Coordinate => Confirmation::Typed {
                expected: port.trim().to_string(),
                typed: String::new(),
            },
            ConfirmBy::Repick => Confirmation::Repicked { done: false },
        }
    }

    /// Whether it has been performed.
    ///
    /// The typed text is trimmed, because a trailing space is not a different
    /// board. Nothing else is forgiven. A coordinate that is one character off
    /// names either no board or the wrong one.
    ///
    /// **An empty expectation is never satisfied**, whatever is typed against it.
    /// No board in a browser has a coordinate to give. Without this check, an
    /// empty text field would confirm a write to such a board. The web build asks
    /// for [`Repicked`](Confirmation::Repicked) for that reason. If a build asks
    /// for the wrong act, this check makes the result a refusal rather than an
    /// open gate.
    pub fn is_satisfied(&self) -> bool {
        match self {
            Confirmation::Typed { expected, typed } => {
                !expected.is_empty() && typed.trim() == expected
            }
            Confirmation::Repicked { done } => *done,
        }
    }
}

/// How a board is named, and the string a confirmation is compared against.
///
/// For a board, it is the bus and the address the operating system gave it. The
/// device list prints them, and the CLI's `--device` takes them, in exactly this
/// form.
/// This string exists to tell two identical boards apart. It is not the product
/// ID, because two identical boards share one.
///
/// It is not a friendly name either, because none exists here. `DeviceInfo`
/// carries no serial and no product string, and reading more descriptors would
/// not supply one. A Rockchip board in maskrom reports a generic string or none,
/// and **two identical boards report the identical string**. A name would be the
/// same word for both boards, and would confirm nothing in the case this string
/// exists to catch.
///
/// **It is empty in a browser**, which has no bus and no address to give. WebUSB
/// hands back a device object, not a place. The gap is not filled with an invented
/// value. The web build therefore confirms a write by re-picking the destination
/// rather than by transcribing it, and [`Confirmation::is_satisfied`] refuses an
/// empty expectation outright.
///
/// **A disk gives its node path**, such as `/dev/sdb`. A node path is a place in
/// the same sense a bus coordinate is. The CLI's `--device` takes it, and the
/// device list prints it. It is also the serial flow's answer, because a port is
/// likewise a path a person named. It is not the model string, for the reason a
/// board's coordinate is not its product ID. Two identical card readers report
/// the identical model.
pub fn coordinate(device: &Chosen) -> String {
    match device {
        Chosen::Usb(device) => usb_coordinate(device),
        Chosen::Block(disk) => disk.node.clone(),
    }
}

/// The board half of [`coordinate`], for the device list, which draws its rows
/// from a bus scan rather than from a chosen slot.
pub fn usb_coordinate(device: &DeviceInfo) -> String {
    if device.bus_id.is_empty() {
        return String::new();
    }
    format!("{}:{}", device.bus_id, device.device_address)
}

/// A plan waiting for a person to agree to it.
///
/// **The dry run is this plan and nothing after it.** It is the write's own call,
/// stopped one step earlier, rather than a separate path.
pub struct Pending {
    /// What the board said a write would do. This is the screen a person reads.
    pub plan: Plan,
    /// What they have to do to say yes.
    pub confirmation: Confirmation,
    /// Why the write would be refused anyway, or `None` if it would not.
    ///
    /// Core is asked before any person is, and it gives the same answer the write
    /// itself will give. Collecting a confirmation for a write the gate will refuse
    /// would teach a person to treat the gate as theater.
    pub refused: Option<String>,
}

/// A write, a clone, or a table repair that a board has described and nobody has
/// agreed to yet.
#[derive(Clone)]
pub enum Plan {
    /// An image onto this board.
    Write(WritePlan),
    /// Another board onto this one.
    Clone(ClonePlan),
    /// A table repair or authoring: a damaged GPT or parameter copy rewritten from
    /// an intact one, or a fresh table written from a layout.
    Table(SegmentedPlan),
}

impl Plan {
    /// Whether this planned write would be refused, with `agent` in hand. `None`
    /// means it would go ahead.
    ///
    /// This is the whole wrong-loader gate, from the same call the write will
    /// make. The screen a person reads and the refusal the write enforces
    /// therefore cannot drift apart.
    fn refusal<T: Transport>(&self, agent: &FlashAgent<T>) -> Option<Error> {
        match self {
            Plan::Write(plan) => verbs::plan_refusal(agent, plan),
            Plan::Clone(plan) => verbs::plan_refusal(agent, &plan.destination),
            Plan::Table(plan) => verbs::segmented_refusal(agent, plan),
        }
    }
}

/// A plan a person agreed to, and the only thing that reaches a write.
pub enum Confirmed {
    /// An image onto a board.
    Write(ConfirmedWrite),
    /// A board onto another board.
    Clone(ConfirmedClone),
    /// A table repair or authoring.
    Table(ConfirmedSegmentedWrite),
}

impl Pending {
    /// Whether the button that says yes is live.
    pub fn can_confirm(&self) -> bool {
        self.refused.is_none() && self.confirmation.is_satisfied()
    }

    /// Mint the confirmation.
    ///
    /// It is private, and reachable only through [`Session::confirm`], which
    /// checks that a person performed the act. Consuming the plan keeps one
    /// confirmation from buying two writes.
    fn into_confirmed(self) -> Confirmed {
        match self.plan {
            Plan::Write(plan) => Confirmed::Write(plan.confirm()),
            Plan::Clone(plan) => Confirmed::Clone(plan.confirm()),
            Plan::Table(plan) => Confirmed::Table(plan.confirm()),
        }
    }
}

/// A StarFive recovery waiting for a person to agree to it.
///
/// The serial counterpart of [`Pending`], simpler in one way. A recovery has no
/// wrong-loader gate to be refused by, because the ROM in recovery only receives.
/// There is therefore no `refused` field. It carries the **port**, which a
/// [`Pending`] does not, because a serial recovery has no [`DeviceInfo`]. The port
/// is where the write goes and, in the native window, the string typed to confirm
/// it.
pub struct RecoveryPending {
    /// What the recovery would do, from [`recovery::plan_recover`]. This is the
    /// screen a person reads. It carries the one fact a person confirming a
    /// StarFive recovery needs that no other write has: the write is not read back.
    pub plan: RecoveryPlan,
    /// The serial port the recovery goes to, named by the person.
    pub port: String,
    /// What they have to do to say yes: type the port path, in the native window.
    pub confirmation: Confirmation,
}

impl RecoveryPending {
    /// Whether the button that says yes is live.
    ///
    /// It checks the confirmation alone, because a recovery has no refusal gate.
    /// [`Pending::can_confirm`] also asks whether the write would be refused.
    pub fn can_confirm(&self) -> bool {
        self.confirmation.is_satisfied()
    }
}

/// The StarFive recovery flow's state: the files it would write, a plan waiting to
/// be agreed to, and the recovery in flight.
///
/// It is one struct because it is one flow, the serial one, beside the USB flow's
/// state on [`Session`]. The files are held as [`Session::image`] is: picked once,
/// and read when the recovery runs. A recovery blob is small, so it is held whole
/// rather than behind a handle.
#[derive(Default)]
pub struct Recovery {
    /// The recovery agent (`jh7110-recovery-*.bin`), uploaded first. Required.
    pub agent: Option<PickedBlob>,
    /// A raw `u-boot-spl.bin`. Optional, but at least one of this and
    /// [`uboot`](Self::uboot) is needed for a recovery to write anything.
    pub spl: Option<PickedBlob>,
    /// A U-Boot FIT payload. Optional, on the same terms as `spl`.
    pub uboot: Option<PickedBlob>,
    /// A recovery plan waiting to be agreed to.
    pub pending: Option<RecoveryPending>,
    /// The recovery in flight, if there is one.
    pub job: Option<RecoveryJob>,
}

impl Recovery {
    /// Whether the serial flow has anything in it: a file chosen, a plan waiting,
    /// or a job running.
    ///
    /// The GUI reveals the serial form only when a person asks for it. A JH7110 in
    /// UART recovery cannot be discovered the way a USB board is. This check keeps
    /// the form from collapsing back to its entry point once it is in use.
    pub fn is_active(&self) -> bool {
        self.agent.is_some()
            || self.spl.is_some()
            || self.uboot.is_some()
            || self.pending.is_some()
            || self.job.is_some()
    }
}

/// The serial line a console session runs on, pinned when the session starts.
///
/// The fields are held together rather than read from a form when needed, and
/// the boot override depends on that. Its plan is produced by **asking the
/// board**, so a person can edit the port field between the question and the
/// answer. The line travels with the plan, so the override runs over the line
/// that was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsoleLine {
    /// The serial port: `/dev/ttyUSB0`, `COM3`.
    pub port: String,
    /// The rate to open it at.
    pub baud: u32,
    /// The prompt to match. Boards use different prompts, so it is named rather
    /// than assumed.
    pub prompt: String,
    /// How many reads to spend on each wait.
    pub reads: u32,
}

/// A boot override waiting for a person to agree to it.
///
/// The console flow's counterpart of [`Pending`] and [`RecoveryPending`], and
/// deliberately the plainest of the three: **there is no [`Confirmation`] here**.
/// The confirmation is a plain yes. A typed coordinate exists because careful
/// reading does not catch a clone with its source and destination swapped. An
/// override has no second board and destroys nothing. It changes volatile RAM on
/// one board for one boot. If a harmless act carried the same ceremony as a
/// dangerous one, people would stop reading the ceremony on the dangerous one.
pub struct BootPending {
    /// What the board answered, and what an override would set. It is produced by
    /// asking the board, so it arrives as a job's report rather than from a pure
    /// call.
    pub plan: BootPlan,
    /// The line it was asked over, and the line the override will run over.
    pub line: ConsoleLine,
}

/// The serial console flow's state: a pending boot override, the session in
/// flight, and the transcript the last session left on screen.
///
/// A sibling of [`Recovery`] rather than part of it, and inside the same flow. A
/// console is reached exactly as a recovery is: a person names a port. The number
/// of flows is bounded by how a board is physically reached, not by what answers
/// on the other end.
#[derive(Default)]
pub struct Console {
    /// A boot override waiting to be agreed to.
    pub pending: Option<BootPending>,
    /// The session in flight, if there is one.
    pub job: Option<ConsoleJob>,
    /// The line the running session was started over.
    ///
    /// It is kept beside the job rather than inside it, because a planned boot
    /// override needs it. The plan comes back as the job's report, and the
    /// override that follows has to run over the same line the plan was asked over.
    pub line: Option<ConsoleLine>,
    /// What the last finished session said, kept on screen after its job is gone.
    /// A console's answer is its transcript, so it outlives the job that read it.
    pub transcript: String,
}

impl Console {
    /// Whether the console flow has anything in it: a plan waiting, a session
    /// running, or a transcript worth keeping on screen.
    pub fn is_active(&self) -> bool {
        self.pending.is_some() || self.job.is_some() || !self.transcript.is_empty()
    }

    /// Throw away the transcript the last session left.
    ///
    /// A transcript keeps the section open after a session ends. It is the
    /// session's answer, so it must not vanish on its own. A person therefore needs
    /// a way to dismiss it once it is read.
    pub fn clear_transcript(&mut self) {
        self.transcript.clear();
    }
}

/// The machine's disks, and whether a person has asked to see them.
///
/// **The list is shown only when asked for.** A board is on the bus because a
/// person put it there, and listing it tells them what they already did. The
/// machine's disks are different. Drawing them beside a board with no distinction
/// is the start of the mistake this backend exists to refuse. The section is
/// therefore closed until it is opened, the window's form of the CLI's `list`
/// against `list --blocks`.
///
/// **The list is not polled.** The bus is rescanned every second so that a board
/// appears promptly when it is plugged in. A disk list is enumerated on demand
/// instead. It is a walk of `/sys/block` per disk rather than a bus scan, and it
/// barely changes. A list of destructive targets also must not reshuffle under
/// the mouse.
#[derive(Default)]
pub struct Disks {
    /// Whether the section is open.
    pub show: bool,
    /// Whether a listing has been made at all, so an empty list can be told from
    /// a question nobody has asked.
    pub asked: bool,
    /// The disks, as of the last listing.
    pub listed: Vec<BlockDevice>,
    /// Why there is no list, where there is none.
    ///
    /// It is kept as a sentence rather than collapsed into an empty list. An empty
    /// list says *this machine has no disks*, which is false on every machine. It
    /// would send a person looking for the disk they can see, instead of telling
    /// them the backend is not built for this platform.
    pub problem: Option<String>,
}

impl Disks {
    /// Take the answer to a listing, whichever it is.
    pub fn seen(&mut self, listed: Result<Vec<BlockDevice>, Error>) {
        self.asked = true;
        match listed {
            Ok(disks) => {
                self.listed = disks;
                self.problem = None;
            }
            Err(error) => {
                self.listed.clear();
                self.problem = Some(error.to_string());
            }
        }
    }
}

/// Everything one window knows.
pub struct Session<T: Transport> {
    /// The devices last seen on the bus, natively. A browser cannot scan a bus,
    /// and is handed the single device a person picked from a chooser.
    pub devices: Vec<DeviceInfo>,
    /// The machine's disks, and whether a person has asked to see them. See
    /// [`Disks`].
    pub disks: Disks,
    /// The board every verb acts on, and for a clone the board that is
    /// overwritten.
    pub target: Board<T>,
    /// The board a clone copies. Only a clone uses it, and it is only read, so a
    /// clone cannot damage the board being copied.
    pub source: Board<T>,
    /// The job in flight, if there is one.
    pub job: Option<Job<T>>,
    /// A maskrom bootstrap in flight, if there is one. It is separate from
    /// [`job`](Self::job) because it holds no agent and hands none back. A maskrom
    /// board is not a [`FlashAgent`], and it re-enumerates as a new device at the
    /// end of the upload.
    pub bootstrap: Option<BootstrapJob>,
    /// An Ingenic boot-ROM bootstrap in flight, if there is one. It is separate
    /// from [`bootstrap`](Self::bootstrap) because its success carries the SoC's
    /// [`CpuInfo`] magic, not a bare `()`. The bootstrap reads the SoC's identity
    /// from the boot ROM during the upload. A person records that value, and a
    /// `soc` gate entry is pinned from it.
    pub ingenic_bootstrap: Option<BootstrapJob<CpuInfo>>,
    /// A plan waiting to be agreed to.
    pub pending: Option<Pending>,
    /// The StarFive recovery flow: files, plan, and the recovery in flight. It is a
    /// serial flow, the second beside the USB flow of [`target`](Self::target) and
    /// [`source`](Self::source). It runs over a port a person names rather than a
    /// bus that is scanned.
    pub recovery: Recovery,
    /// The serial console flow: a boot override waiting to be agreed to, the
    /// session in flight, and the transcript it left. It belongs to the serial flow
    /// beside [`recovery`](Self::recovery), because a console is reached the same
    /// way a recovery is: a person names a port. Only what answers on the port
    /// differs.
    pub console: Console,
    /// The image a write would write, or a verify would compare against.
    pub image: Option<PickedImage>,
    /// What the last job produced, or what went wrong.
    pub last: Option<Outcome>,
    /// How this build asks a person to confirm a write.
    how: ConfirmBy,
    /// When the bus was last scanned, on the frame clock.
    scanned_at: f64,
    /// Whether it has ever been scanned.
    scanned: bool,
}

impl<T: Transport> Session<T> {
    /// A session that has opened nothing.
    pub fn new(how: ConfirmBy) -> Self {
        Self {
            devices: Vec::new(),
            disks: Disks::default(),
            target: Board::default(),
            source: Board::default(),
            job: None,
            bootstrap: None,
            ingenic_bootstrap: None,
            pending: None,
            recovery: Recovery::default(),
            console: Console::default(),
            image: None,
            last: None,
            how,
            scanned_at: 0.0,
            scanned: false,
        }
    }

    /// Whether anything at all is running, across every flow.
    ///
    /// Five slots hold a job, because the kinds of job carry different things
    /// back. Three carry an agent, a `CpuInfo` and a transcript, and the other two
    /// carry nothing. None of them collapses into the others. The window needs an
    /// answer none of them gives alone. It asks whether a job is in flight whose
    /// progress and cancellation a person must be able to see. That holds whichever
    /// part of the window the person is looking at.
    pub fn anything_running(&self) -> bool {
        self.job.is_some()
            || self.bootstrap.is_some()
            || self.ingenic_bootstrap.is_some()
            || self.recovery.job.is_some()
            || self.console.job.is_some()
    }

    /// Whether to scan the bus at this frame.
    ///
    /// **Never during a job.** A job needs the bus quiet, and a person watching
    /// one needs the list still. A device list that reshuffles during a running
    /// write can move the row a person is watching.
    pub fn wants_rescan(&self, now: f64) -> bool {
        self.job.is_none()
            && self.bootstrap.is_none()
            && self.ingenic_bootstrap.is_none()
            && (!self.scanned || now - self.scanned_at >= RESCAN_SECONDS)
    }

    /// Take a fresh device list.
    ///
    /// A selected board that has left the bus and is not open is forgotten. It is
    /// not left as a row naming a device that is not there. A selected board that
    /// is still *open* is left alone. The agent is the authority on whether it
    /// still works, and reports a failure the moment it is used.
    pub fn devices_seen(&mut self, devices: Vec<DeviceInfo>, now: f64) {
        self.devices = devices;
        self.scanned_at = now;
        self.scanned = true;

        for board in [&mut self.target, &mut self.source] {
            // Keyed on the handle, not on what the row says. A handle is what
            // identity *means* in each build -- a place on the bus natively, the
            // browser's own device object in a tab -- and two identical boards
            // agree on every other field.
            //
            // A chosen *disk* carries no handle, so it falls through untouched:
            // this is the bus, and a disk is not on it. The disk list has its own
            // refresh, and a bus scan has nothing to say about one either way.
            let gone = match (board.device.as_ref(), board.handle.as_ref()) {
                (Some(_), Some(chosen)) => !self
                    .devices
                    .iter()
                    .any(|seen| platform::same_board(&platform::handle_of(seen), chosen)),
                _ => false,
            };

            if gone && matches!(board.connection, Connection::Disconnected) {
                board.device = None;
            }
        }
    }

    /// Point the target board at a device, unless a job is holding its agent.
    ///
    /// Returns whether the board was re-pointed. **A [`Busy`](Connection::Busy)
    /// board is refused and left exactly as it is.** A job has that board's agent
    /// on a thread, and hands it back through `restore` when it ends. Re-pointing
    /// the board during the job would attach the old board's agent to a new
    /// board's identity. The plan screen would then name one board while the write
    /// reaches another. That is the one cross-attach the typed-coordinate gate
    /// cannot catch, because both boards are real.
    ///
    /// An idle, disconnected or desynchronized board is re-pointed freely. No
    /// agent is on a thread to come back to the wrong board, and switching an open
    /// board drops its agent, which closes it. The one state the session cannot
    /// see is a board *mid-open*, with an agent on its way through the frame loop's
    /// `collect_opened`. Only the frame loop knows that state, so
    /// [`App::can_select`](crate::app::App::can_select) gates that case instead.
    pub fn select_target(&mut self, handle: Handle, device: DeviceInfo) -> bool {
        if self.target.connection.is_busy() {
            return false;
        }
        self.target.select(handle, device);
        // The plan named a board, and this is a different one.
        self.pending = None;
        true
    }

    /// Point the source board, the one a clone copies, at a device, unless a job
    /// is holding its agent.
    ///
    /// A [`Busy`](Connection::Busy) source is refused for the reason
    /// [`select_target`](Self::select_target) gives. A clone holds both boards busy
    /// while it runs.
    pub fn select_source(&mut self, handle: Handle, device: DeviceInfo) -> bool {
        if self.source.connection.is_busy() {
            return false;
        }
        self.source.select(handle, device);
        self.pending = None;
        true
    }

    /// Point the target slot at a disk. The disk counterpart of
    /// [`select_target`](Self::select_target), refusing a busy slot for exactly
    /// the same reason.
    pub fn select_target_disk(&mut self, disk: BlockDevice) -> bool {
        if self.target.connection.is_busy() {
            return false;
        }
        self.target.select_disk(disk);
        self.pending = None;
        true
    }

    /// Point the source slot, the one a clone copies from, at a disk.
    ///
    /// A clone with a disk is useful in both directions: a card cloned onto a
    /// board, and a board's flash cloned onto a card. The one write loop serves
    /// both. A clone is the write path fed by a different image source, and the
    /// source is only read.
    pub fn select_source_disk(&mut self, disk: BlockDevice) -> bool {
        if self.source.connection.is_busy() {
            return false;
        }
        self.source.select_disk(disk);
        self.pending = None;
        true
    }

    /// Stop pointing at a board to copy, unless a job is holding its agent.
    ///
    /// The counterpart of the two `select_source` methods. A slot that can be
    /// filled has to be emptiable. Otherwise the panel that draws it stays on the
    /// screen for the rest of the session. The agent goes with it, which closes the
    /// board. That is intended, because a person usually empties the slot to give
    /// a board back.
    ///
    /// A busy source is refused for the reason
    /// [`select_source`](Self::select_source) refuses one. A clone has that
    /// board's agent on a thread, and hands it back through `restore`.
    pub fn forget_source(&mut self) -> bool {
        if self.source.connection.is_busy() {
            return false;
        }
        self.source = Board::default();
        // A clone plan named two boards, and one of them is gone.
        self.pending = None;
        true
    }

    /// Take a fresh disk listing, however it turned out.
    ///
    /// **A chosen disk is left exactly as it is.** A board is forgotten when the
    /// bus stops reporting it. A disk that has gone is different, because its open
    /// fails and reports why. The slot is also the only record of the node path a
    /// person is about to type into the write gate. Clearing it during a
    /// confirmation would be worse than a row that is briefly wrong.
    pub fn disks_seen(&mut self, listed: Result<Vec<BlockDevice>, Error>) {
        self.disks.seen(listed);
    }

    /// Take an image, or take it away.
    ///
    /// **Changing the image discards any plan made against the old one.** A plan
    /// says how many bytes go where, and [`verbs::flash`] streams exactly that many
    /// from whatever it is handed. A plan confirmed against one image and fed
    /// another would write the wrong length of the wrong file. It would land in
    /// sectors a person approved for something else. The plan and the image are
    /// agreed to together, or not at all.
    pub fn set_image(&mut self, image: Option<PickedImage>) {
        self.image = image;
        self.pending = None;
    }

    /// Take the recovery agent, or take it away.
    ///
    /// **Changing any recovery input discards a recovery plan made against the old
    /// inputs**, for the reason [`set_image`](Self::set_image) gives. The plan
    /// counts the exact bytes each transfer sends. A plan confirmed against one set
    /// of files and handed another would send the wrong lengths, under menu options
    /// a person approved for different files.
    pub fn set_recovery_agent(&mut self, agent: Option<PickedBlob>) {
        self.recovery.agent = agent;
        self.recovery.pending = None;
    }

    /// Take the SPL, or take it away. See
    /// [`set_recovery_agent`](Self::set_recovery_agent).
    pub fn set_recovery_spl(&mut self, spl: Option<PickedBlob>) {
        self.recovery.spl = spl;
        self.recovery.pending = None;
    }

    /// Take the U-Boot payload, or take it away. See
    /// [`set_recovery_agent`](Self::set_recovery_agent).
    pub fn set_recovery_uboot(&mut self, uboot: Option<PickedBlob>) {
        self.recovery.uboot = uboot;
        self.recovery.pending = None;
    }

    /// Plan a StarFive recovery: the dry run.
    ///
    /// It is pure and **synchronous**. Unlike a USB
    /// [`plan_write`](verbs::plan_write), it has no device to ask. A serial
    /// receiver in recovery cannot report geometry or a table, and cannot be read.
    /// It is therefore [`recovery::plan_recover`] over the picked files. Its answer
    /// becomes a screen waiting to be agreed to. The screen carries the one fact a
    /// person recovering a board needs that no other write states: the write is
    /// not read back.
    ///
    /// It refuses up front what it can. A recovery with no port, no agent, or
    /// neither an SPL nor a U-Boot is refused. The plan and the port are remembered
    /// together, because the confirmation is typed against the port.
    pub fn plan_recovery(&mut self, port: String, target: RecoveryTarget) -> Result<(), Error> {
        let port = port.trim().to_string();
        if port.is_empty() {
            return Err(Error::InvalidRequest(
                "name the serial port the board is on, for example /dev/ttyUSB0".to_string(),
            ));
        }
        let Some(agent) = self.recovery.agent.as_ref() else {
            return Err(Error::InvalidRequest(
                "choose the recovery agent (a jh7110-recovery-*.bin) to upload first".to_string(),
            ));
        };

        // `plan_recover` is what refuses a recovery that writes neither an SPL nor
        // a U-Boot, so that check is not duplicated here.
        let request = recovery::RecoveryRequest {
            target,
            agent: &agent.bytes,
            spl: self.recovery.spl.as_ref().map(|blob| blob.bytes.as_slice()),
            uboot: self
                .recovery
                .uboot
                .as_ref()
                .map(|blob| blob.bytes.as_slice()),
        };
        let plan = recovery::plan_recover(&request)?;

        self.recovery.pending = Some(RecoveryPending {
            confirmation: Confirmation::for_port(self.how, &port),
            plan,
            port,
        });
        Ok(())
    }

    /// Throw away a recovery plan nobody agreed to.
    pub fn dismiss_recovery_plan(&mut self) {
        self.recovery.pending = None;
    }

    /// Say yes to the recovery plan on the screen.
    ///
    /// **This is the whole recovery gate.** A [`ConfirmedRecovery`] is minted here
    /// and nowhere else. A recovery no person agreed to is therefore, as in the
    /// CLI, a value that cannot be built. It hands back the port and the owned
    /// bytes to write alongside the consent. The job needs all three, and producing
    /// them spends the plan.
    ///
    /// `None` when the port has not been typed, or, defensively, when a file is
    /// missing. The plan then stays on the screen. A button that was not supposed
    /// to act must not remove the plan a person is reading.
    pub fn confirm_recovery(
        &mut self,
    ) -> Option<(String, OwnedRecoveryRequest, ConfirmedRecovery)> {
        if !self
            .recovery
            .pending
            .as_ref()
            .is_some_and(RecoveryPending::can_confirm)
        {
            return None;
        }

        // The files the plan was made against, cloned into owned bytes the job
        // holds across a thread. Their presence was checked when the plan was made,
        // and changing any of them threw the plan away -- so if the plan is still
        // here, the agent is too. The `?` is a belt to that braces.
        let agent = self.recovery.agent.as_ref()?.bytes.clone();
        let spl = self.recovery.spl.as_ref().map(|blob| blob.bytes.clone());
        let uboot = self.recovery.uboot.as_ref().map(|blob| blob.bytes.clone());

        let pending = self.recovery.pending.take()?;
        let request = OwnedRecoveryRequest {
            target: pending.plan.target,
            agent,
            spl,
            uboot,
        };
        Some((pending.port, request, pending.plan.confirm()))
    }

    /// Make a recovery job over an opened serial port.
    ///
    /// The serial counterpart of [`start_bootstrap`](Self::start_bootstrap). It
    /// takes an opened [`Serial`] transport rather than lending an agent, because a
    /// recovery holds no [`FlashAgent`] and hands none back. `None` when a recovery
    /// is already running, and the transport it was handed is then dropped, closing
    /// the port. A USB job can run alongside a recovery, because they use different
    /// transports on different flows and do not contend.
    pub fn start_recovery<S: Serial + 'static>(
        &mut self,
        serial: S,
        request: OwnedRecoveryRequest,
        confirmed: ConfirmedRecovery,
        now: f64,
        wake: Wake,
    ) -> Option<RecoveryWork<S>> {
        if self.recovery.job.is_some() {
            return None;
        }
        let (work, job) = RecoveryWork::new(serial, request, confirmed, now, wake);
        self.recovery.job = Some(job);
        Some(work)
    }

    /// Start a console session over an opened serial port.
    ///
    /// The console counterpart of [`start_recovery`](Self::start_recovery). It
    /// holds no [`FlashAgent`] for the same reason: a console is a serial line, not
    /// a block backend. `None` when a session is already running, and the
    /// transport it was handed is then dropped, closing the port. A USB job can run
    /// alongside a session, because they use different transports on different
    /// flows and do not contend.
    ///
    /// Starting a session clears the transcript the last one left, because a
    /// transcript holding two sessions reads as one.
    pub fn start_console<S: Serial + 'static>(
        &mut self,
        serial: S,
        line: &ConsoleLine,
        task: ConsoleTask,
        now: f64,
        wake: Wake,
    ) -> Option<ConsoleWork<S>> {
        if self.console.job.is_some() {
            return None;
        }
        self.console.transcript.clear();
        self.console.line = Some(line.clone());
        let (work, job) =
            ConsoleWork::new(serial, line.prompt.clone(), line.reads, task, now, wake);
        self.console.job = Some(job);
        Some(work)
    }

    /// Take back a finished console session, if it has finished.
    ///
    /// As with [`harvest_recovery`](Self::harvest_recovery), there is no agent to
    /// restore, because the port is dropped when the session ends. Two things
    /// differ. The **transcript is kept**, because a console's answer is what it
    /// printed, and it must not vanish with the job that read it. A **planned boot
    /// override does not become a report**. It is the screen a person agrees to, so
    /// it goes to [`console.pending`](Console::pending) instead. Any previous report
    /// is cleared, so that the plan is what is on screen.
    pub fn harvest_console(&mut self) -> bool {
        let Some(job) = &self.console.job else {
            return false;
        };
        let Some(outcome) = job.take_ended() else {
            return false;
        };
        // Taken before the job is dropped: what it printed is the answer.
        self.console.transcript = job.transcript();
        let line = self.console.line.take();
        self.console.job = None;

        match outcome {
            Ok(ConsoleReport::BootPlanned(plan)) => {
                // The dry run's answer is a screen, not a report.
                self.last = None;
                self.console.pending = line.map(|line| BootPending { plan, line });
            }
            outcome => self.last = Some(outcome.map(Report::Console)),
        }
        true
    }

    /// Throw away a boot override nobody agreed to.
    pub fn dismiss_boot_plan(&mut self) {
        self.console.pending = None;
    }

    /// Say yes to the boot override on the screen.
    ///
    /// **This is the whole of the console flow's gate.** A [`ConfirmedBoot`] is
    /// minted here and nowhere else. An override no person agreed to is therefore,
    /// as in the CLI, a value that cannot be built. It hands back the line the plan
    /// was made over alongside the consent. The override runs over that line, not
    /// over whatever the form holds at the time.
    ///
    /// The act is a plain yes. [`BootPending`] says why this one gate asks for no
    /// typed coordinate.
    pub fn confirm_boot(&mut self) -> Option<(ConsoleLine, ConfirmedBoot)> {
        let pending = self.console.pending.take()?;
        Some((pending.line, pending.plan.confirm()))
    }

    /// Say yes to the plan on the screen.
    ///
    /// **This is the whole write gate.** A [`ConfirmedWrite`], a [`ConfirmedClone`]
    /// or a [`ConfirmedSegmentedWrite`] is minted here and nowhere else in the GUI.
    /// A write no person agreed to is therefore, as in the CLI, a compile error
    /// rather than a code review.
    ///
    /// `None` when the act has not been performed (the coordinate is not typed, or
    /// the destination has not been picked again). Also `None` when the write
    /// would be refused anyway. The plan then stays on the screen a person is
    /// reading. A button that was not supposed to act must not make the plan
    /// vanish.
    pub fn confirm(&mut self) -> Option<Confirmed> {
        if !self.pending.as_ref().is_some_and(Pending::can_confirm) {
            return None;
        }
        // Taken for good, now that it is taken at all.
        self.pending.take().map(Pending::into_confirmed)
    }

    /// Throw away a plan nobody agreed to.
    pub fn dismiss_plan(&mut self) {
        self.pending = None;
    }

    /// Scan the bus at the next opportunity, rather than waiting for the next
    /// scheduled rescan.
    pub fn force_rescan(&mut self) {
        self.scanned = false;
    }

    /// Make a job, if the boards it needs are free.
    ///
    /// It hands back the work rather than running it. The app can then spawn it on
    /// a thread, and a test can drive it to completion inline. `None` means a board
    /// it needs is busy, closed or finished, and nothing has been taken.
    pub fn start(&mut self, task: Task, now: f64, wake: Wake) -> Option<Work<T>> {
        // **A bootstrap holds the flow as firmly as a job does.** It is not a
        // board this session can see as busy -- a maskrom or boot-ROM device is
        // not a [`FlashAgent`], so the target's connection reads `Disconnected`
        // throughout -- and while it runs the board it uploads to is on its way
        // to disappearing off the bus. Letting a verb start on some *other* board
        // in that window is how a second board's agent ends up held with no panel
        // to close it from. [`start_bootstrap`](Self::start_bootstrap) refuses
        // while a job runs; this is the other half of that.
        if self.bootstrap.is_some() || self.ingenic_bootstrap.is_some() {
            return None;
        }
        if self.job.is_some() || !self.target.connection.is_idle() {
            return None;
        }
        if task.needs_source() && !self.source.connection.is_idle() {
            return None;
        }

        let target = self.target.connection.take()?;
        let source = if task.needs_source() {
            match self.source.connection.take() {
                Some(agent) => Some(agent),
                None => {
                    // Nothing is running, so the board that was taken goes
                    // straight back rather than sitting Busy with no job.
                    self.target.connection.opened(target);
                    return None;
                }
            }
        } else {
            None
        };

        let (work, job) = Work::new(target, source, task, now, wake);
        self.job = Some(job);
        Some(work)
    }

    /// Make a maskrom bootstrap job over an already-open transport.
    ///
    /// Unlike [`start`](Self::start), this takes the transport rather than lending
    /// an agent, because a maskrom board is not a [`FlashAgent`]. `None` when a job
    /// or another bootstrap is already running. The transport it was handed is then
    /// dropped, which closes it.
    pub fn start_bootstrap(
        &mut self,
        transport: T,
        loader: LoaderImage,
        soc: Option<Soc>,
        board: Option<Handle>,
        now: f64,
        wake: Wake,
    ) -> Option<BootstrapWork<T>> {
        if self.job.is_some() || self.bootstrap.is_some() || self.ingenic_bootstrap.is_some() {
            return None;
        }
        let (work, job) = BootstrapWork::new(transport, loader, soc, board, now, wake);
        self.bootstrap = Some(job);
        Some(work)
    }

    /// Make an Ingenic boot-ROM bootstrap job over an already-open transport.
    ///
    /// The Ingenic counterpart of [`start_bootstrap`](Self::start_bootstrap). It
    /// takes a bare transport rather than an agent, because a boot-ROM board is not
    /// a [`FlashAgent`]. Its flash is unreachable until the uploaded loader is
    /// running. `None` when a job or either bootstrap is already running. The
    /// transport it was handed is then dropped, which closes it.
    pub fn start_ingenic_bootstrap(
        &mut self,
        transport: T,
        loader: IngenicLoader,
        board: Option<Handle>,
        now: f64,
        wake: Wake,
    ) -> Option<IngenicBootstrapWork<T>> {
        if self.job.is_some() || self.bootstrap.is_some() || self.ingenic_bootstrap.is_some() {
            return None;
        }
        let (work, job) = IngenicBootstrapWork::new(transport, loader, board, now, wake);
        self.ingenic_bootstrap = Some(job);
        Some(work)
    }

    /// Take back what a finished job left, if it has finished.
    ///
    /// It is called once a frame. The agents come back here, a plan becomes a
    /// screen waiting for an answer, and what a board reported is kept.
    ///
    /// **A job that ended by dying takes its agents with it.** A panic on the job
    /// thread cannot hand an agent back, so there is nothing to restore. The
    /// connections it held are landed in
    /// [`Desynchronized`](Connection::Desynchronized), and a fault is reported.
    /// Otherwise they would stay [`Busy`](Connection::Busy) forever, waiting for
    /// an agent that is gone.
    pub fn harvest(&mut self) -> bool {
        let Some(job) = &self.job else {
            return false;
        };
        let Some(ended) = job.take_ended() else {
            return false;
        };
        self.job = None;

        match ended {
            Ended::Returned {
                target,
                source,
                outcome,
            } => {
                self.target.connection.restore(target);
                if let Some(source) = source {
                    self.source.connection.restore(source);
                }
                self.remember(&outcome);
                self.last = Some(outcome);
            }
            Ended::Died { had_source } => {
                self.target.connection = Connection::Desynchronized;
                if had_source {
                    self.source.connection = Connection::Desynchronized;
                }
                self.last = Some(Err(Error::Internal(
                    "the job ended unexpectedly, and the board's connection was lost. Reopen the board"
                        .to_string(),
                )));
            }
        }
        true
    }

    /// Take back a finished maskrom bootstrap, if it has finished.
    ///
    /// There is no agent to restore. The board re-enumerates on success, and can
    /// be wedged on failure. This method therefore forgets the target board, if
    /// the slot still holds it, and forces a rescan. The board that reappears is
    /// then found and can be selected afresh. If the upload worked, it reappears
    /// in loader mode.
    ///
    /// The re-enumeration is verified on hardware: an RK3576 reappeared as a
    /// loader after the `0x0472` jump. This GUI flow driving it end to end is
    /// **\[UNVERIFIED\]**. The verbs it calls are pinned against a scripted
    /// transport, but no board has run the button.
    pub fn harvest_bootstrap(&mut self) -> bool {
        let Some(job) = &self.bootstrap else {
            return false;
        };
        let Some(outcome) = job.take_ended() else {
            return false;
        };
        let uploaded_to = job.board.clone();
        let chip = job.chip.clone();
        self.bootstrap = None;

        self.forget_bootstrapped_board(uploaded_to.as_ref());

        self.last = Some(outcome.map(|()| Report::Bootstrapped { chip }));
        true
    }

    /// Forget the target slot, but only if it still holds the board the bootstrap
    /// uploaded to.
    ///
    /// The device the upload spoke to is gone. On success it re-enumerates as a
    /// different device, so the old selection names a board that has left the bus.
    /// The bus is rescanned either way, so the board that reappears is found and
    /// can be selected afresh.
    ///
    /// **It does not clear a slot a person re-pointed while the upload ran.**
    /// Nothing prevents that re-pointing. A maskrom or boot-ROM board is not a
    /// [`FlashAgent`], so the target's connection reads `Disconnected` throughout,
    /// and every `Use` button in the list stays live. Replacing the whole
    /// [`Board`] regardless would drop a second board's identity while its agent
    /// is still held. That agent is an open USB interface or a disk under
    /// `O_EXCL`, with no panel drawn and no way to close it. The handle is
    /// therefore compared, and a slot holding anything else is left as it is.
    fn forget_bootstrapped_board(&mut self, uploaded_to: Option<&Handle>) {
        let still_the_same = match (uploaded_to, self.target.handle.as_ref()) {
            (Some(uploaded), Some(chosen)) => platform::same_board(uploaded, chosen),
            // A bootstrap with no handle to compare, or a slot pointed at
            // something with none (a disk), is not the board that was uploaded to.
            _ => false,
        };
        if still_the_same {
            self.target = Board::default();
        }
        self.force_rescan();
    }

    /// Take back a finished Ingenic boot-ROM bootstrap, if it has finished.
    ///
    /// The Ingenic counterpart of [`harvest_bootstrap`](Self::harvest_bootstrap).
    /// It forgets the target and forces a rescan for the same reason. On success
    /// the `VR_PROGRAM_START2` jump removes the boot-ROM device, and a DFU gadget
    /// re-enumerates in its place as a different device. The old selection then
    /// names a board that has left the bus.
    ///
    /// It differs in what it keeps. The outcome carries the [`CpuInfo`] the boot
    /// ROM reported, and that magic goes into the report unchanged. A `soc` gate
    /// entry is pinned from that value, so reading it from a real board is what
    /// the run is for. The GUI flow driving this end to end is **\[UNVERIFIED\]**.
    /// The verbs it calls are pinned against a scripted transport, but no Ingenic
    /// board has run the button.
    pub fn harvest_ingenic_bootstrap(&mut self) -> bool {
        let Some(job) = &self.ingenic_bootstrap else {
            return false;
        };
        let Some(outcome) = job.take_ended() else {
            return false;
        };
        let uploaded_to = job.board.clone();
        self.ingenic_bootstrap = None;

        // The boot-ROM device the upload spoke to is gone: on success it comes back
        // as a DFU gadget, a different device, so the old selection is stale -- but
        // only if the slot still holds it. See `forget_bootstrapped_board`.
        self.forget_bootstrapped_board(uploaded_to.as_ref());

        self.last = Some(outcome.map(Report::IngenicBootstrapped));
        true
    }

    /// Take back a finished recovery, if it has finished.
    ///
    /// As with [`harvest_bootstrap`](Self::harvest_bootstrap), there is no agent
    /// to restore, so this only reports the outcome. The port is dropped. If the
    /// recovery worked, the board needs to be powered off and re-strapped rather
    /// than addressed again. The picked files are kept, because a person
    /// recovering a second board uses the same agent and payloads.
    pub fn harvest_recovery(&mut self) -> bool {
        let Some(job) = &self.recovery.job else {
            return false;
        };
        let Some(outcome) = job.take_ended() else {
            return false;
        };
        self.recovery.job = None;
        self.last = Some(outcome.map(|()| Report::Recovered));
        true
    }

    /// Keep what a job learned where the next frame can find it.
    fn remember(&mut self, outcome: &Outcome) {
        let Ok(report) = outcome else {
            return;
        };

        match report {
            Report::Info(flash) => self.target.flash = Some(flash.clone()),

            Report::Partitions(table) => {
                self.target.table = match table {
                    Some(table) => Table::Read(table.clone()),
                    None => Table::Absent,
                };
            }

            Report::ChipVersion(version) => self.target.chip_version = Some(version.clone()),

            // The loader's account of itself and which medium it addresses are
            // both reports rather than facts the board panel keeps: nothing is
            // gated on either, and a medium cached across a `switch_storage`
            // pyrographer does not send would be a stale answer to a question
            // that only means anything now.
            Report::Capability(_) | Report::StorageMedium(_) => {}

            // The dry run's answer becomes the screen. It carries the loader's
            // account of itself, so that is worth keeping too -- it is the same
            // bytes `chipver` prints, and the plan is where a person reads them.
            Report::Planned(plan) => {
                self.target.chip_version = Some(plan.chip_version.clone());
                self.pending = self.awaiting(Plan::Write(plan.clone()));
            }

            Report::PlannedClone(plan) => {
                self.target.chip_version = Some(plan.destination.chip_version.clone());
                self.pending = self.awaiting(Plan::Clone(plan.clone()));
            }

            Report::PlannedTable(plan) => {
                self.target.chip_version = Some(plan.chip_version.clone());
                self.pending = self.awaiting(Plan::Table(plan.clone()));
            }

            // A board that has taken a reset has left, under every mode -- and the
            // three that are not a plain reboot leave it somewhere no verb here
            // can reach. Everything it told us was about a board no longer there.
            Report::Reset { .. } => self.target.close(),

            // A bootstrap, a recovery, and a console session never reach here: each
            // is harvested by its own method, which sets `last` directly, because
            // there is no agent for the normal harvest to restore. A finished repair
            // is an ordinary returned job, but it kept nothing to remember here --
            // the read-back was its own proof, and the table it rewrote is re-read
            // on demand.
            Report::Console(_)
            | Report::Dumped { .. }
            | Report::Verified { .. }
            | Report::Wrote { .. }
            | Report::Cloned { .. }
            | Report::Bootstrapped { .. }
            | Report::IngenicBootstrapped(_)
            | Report::Recovered
            | Report::TableWritten { .. } => {}
        }
    }

    /// A plan, and the act this build asks for before it becomes a write.
    ///
    /// The confirmation is always against the board that is *overwritten*, which
    /// for a clone is the target and not the board being copied. A swapped clone
    /// is refused for that reason alone. With the two boards swapped, the string to
    /// be typed names a different board.
    fn awaiting(&self, plan: Plan) -> Option<Pending> {
        let destination = self.target.device.as_ref()?;
        // The whole gate, plan in hand: the same comparison the write will
        // make, from the same call. A refused plan still gets its screen -- the
        // screen is the explanation -- but not a confirmation field that
        // pretends typing could unlock it.
        let refused = self
            .target
            .connection
            .agent()
            .and_then(|agent| plan.refusal(agent))
            .map(|error| error.to_string());
        Some(Pending {
            plan,
            confirmation: Confirmation::asked_of(self.how, destination),
            refused,
        })
    }
}

#[cfg(test)]
mod tests;
