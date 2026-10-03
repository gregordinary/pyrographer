//! The frame loop, and the tasks that run outside it.
//!
//! egui is immediate mode: `App::ui` runs every frame and must not block.
//! Everything that waits therefore runs as a task of its own, and puts its answer
//! into a slot the next frame reads. That covers opening a board, choosing a file,
//! choosing a board, and every verb. `App::logic` collects those answers, and is
//! the only place the [`Session`] changes shape.
//!
//! This module connects egui to the session. It holds egui's `Context`, knows
//! which build it is, and turns a button into a [`Task`]. The reasoning is in
//! [`state`](crate::state), which has no egui in it. The drawing is in [`ui`],
//! which is thin.

use eframe::egui;
use pyrographer_core::agent::FlashAgent;
use pyrographer_core::block::BlockDevice;
use pyrographer_core::codec::rkboot::LoaderImage;
use pyrographer_core::codec::rockusb::ResetMode;
use pyrographer_core::partition::TableFormat;
use pyrographer_core::soc::Soc;
use pyrographer_core::verbs::ParamMedium;
use pyrographer_core::{Error, Result};

use crate::platform::{
    self, PickedBlob, PickedImage, PickedSink, SerialWire, Shared, Wake, Wire, share,
};
use crate::state::{Aim, Board, Chosen, ConfirmBy, Confirmed, LayoutSource, Session, Task, lock};
use crate::ui;

// The two serial flows -- StarFive recovery and the bootloader console -- run in
// both builds. Their *state* and their jobs are cross-platform and live on the
// session; what is here is the forms a person fills in and the acts that start a
// session. Only the acquisition differs, and it differs in `platform`: natively a
// port is a path somebody types, and in a tab it is an object a gesture-bound
// chooser hands back.
use crate::state::{ConsoleLine, ConsoleTask};
use pyrographer_core::console;
use pyrographer_core::recovery::RecoveryTarget;
use pyrographer_core::uboot::{Gadget, GadgetDevice};

/// How this build asks somebody to confirm a write.
///
/// This is the one place the choice between the two platforms' answers is made.
/// [`ConfirmBy`] states the rule both answers follow, and why they differ.
#[cfg(not(target_arch = "wasm32"))]
const CONFIRM_BY: ConfirmBy = ConfirmBy::Coordinate;

/// How this build asks somebody to confirm a write.
#[cfg(target_arch = "wasm32")]
const CONFIRM_BY: ConfirmBy = ConfirmBy::Repick;

/// Where a task that is not on the frame loop drops its answer.
///
/// The outer `None` is *still running*. The answer can itself be an absence, such
/// as a dialog somebody closed, so the two are not flattened into one. "Still
/// choosing" leaves the button disabled, and "chose nothing" gives it back.
///
/// A slot is only ever filled through [`platform::spawn_answering`], which answers
/// with a fault for a task that dies. *Still running* therefore cannot outlive the
/// task, and no spinner runs forever over a slot that nothing will fill.
type Slot<T> = Shared<Option<T>>;

/// What a dialog answered: something, nothing, or a failure.
type Picked<T> = Result<Option<T>>;

/// A picked loader and the maskrom transport opened for it, or a closed dialog,
/// or a failure. Named because the nested type is otherwise hard to read.
type BootstrapPick = Result<Option<(Wire, LoaderImage)>>;

/// Core's Ingenic bootstrap facts: the family-wide load addresses and the default
/// DRAM settle the boot-ROM form starts from.
use pyrographer_core::bootstrap::ingenic;

/// A built Ingenic loader and the boot-ROM transport opened for it, or a closed
/// dialog, or a failure. The Ingenic counterpart of [`BootstrapPick`]: the loader
/// is assembled from the form before the open, so the task only opens the board.
type IngenicBootstrapPick = Result<Option<(Wire, ingenic::IngenicLoader)>>;

/// Which of the two boards something is about.
///
/// There are two because a clone has two. They differ in which one is destroyed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    /// The board every verb acts on, and the one a write overwrites.
    Target,
    /// The board a clone copies. Only ever read.
    Source,
}

/// Which flow the window is showing.
///
/// The window splits on *what answers*, not on what the device is plugged into.
/// One side drives a [`FlashAgent`]: a board in a boot mode, or a disk the host
/// also owns. A flash agent has LBAs, geometry, a partition table and the uniform
/// verbs. It is reached three ways: scanning the bus, asking the host for its
/// disks, and bootstrapping a board that answers nothing else yet. The other side
/// drives a serial line, where nothing has a `FlashAgent`, so none of that surface
/// is offered or grayed out.
///
/// The sections are organized by the same rule, and the tabs are where the window
/// splits on it. Adding a board whose backend reads and writes flash adds no tab.
///
/// The bar is not drawn over a plan waiting for confirmation. A tab would give a
/// person a way out of the gate, and the gate deliberately has none, so a plan
/// takes the whole window. The chosen tab is remembered across the plan, so
/// canceling returns the person to where they were.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    /// Boards and disks: everything with a flash agent under it, and the
    /// bootstraps that bring a board to one.
    #[default]
    Flash,
    /// A serial line: StarFive recovery, and a bootloader prompt.
    Serial,
}

impl Tab {
    /// Both tabs, in the order the bar draws them.
    pub const ALL: [Tab; 2] = [Tab::Flash, Tab::Serial];

    /// What the tab is called.
    pub fn name(self) -> &'static str {
        match self {
            Tab::Flash => "Boards and disks",
            Tab::Serial => "Serial",
        }
    }

    /// What answers on this side, in one sentence. It is the tab's own
    /// description, and it points at a job running on the other tab.
    pub fn describe(self) -> &'static str {
        match self {
            Tab::Flash => {
                "Targets whose flash pyrographer reads and writes: a board in a boot mode, or \
                 one of this machine's own disks. Read, write, clone, and verify flash, and \
                 repair or author a partition table."
            }
            Tab::Serial => {
                "Targets on a serial line, which have no sectors and no partition table: \
                 StarFive recovery, and a bootloader prompt."
            }
        }
    }
}

/// What has been typed into the form.
///
/// The fields are strings, because a text field holds a string. Parsing them is
/// this module's job, and [`state`](crate::state) takes numbers.
pub struct Form {
    /// Aim at a partition the device named, rather than at a number somebody
    /// worked out. It is on by default, because it is the safer form. It is the
    /// only form that knows where a partition *ends*. It is therefore the only one
    /// that can refuse an image too big to fit.
    pub by_name: bool,
    /// The partition, for aiming by name.
    pub partition: String,
    /// The first sector, for aiming by number.
    pub lba: String,
    /// How many sectors a raw dump reads.
    pub sectors: String,
    /// The SoC the person says this board is, for the wrong-loader gate.
    /// It is free text here, and [`App::named_soc`] parses it.
    pub soc: String,
    /// Which ending `reset` asks the board for.
    ///
    /// Unlike its neighbors, it is typed rather than a string. It is chosen from a
    /// list of the four modes. There is no text to parse, and no way to name a mode
    /// that does not exist.
    pub reset_mode: ResetMode,
}

impl Default for Form {
    fn default() -> Self {
        Self {
            by_name: true,
            partition: String::new(),
            lba: "0".to_string(),
            sectors: String::new(),
            soc: String::new(),
            reset_mode: ResetMode::default(),
        }
    }
}

/// Which source a fresh table is authored from.
///
/// The three layout front-ends, as radio buttons rather than the CLI's mutually
/// exclusive flags. [`Text`](Self::Text) frames an existing parameter block whole.
/// It is offered only for a Rockchip parameter table, because a GPT is not built
/// from parameter text. The author form therefore hides it whenever
/// [`AuthorForm::format`] is a GPT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorSourceKind {
    /// A native-format layout file.
    Native,
    /// A file holding a board's own `mtdparts=` line.
    Mtdparts,
    /// An existing parameter block's whole text, kept verbatim. Parameter only.
    Text,
}

/// What has been chosen in the "Author a fresh table" form.
///
/// It is cross-platform, as [`RecoverForm`] is. Authoring a table is an ordinary
/// gated write through a flash agent, which the web build carries as well as the
/// native one.
///
/// The picked file is held here as bytes rather than behind a streaming handle,
/// because a layout is a few lines of text. It is *not* coupled to the plan the way
/// [`Session::image`](crate::state::Session::image) is. A table authoring bakes its
/// bytes into the [`SegmentedPlan`](pyrographer_core::verbs::SegmentedPlan) when it
/// is planned. Changing the file afterward therefore cannot reach a plan already
/// made.
pub struct AuthorForm {
    /// Which format a fresh authoring writes.
    pub format: TableFormat,
    /// Where the layout comes from.
    pub source: AuthorSourceKind,
    /// Where the parameter copies go, for a Rockchip parameter table. Ignored for a
    /// GPT, which is absolute.
    pub medium: ParamMedium,
    /// The layout or block-text file a person picked: its name, and its bytes.
    pub file: Option<PickedBlob>,
    /// Whether the author sub-form is revealed. The form has many settings, so it
    /// stays a single button until a person asks for it. The serial recovery form
    /// uses the same "declared, not dumped" shape.
    pub show: bool,
}

impl Default for AuthorForm {
    fn default() -> Self {
        Self {
            format: TableFormat::Gpt,
            source: AuthorSourceKind::Native,
            medium: ParamMedium::Emmc,
            file: None,
            show: false,
        }
    }
}

/// What has been typed into the StarFive recovery form.
///
/// The serial flow's counterpart of [`Form`]. The picked files live on the
/// session's recovery state, as the image does. This form holds the two small
/// things entered with them: which serial port the board is on, and which boot
/// medium to write.
pub struct RecoverForm {
    /// What the port is called: natively the path somebody typed, and in a tab
    /// the name [`platform::port_name`] gives the object a chooser handed back.
    ///
    /// It is a name in both builds. The plan quotes it, and a native confirmation
    /// is transcribed from it. What gets *opened* is a
    /// [`PortHandle`](platform::PortHandle), which natively is this same string and
    /// in a tab is the object.
    pub port: String,
    /// The boot medium both stages are written to.
    pub target: RecoveryTarget,
}

impl Default for RecoverForm {
    fn default() -> Self {
        Self {
            port: String::new(),
            target: RecoveryTarget::NorFlash,
        }
    }
}

/// What has been typed into the serial console form.
///
/// The console flow's counterpart of [`RecoverForm`]. It is larger, because a
/// console is both watched and typed at.
///
/// When a session starts, the line's own fields (port, baud, prompt and reads) are
/// copied into a [`ConsoleLine`], and the session runs against that copy. Editing
/// a field afterward changes what the *next* session does. It never changes what a
/// running session, or a confirmed boot override, is doing.
pub struct ConsoleForm {
    /// What the port is called, the way [`RecoverForm::port`] is a name.
    pub port: String,
    /// The rate to open it at.
    pub baud: u32,
    /// The prompt to match. Boards differ, and pyrographer catalogs none of them.
    pub prompt: String,
    /// How many reads to spend on each wait.
    pub reads: u32,
    /// Text that means it worked, one pattern per line.
    pub expect: String,
    /// Text that means the far end reported a failure, one pattern per line.
    pub fail: String,
    /// Which block device a rockusb gadget exposes.
    pub gadget_dev: String,
    /// The boot order an override sets.
    pub targets: String,
    /// One command line to type at the prompt.
    pub command: String,
    /// Whether the U-Boot half of the section is revealed. The watch needs no
    /// prompt and no board knowledge. Driving a prompt needs both, so that half is
    /// drawn only on request.
    pub show_uboot: bool,
}

impl Default for ConsoleForm {
    fn default() -> Self {
        Self {
            port: String::new(),
            baud: pyrographer_core::transport::DEFAULT_BAUD,
            prompt: pyrographer_core::uboot::DEFAULT_PROMPT.to_string(),
            reads: pyrographer_core::console::DEFAULT_READS,
            expect: String::new(),
            fail: String::new(),
            gadget_dev: "mmc:0".to_string(),
            targets: String::new(),
            command: String::new(),
            show_uboot: false,
        }
    }
}

/// Which of a recovery's three files a pick is for.
///
/// One file dialog serves all three, so the file it is filling is remembered until
/// it answers. The web build's `choosing_for` has the same shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryFileKind {
    /// The recovery agent (`jh7110-recovery-*.bin`).
    Agent,
    /// The raw `u-boot-spl.bin`, headered for the target at send time.
    Spl,
    /// The U-Boot FIT payload, sent as-is.
    Uboot,
}

/// What has been typed and picked into the Ingenic boot-ROM bootstrap form.
///
/// The USB counterpart of the maskrom "Upload loader" button. It is a form rather
/// than a button, because the two flows need different inputs. A Rockchip loader
/// is one RKBOOT container that carries everything, so a single file pick is
/// enough. An Ingenic bootstrap is a two-stage loader with a per-SoC load address
/// for each stage, which no single pick can express. The stages are therefore held
/// here as picked blobs, and the addresses as text parsed on submit, as
/// [`RecoverForm`] holds its port.
///
/// It is cross-platform, as [`RecoverForm`] is. The web build draws the same
/// working form, and only the board's open differs between the builds.
///
/// The addresses start at the family-wide thingino defaults
/// ([`ingenic::SPL_LOAD_ADDRESS`] and [`ingenic::UBOOT_LOAD_ADDRESS`]) rather than
/// empty. thingino-dfu's `ddr_config_database` gives every XBurst SoC the same
/// `spl_addr` and `uboot_addr`. A default is therefore a fact about the family,
/// not a guess about the part.
///
/// The addresses stay editable, and are **\[COMMUNITY\]**. A build that links its
/// stages elsewhere overrides them. Nothing a write trusts depends on them: the
/// wrong-loader gate is pinned from the `VR_GET_CPU_INFO` magic, not from a load
/// address.
pub struct IngenicForm {
    /// The DRAM-init SPL (stage1), run from SRAM. Required.
    pub stage1: Option<PickedBlob>,
    /// Where stage1 loads and runs, as typed. Parsed as hex on submit.
    pub stage1_addr: String,
    /// The DFU-capable U-Boot (stage2), run from DRAM. Optional. A stage1-only
    /// bootstrap initializes DRAM and stops. It is useful only for probing,
    /// because nothing then re-enumerates.
    pub stage2: Option<PickedBlob>,
    /// Where stage2 loads and runs, as typed. Required once a stage2 is chosen.
    pub stage2_addr: String,
    /// Milliseconds to settle after stage1 while DRAM comes up, as typed.
    pub settle_ms: String,
    /// Whether the bootstrap sub-form is revealed. A boot-ROM board *is*
    /// discovered, as a row in the device list. The reveal is therefore part of
    /// that board's own controls, rather than declared the way serial recovery is.
    pub show: bool,
}

impl Default for IngenicForm {
    fn default() -> Self {
        Self {
            stage1: None,
            stage1_addr: format!("0x{:08x}", ingenic::SPL_LOAD_ADDRESS),
            stage2: None,
            stage2_addr: format!("0x{:08x}", ingenic::UBOOT_LOAD_ADDRESS),
            settle_ms: ingenic::DEFAULT_SETTLE_MS.to_string(),
            show: false,
        }
    }
}

/// The raw maskrom stages, as an alternative to a container.
///
/// `db --loader` takes an rkbin `_loader.bin`, which names its own sections and
/// the SoC it was built for. `db --code471` and `--code472` take the bare stage
/// blobs that mainline U-Boot's binman emits (`u-boot-rockchip-usb471.bin` and
/// `-usb472.bin`). Those blobs have no container around them. The RAM-boot path
/// depends on the second form: it is how a mainline U-Boot is loaded into a
/// maskrom board's DRAM.
///
/// This form is the window's route to the bare stages.
/// [`platform::ask_for_loader`] parses what it is handed and refuses anything that
/// is not a container, so it cannot take them.
///
/// **Bare stages name no SoC, so no gate checks them.** That is a property of the
/// files, not a hole in the gate. `verbs::loader_blob_refusal` passes anything it
/// cannot judge, and says so where the upload is running.
#[derive(Default)]
pub struct MaskromForm {
    /// The DRAM-init stage, sent first. It is optional on the same terms as `db`'s
    /// flags: at least one of the two is needed, and 471 always goes first.
    pub code_471: Option<PickedBlob>,
    /// The loader stage. Optional on the same terms.
    pub code_472: Option<PickedBlob>,
    /// Whether the raw-stage sub-form is revealed. A maskrom board *is*
    /// discovered, so the reveal is part of that board's own controls, as
    /// [`IngenicForm::show`] is.
    pub show: bool,
}

/// Which of the raw maskrom stages a file pick is for.
///
/// One file dialog serves both, and the stage it is filling is remembered until
/// it answers, as in [`IngenicStageKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaskromStageKind {
    /// The DRAM-init stage, uploaded to `0x0471`.
    Code471,
    /// The loader stage, uploaded to `0x0472`.
    Code472,
}

/// Which of the Ingenic bootstrap's two stages a file pick is for.
///
/// One file dialog serves both, so the stage it is filling is remembered until it
/// answers, as in [`RecoveryFileKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngenicStageKind {
    /// The DRAM-init SPL, run from SRAM.
    Stage1,
    /// The DFU-capable U-Boot, run from DRAM.
    Stage2,
}

/// What a serial port is being opened for.
///
/// It is held from the moment an open is requested until the open answers. The
/// transport arrives on a later frame, and by then nothing else records which of
/// the three serial jobs was waiting for it. The confirmations are already spent
/// by the time one of these exists. A [`ConfirmedRecovery`] and a `ConfirmedBoot`
/// are minted where a person said yes, and are carried here to the job.
///
/// [`ConfirmedRecovery`]: pyrographer_core::recovery::ConfirmedRecovery
pub enum PendingSerial {
    /// A StarFive recovery, with the files it writes and the plan's confirmation.
    Recovery {
        /// The agent, and whichever of the SPL and U-Boot payload were chosen.
        request: crate::state::OwnedRecoveryRequest,
        /// The plan, confirmed.
        confirmed: pyrographer_core::recovery::ConfirmedRecovery,
    },
    /// A console session: a watch, a gadget, a typed line, a boot plan, or a
    /// boot override that has already been confirmed.
    Console {
        /// The line the session runs over, read from the form at the session's
        /// start.
        line: ConsoleLine,
        /// What the session does.
        task: ConsoleTask,
    },
}

impl PendingSerial {
    /// What to call the work in a fault report, for an open that never answers.
    fn what(&self) -> &'static str {
        match self {
            PendingSerial::Recovery { .. } => "opening the port for a recovery",
            PendingSerial::Console { .. } => "opening the port for a console session",
        }
    }
}

/// Which of the two serial forms a port belongs to.
///
/// Natively it only says which string to read. In a tab it is also what the
/// chooser remembers while it is open. It is cross-platform so that
/// `App::port_handle`, the one seam that answers "which port", is one function
/// rather than a pair that could drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortField {
    /// The StarFive recovery form's port.
    Recovery,
    /// The serial console form's port.
    Console,
}

/// The application.
pub struct App {
    /// Everything the GUI knows.
    pub session: Session<Wire>,
    /// Everything somebody has typed into the USB form.
    pub form: Form,
    /// What has been chosen in the "Author a fresh table" form. It is
    /// cross-platform, as the recovery form is, because authoring is a gated write
    /// the web build carries too.
    pub author: AuthorForm,
    /// The port and medium typed into the StarFive recovery form.
    pub recover: RecoverForm,
    /// What has been typed into the serial console form, in the same flow as
    /// [`recover`](Self::recover).
    pub console: ConsoleForm,
    /// The stages, addresses, and settle time typed into the Ingenic boot-ROM
    /// bootstrap form.
    pub ingenic: IngenicForm,
    /// The bare 471/472 stage files chosen for a maskrom upload, where a container
    /// is not what is being uploaded. See [`MaskromForm`].
    pub maskrom: MaskromForm,
    /// Which flow the window is showing. See [`Tab`]: the window splits on what
    /// answers, the same rule the sections are grouped by.
    pub tab: Tab,
    /// Whether the partition-table tools (repair and authoring) are revealed.
    ///
    /// It is shut by default, the only part of the verb surface that is. Repairing
    /// a damaged table and authoring a fresh one are what a person comes to this
    /// window *for*, on a board that is already wrong. Neither is part of dumping,
    /// flashing or verifying a board that is fine. Drawn open, the tools measured
    /// 119 px of specialist controls in the middle of the everyday surface. That
    /// pushed the buttons a person came for off the screen.
    ///
    /// The serial recovery form, the console section and the Ingenic bootstrap
    /// form use the same "declared, not dumped" shape. The authoring half has its
    /// own reveal too, so this one puts repair and authoring behind a single door.
    pub show_table_tools: bool,
    /// Whether the serial recovery form has been revealed.
    ///
    /// The USB flows are discovered: a scan finds a board, and its screen
    /// appears. Serial recovery cannot be discovered, because a JH7110 in UART
    /// recovery is just a port, and nothing announces the board. It is therefore
    /// *declared*. It stays a single secondary action until a person asks for it,
    /// and only then does its form appear. It is set once a person opens the flow,
    /// and the session's recovery state holds it open from then on.
    pub show_recovery: bool,
    /// Whether the serial console section has been revealed.
    ///
    /// It is declared rather than discovered, as
    /// [`show_recovery`](Self::show_recovery) is, and for the same reason. A board
    /// with a console on a UART is just a port, and nothing on it announces the
    /// board. It is set once a person opens the section, and the session's console
    /// state holds it open from then on.
    pub show_console: bool,
    /// egui's context, kept so a job elsewhere can ask for a repaint.
    ctx: egui::Context,

    opening_target: Option<Slot<Result<FlashAgent<Wire>>>>,
    opening_source: Option<Slot<Result<FlashAgent<Wire>>>>,
    picking_image: Option<Slot<Picked<PickedImage>>>,
    picking_sink: Option<Slot<Picked<PickedSink>>>,

    /// A layout or block-text file being picked for a table authoring. It is held
    /// whole, like a recovery file rather than a streamed image, because a layout
    /// is small. It is cross-platform, because the web build offers authoring too.
    picking_layout: Option<Slot<Picked<PickedBlob>>>,

    /// A maskrom board's loader being picked and its transport opened, before the
    /// bootstrap job starts. `Ok(None)` is a file dialog somebody closed.
    bootstrapping: Option<Slot<BootstrapPick>>,

    /// A bare maskrom stage file being picked (471 or 472).
    picking_maskrom_stage: Option<Slot<Picked<PickedBlob>>>,

    /// Which raw stage the `picking_maskrom_stage` dialog is filling.
    picking_maskrom_which: MaskromStageKind,

    /// A recovery file being picked (the agent, the SPL, or the U-Boot payload).
    picking_recovery_file: Option<Slot<Picked<PickedBlob>>>,

    /// Which recovery file the `picking_recovery_file` dialog is filling.
    picking_recovery_which: RecoveryFileKind,

    /// A serial port being opened, and the work waiting on the far side of it.
    ///
    /// All three serial jobs (a recovery, a console session and a boot override)
    /// share this one path. The open is a task in both builds, and
    /// [`platform::open_serial_at`] says why. Three copies of the
    /// task-answer-then-start sequence would be three places for it to differ
    /// slightly. The [`PendingSerial`] says what the transport is *for*, and is
    /// held from the moment the open is requested until it answers.
    opening_serial: Option<Slot<Result<SerialWire>>>,

    /// What the port being opened is being opened for.
    ///
    /// It is set with [`opening_serial`](Self::opening_serial) and taken with it.
    /// While an open is in flight, its presence also makes a second click a no-op.
    /// The job does not exist yet, so `session.recovery.job` cannot answer that
    /// question on its own.
    pending_serial: Option<PendingSerial>,

    /// The port the recovery form names. Web only, because natively the form's
    /// own text *is* the handle, and there is nothing else to hold.
    #[cfg(target_arch = "wasm32")]
    recover_port: Option<platform::PortHandle>,

    /// The port the console form names. Web only, like
    /// [`recover_port`](Self::recover_port).
    #[cfg(target_arch = "wasm32")]
    console_port: Option<platform::PortHandle>,

    /// A serial port being chosen from the browser's chooser. Web only, because
    /// natively a port is typed, and typing needs no task to come back through.
    #[cfg(target_arch = "wasm32")]
    choosing_port: Option<Slot<Picked<platform::PortHandle>>>,

    /// The recovery's port being picked again, to confirm it. Web only, and the
    /// serial counterpart of [`repicking`](Self::repicking).
    #[cfg(target_arch = "wasm32")]
    repicking_port: Option<Slot<Result<bool>>>,

    /// Which form the `choosing_port` chooser is filling. Web only.
    #[cfg(target_arch = "wasm32")]
    choosing_port_for: PortField,

    /// An Ingenic bootstrap stage (the SPL or the U-Boot) being picked.
    ingenic_picking: Option<Slot<Picked<PickedBlob>>>,

    /// Which stage the `ingenic_picking` dialog is filling.
    ingenic_picking_which: IngenicStageKind,

    /// A boot-ROM board being opened for a bootstrap, before the job starts. The
    /// loader is already built from the form, and this task only opens the board.
    ingenic_bootstrapping: Option<Slot<IngenicBootstrapPick>>,

    /// A board being chosen from the browser's chooser. Web only, because
    /// natively the window draws its own list, and choosing from it is a click.
    #[cfg(target_arch = "wasm32")]
    choosing: Option<Slot<Picked<(platform::Handle, pyrographer_core::discovery::DeviceInfo)>>>,

    /// Which board the open chooser is choosing. Web only.
    #[cfg(target_arch = "wasm32")]
    choosing_for: Which,

    /// The destination being picked again, to confirm a write. Web only.
    #[cfg(target_arch = "wasm32")]
    repicking: Option<Slot<Result<bool>>>,

    /// The granted-board list being fetched. Web only. Natively the bus is polled
    /// on a timer, and the scan is a blocking call that needs no slot to come back
    /// through.
    #[cfg(target_arch = "wasm32")]
    listing: Option<Slot<Result<Vec<pyrographer_core::discovery::DeviceInfo>>>>,

    /// The range a dump will read, held while somebody chooses where to put it.
    ///
    /// It is captured at the button press, so what runs is what was on the screen
    /// at the moment of the request. Edits made to the form while the file dialog
    /// is open do not change it.
    dump_range: Option<(u64, u64)>,

    /// Whether the last scan of the bus failed. See [`rescan`](Self::rescan).
    ///
    /// Native only, because a browser has no bus to scan.
    #[cfg(not(target_arch = "wasm32"))]
    scan_failed: bool,

    /// Whether the last re-pick named something other than what the plan is for.
    ///
    /// The web build's whole write gate is one button. A chooser that returns a
    /// different board writes `false` into the confirmation and redraws the same
    /// button. Without this flag, a wrong pick and a click that did not register
    /// would look identical, and the refusal would be invisible. It is cleared as
    /// soon as another re-pick is requested, so it describes the last answer and
    /// not the session.
    ///
    /// It is cross-platform, so the field and the sentence that reads it are one
    /// piece of code. Behind a `cfg`, the desktop build would never compile them.
    /// Natively nothing sets it, because a wrong typed coordinate is visible on its
    /// own.
    repick_missed: bool,
}

impl App {
    /// The app, before it has seen a device.
    pub fn new(ctx: egui::Context) -> Self {
        // The look, installed once. Both platforms reach it here, so the window
        // and the tab are styled the same.
        crate::theme::install(&ctx);
        #[allow(unused_mut)]
        let mut app = Self {
            session: Session::new(CONFIRM_BY),
            form: Form::default(),
            author: AuthorForm::default(),
            recover: RecoverForm::default(),
            console: ConsoleForm::default(),
            ingenic: IngenicForm::default(),
            maskrom: MaskromForm::default(),
            tab: Tab::default(),
            show_table_tools: false,
            show_recovery: false,
            show_console: false,
            ctx,
            opening_target: None,
            opening_source: None,
            picking_image: None,
            picking_sink: None,
            picking_layout: None,
            bootstrapping: None,
            picking_maskrom_stage: None,
            picking_maskrom_which: MaskromStageKind::Code471,
            picking_recovery_file: None,
            picking_recovery_which: RecoveryFileKind::Agent,
            opening_serial: None,
            pending_serial: None,
            #[cfg(target_arch = "wasm32")]
            recover_port: None,
            #[cfg(target_arch = "wasm32")]
            console_port: None,
            #[cfg(target_arch = "wasm32")]
            choosing_port: None,
            #[cfg(target_arch = "wasm32")]
            repicking_port: None,
            #[cfg(target_arch = "wasm32")]
            choosing_port_for: PortField::Recovery,
            ingenic_picking: None,
            ingenic_picking_which: IngenicStageKind::Stage1,
            ingenic_bootstrapping: None,
            #[cfg(target_arch = "wasm32")]
            choosing: None,
            #[cfg(target_arch = "wasm32")]
            choosing_for: Which::Target,
            #[cfg(target_arch = "wasm32")]
            repicking: None,
            dump_range: None,
            #[cfg(not(target_arch = "wasm32"))]
            scan_failed: false,
            repick_missed: false,
            #[cfg(target_arch = "wasm32")]
            listing: None,
        };

        // The boards this origin was already granted, asked for once at startup.
        // It needs no user gesture, which is the whole reason a page can do this
        // and cannot open a chooser unbidden. Natively the bus is polled instead;
        // see `rescan`.
        #[cfg(target_arch = "wasm32")]
        app.relist();

        app
    }

    /// egui's frame clock, in seconds.
    ///
    /// **It is not `std::time::Instant::now()`, which panics on `wasm32`.** Core
    /// reports bytes and never reads a clock, because only the caller knows how it
    /// measures time. This is the caller's clock.
    pub fn now(&self) -> f64 {
        self.ctx.input(|input| input.time)
    }

    /// A closure a job calls to wake the frame loop.
    fn wake(&self) -> Wake {
        let ctx = self.ctx.clone();
        Box::new(move || ctx.request_repaint())
    }

    /// One board.
    pub fn board(&self, which: Which) -> &Board<Wire> {
        match which {
            Which::Target => &self.session.target,
            Which::Source => &self.session.source,
        }
    }

    /// Whether a board is being opened right now.
    pub fn is_opening(&self, which: Which) -> bool {
        match which {
            Which::Target => self.opening_target.is_some(),
            Which::Source => self.opening_source.is_some(),
        }
    }

    /// Whether the given board slot can be re-pointed at a device right now.
    ///
    /// **It is false while the board is opening or busy.** A board mid-open has an
    /// agent on its way to it. `collect_opened` installs whatever arrives into
    /// whichever board is selected by then. A busy board has its agent out on a
    /// job, which [`Session::harvest`](crate::state::Session::harvest) restores.
    /// Re-pointing either board while its agent is in flight lands one board's
    /// agent under another board's identity. The plan screen and the typed
    /// coordinate then name a board the write never reaches.
    ///
    /// [`Session::select_target`](crate::state::Session::select_target) refuses the
    /// busy case on its own. Whether an open is in flight is known to the frame
    /// loop, not the session, so this function gates the button that re-points a
    /// board.
    pub fn can_select(&self, which: Which) -> bool {
        !self.is_opening(which) && !self.board(which).connection.is_busy()
    }

    /// Whether a dialog is open.
    ///
    /// It includes the layout-file dialog, so the verbs are disabled while it is
    /// open, as they are for an image or a dump sink. A file dialog is off-frame,
    /// and a verb pressed behind one would run invisibly.
    pub fn is_picking(&self) -> bool {
        self.picking_image.is_some()
            || self.picking_sink.is_some()
            || self.picking_layout.is_some()
            || self.picking_maskrom_stage.is_some()
            // The recovery files and the bootstrap stages, which used to guard
            // only their own slot -- so an image dialog and a recovery-file dialog
            // could be up at once. Harmless, but the guards should be one rule:
            // a dialog is off-frame whichever form asked for it.
            || self.picking_recovery_file.is_some()
            || self.ingenic_picking.is_some()
    }

    /// Report a failure where the next frame will show it.
    fn failed(&mut self, error: Error) {
        self.session.last = Some(Err(error));
    }

    /// One turn of the frame loop's collection, for a test that has no frame loop.
    ///
    /// The tasks a job starts answer into slots, and [`collect`](Self::collect)
    /// drains them. A headless test drives real tasks: a serial open is a real
    /// open, against a port that cannot exist. The test therefore needs the same
    /// drain the window uses, without the window.
    #[cfg(test)]
    pub(crate) fn collect_for_test(&mut self, now: f64) {
        self.collect(now);
    }

    /// Collect what the off-frame tasks have finished, once a frame.
    fn collect(&mut self, now: f64) {
        self.collect_opened();
        self.collect_image();
        self.collect_sink(now);
        self.collect_layout_file();
        self.collect_maskrom_stage();
        self.collect_bootstrap(now);
        self.collect_recovery_file();
        self.collect_serial(now);

        self.collect_ingenic_stage();
        self.collect_ingenic_bootstrap(now);

        #[cfg(target_arch = "wasm32")]
        {
            self.collect_chosen();
            self.collect_repick();
            self.collect_listed(now);
            self.collect_chosen_port();
            self.collect_repick_port();
        }

        self.session.harvest();
        self.session.harvest_bootstrap();
        self.session.harvest_ingenic_bootstrap();
        self.session.harvest_recovery();
        self.session.harvest_console();
    }

    /// Boards that finished opening.
    ///
    /// An open that failed is reported like any other failure. A missing udev rule
    /// surfaces here, and the hint core attaches to it says how to fix it.
    fn collect_opened(&mut self) {
        for which in [Which::Target, Which::Source] {
            let slot = match which {
                Which::Target => &self.opening_target,
                Which::Source => &self.opening_source,
            };
            let Some(opened) = slot.as_ref().and_then(|slot| lock(slot).take()) else {
                continue;
            };

            match which {
                Which::Target => self.opening_target = None,
                Which::Source => self.opening_source = None,
            }

            match opened {
                Ok(agent) => match which {
                    Which::Target => self.session.target.opened(agent),
                    Which::Source => self.session.source.opened(agent),
                },
                Err(error) => self.failed(error),
            }
        }
    }

    /// An image somebody picked. Picking one discards any plan made against the
    /// last one. See [`Session::set_image`].
    fn collect_image(&mut self) {
        let Some(picked) = self
            .picking_image
            .as_ref()
            .and_then(|slot| lock(slot).take())
        else {
            return;
        };
        self.picking_image = None;

        match picked {
            Ok(Some(image)) => self.session.set_image(Some(image)),
            Ok(None) => {}
            Err(error) => self.failed(error),
        }
    }

    /// A layout or block-text file somebody picked for a table authoring.
    ///
    /// A dialog somebody closed leaves the file as it was. The "Forget" button
    /// clears it by setting it to `None`, as for a recovery file.
    fn collect_layout_file(&mut self) {
        let Some(picked) = self
            .picking_layout
            .as_ref()
            .and_then(|slot| lock(slot).take())
        else {
            return;
        };
        self.picking_layout = None;

        match picked {
            Ok(Some(blob)) => self.author.file = Some(blob),
            Ok(None) => {}
            Err(error) => self.failed(error),
        }
    }

    /// Somewhere to put a dump. The dump starts as soon as there is one, because
    /// asking for a destination was asking for the dump.
    fn collect_sink(&mut self, now: f64) {
        let Some(picked) = self
            .picking_sink
            .as_ref()
            .and_then(|slot| lock(slot).take())
        else {
            return;
        };
        self.picking_sink = None;
        let range = self.dump_range.take();

        match picked {
            Ok(Some(sink)) => {
                let Some((lba, sectors)) = range else { return };
                let name = sink.name.clone();
                // **A refused start owes a different sentence here.** Asking for
                // the sink has already created the file, truncating whatever was
                // at that path -- so "the board is busy" explains why nothing was
                // read and leaves the zero-byte file it left behind unexplained.
                if !self.run(
                    Task::Dump {
                        lba,
                        sectors,
                        sink: sink.writer,
                        name: sink.name,
                    },
                    now,
                ) {
                    self.failed(Error::InvalidRequest(format!(
                        "the dump did not start, because the device is busy with another job. \
                         {name} was created and left empty. Wait for the job to finish, then dump \
                         again."
                    )));
                }
            }
            Ok(None) => {}
            Err(error) => self.failed(error),
        }
    }

    /// A maskrom loader that has been picked and its transport opened: start the
    /// bootstrap. `Ok(None)` is a file dialog somebody closed, and nothing starts.
    fn collect_bootstrap(&mut self, now: f64) {
        let Some(result) = self
            .bootstrapping
            .as_ref()
            .and_then(|slot| lock(slot).take())
        else {
            return;
        };
        self.bootstrapping = None;

        match result {
            Ok(Some((transport, loader))) => {
                // The SoC field the write surface already collects, read here for
                // the loader-file gate. An unpinned name is a usage problem and is
                // reported as one -- the same answer the write path gives it -- and
                // the opened transport is dropped rather than uploaded through.
                let soc = match self.named_soc() {
                    Ok(soc) => soc,
                    Err(error) => return self.failed(error),
                };
                // Refused before the job exists, so nothing spins up to be torn
                // down and the person reads one sentence about the file they
                // picked rather than a failed upload.
                if let Some(refusal) = pyrographer_core::verbs::loader_blob_refusal(soc, &loader) {
                    return self.failed(refusal);
                }
                let wake = self.wake();
                // The board the transport was opened on, carried into the job so
                // the harvest can tell whether the slot still holds it. See
                // `Session::harvest_bootstrap`.
                let board = self.session.target.handle.clone();
                match self
                    .session
                    .start_bootstrap(transport, loader, soc, board, now, wake)
                {
                    Some(work) => platform::spawn(move || work.run()),
                    // The transport was opened for this upload; if a job started
                    // underneath the file dialog, say so rather than dropping the
                    // opened maskrom device on the floor without a word.
                    None => self.failed(Error::InvalidRequest(
                        "a job is already running. Wait for it to finish, then upload the loader \
                         again"
                            .to_string(),
                    )),
                }
            }
            Ok(None) => {}
            Err(error) => self.failed(error),
        }
    }

    /// If it is time and nothing is running, rescan the bus.
    ///
    /// Native only. A browser cannot scan a bus. It is handed the one device
    /// somebody picked, so there is nothing to poll.
    #[cfg(not(target_arch = "wasm32"))]
    fn rescan(&mut self, now: f64) {
        if !self.session.wants_rescan(now) {
            return;
        }

        match platform::list() {
            Ok(devices) => {
                self.scan_failed = false;
                self.session.devices_seen(devices, now);
            }
            Err(error) => {
                self.session.devices_seen(Vec::new(), now);

                // Reported once, and not once a second for as long as it goes on
                // failing. A bus that cannot be scanned will go on not being
                // scannable, and a report that rewrote itself every second would
                // bury whatever the last job said under a failure the person has
                // already read. The scan keeps trying, so it heals on its own.
                if !self.scan_failed {
                    self.scan_failed = true;
                    self.failed(error);
                }
            }
        }
    }

    /// Open whatever is in the slot: a board on the bus, or a disk.
    ///
    /// **A disk is opened on the frame thread, by design.** A USB open runs on a
    /// task, because it waits for a kernel driver to let go of an interface.
    /// Opening a file is a syscall that returns. A disk also has no counterpart in
    /// the other build to stay uniform with, because a tab has no Block backend.
    /// The cost is a moment of `/sys` reads and one `open(2)`.
    pub fn open(&mut self, which: Which) {
        if self.is_opening(which) {
            return;
        }
        match self.board(which).device.clone() {
            Some(Chosen::Block(disk)) => self.open_disk(which, &disk),
            _ => self.open_board(which),
        }
    }

    /// Open a board on the bus, on a task of its own.
    fn open_board(&mut self, which: Which) {
        let Some(handle) = self.board(which).handle.clone() else {
            return;
        };

        let slot: Slot<Result<FlashAgent<Wire>>> = share(None);
        match which {
            Which::Target => self.opening_target = Some(slot.clone()),
            Which::Source => self.opening_source = Some(slot.clone()),
        }

        let wake = self.wake();
        platform::spawn_answering("opening the board", &slot, wake, move || async move {
            platform::open(&handle).await
        });
    }

    /// Open a disk, exclusively and uncached, and hold it until it is closed.
    ///
    /// Each refusal arrives here as its own error: missing privilege, a device
    /// something else holds, or a disk the running system rests on. The kernel makes
    /// the first two, and the last is core's own check. They are reported
    /// separately, because each means something different to the person holding the
    /// card.
    fn open_disk(&mut self, which: Which, disk: &BlockDevice) {
        match platform::blocks::open(disk) {
            Ok(agent) => {
                // **The slot takes the description the open actually read.**
                // `block::open` lists again at the moment of the open and refuses
                // a device that is not the one described, so the agent's copy is
                // the fresh one and the snapshot somebody clicked minutes ago is
                // not. Without this the panel draws a disk that has been unmounted
                // as still mounted -- next to the sentence saying it is now held
                // exclusively -- and the plan screen renders the running-system
                // row off a struct the caller filled in, which is the one thing
                // `still_the_same_device` exists to say is not the authority.
                if let FlashAgent::Block(block) = &agent {
                    let fresh = Chosen::Block(block.device().clone());
                    match which {
                        Which::Target => self.session.target.device = Some(fresh),
                        Which::Source => self.session.source.device = Some(fresh),
                    }
                }
                match which {
                    Which::Target => self.session.target.opened(agent),
                    Which::Source => self.session.source.opened(agent),
                }
            }
            Err(error) => self.failed(error),
        }
    }

    /// Open or close the disk section, and list the machine's disks on opening.
    ///
    /// **The listing is made here, not every frame.** The bus is polled at rest,
    /// so that a board plugged in appears on its own. A disk list is a walk of
    /// `/sys/block` per disk, and it rarely changes. A list of destructive targets
    /// must not reshuffle under the mouse. The list is therefore taken as the
    /// section opens, and again on request.
    pub fn show_disks(&mut self, show: bool) {
        self.session.disks.show = show;
        if show && !self.session.disks.asked {
            self.list_disks();
        }
    }

    /// List the machine's disks, whatever the answer is.
    ///
    /// It is synchronous, and needs no privilege. Everything a row shows comes
    /// from `/sys` and `/proc`, with nothing opened:
    ///
    /// - Capacity
    /// - Both sector sizes
    /// - The bus
    /// - The mounts
    /// - Whether the running system rests on the disk
    ///
    /// Linux answers all of this without privilege, so elevation is needed one
    /// step later, at the open.
    pub fn list_disks(&mut self) {
        let listed = platform::blocks::list();
        self.session.disks_seen(listed);
    }

    /// Whether the last re-pick named something other than what the plan is for.
    ///
    /// See [`repick_missed`](Self::repick_missed). Always `false` natively, where
    /// the confirmation is a typed coordinate, and a wrong one is visible on its
    /// own.
    pub fn repick_missed(&self) -> bool {
        self.repick_missed
    }

    /// Stop copying from the device in the source slot, and close it.
    ///
    /// *Clone from* on any row fills the slot, and this function empties it.
    /// Without it, the panel and the board it holds open would stay for the rest
    /// of the session.
    pub fn forget_source(&mut self) {
        self.session.forget_source();
    }

    /// Close a board, and forget what it said.
    pub fn close(&mut self, which: Which) {
        match which {
            Which::Target => self.session.target.close(),
            Which::Source => self.session.source.close(),
        }
        self.session.dismiss_plan();
    }

    /// Ask for an image to write, or to compare the flash against.
    pub fn pick_image(&mut self) {
        if self.is_picking() {
            return;
        }
        let slot: Slot<Picked<PickedImage>> = share(None);
        self.picking_image = Some(slot.clone());

        let wake = self.wake();
        platform::spawn_answering("picking an image", &slot, wake, platform::ask_for_image);
    }

    /// Forget the image, and with it any plan made against it.
    pub fn forget_image(&mut self) {
        self.session.set_image(None);
    }

    /// Ask for a layout or block-text file to author a fresh table from.
    ///
    /// It reuses the cross-platform blob picker, because a layout is a few lines of
    /// text, read whole rather than streamed. What it reads goes into
    /// [`AuthorForm::file`]. Unlike a picked image, the file is not coupled to any
    /// plan: authoring bakes its bytes into the plan at planning time.
    pub fn pick_layout_file(&mut self) {
        if self.picking_layout.is_some() {
            return;
        }
        let slot: Slot<Picked<PickedBlob>> = share(None);
        self.picking_layout = Some(slot.clone());

        let wake = self.wake();
        platform::spawn_answering("picking a layout file", &slot, wake, platform::ask_for_blob);
    }

    /// Plan authoring a fresh table from the author form: the dry run.
    ///
    /// The layout file's bytes are decoded to text here and handed to the job as a
    /// [`LayoutSource`]. The parse into a
    /// [`Layout`](pyrographer_core::layout::Layout) happens on the job thread, where
    /// the device's sector count is in reach. A missing file, or one that is not
    /// UTF-8 text, is reported like any other failure, and no plan screen appears.
    /// The GPT form never offers a verbatim-text source, so that combination cannot
    /// arise here.
    pub fn plan_author(&mut self, now: f64) {
        let Some(file) = self.author.file.as_ref() else {
            self.failed(Error::InvalidRequest(
                "choose a layout file to author the table from first".to_string(),
            ));
            return;
        };
        let text = match String::from_utf8(file.bytes.clone()) {
            Ok(text) => text,
            Err(_) => {
                self.failed(Error::InvalidRequest(
                    "the layout file is not valid UTF-8 text".to_string(),
                ));
                return;
            }
        };

        let source = match self.author.source {
            AuthorSourceKind::Native => LayoutSource::Native(text),
            AuthorSourceKind::Mtdparts => LayoutSource::Mtdparts(text),
            AuthorSourceKind::Text => LayoutSource::Text(text),
        };
        let soc = self.planned_soc();

        let task = match self.author.format {
            TableFormat::Gpt => Task::PlanAuthorGpt { source, soc },
            TableFormat::RockchipParam => Task::PlanAuthorParam {
                source,
                medium: self.author.medium,
                soc,
            },
            // The authoring form offers only GPT and Rockchip parameter (see the
            // radio in `ui::author_form`), so this is unreachable today. Refused
            // rather than `unreachable!`: this is a button handler on the frame
            // thread, and a panic there takes the window down. A DFU board's table
            // is its alt-settings, which the device owns -- there is nothing to
            // author onto the flash -- so the refusal is also the true answer if a
            // third format is ever added to the radio.
            TableFormat::DfuAltInfo => {
                self.failed(Error::InvalidRequest(
                    "a DFU board's partition table is its alt-settings, which the device owns. \
                     There is no table to author onto its flash"
                        .to_string(),
                ));
                return;
            }
        };
        self.run(task, now);
    }

    /// Whether a maskrom loader is being picked, or its board opened, or uploaded.
    pub fn is_bootstrapping(&self) -> bool {
        self.bootstrapping.is_some() || self.session.bootstrap.is_some()
    }

    /// Pick a loader for the target board and upload it, bringing a maskrom board
    /// to loader mode.
    ///
    /// One task does both the pick and the open, in that order. The file dialog
    /// needs the user gesture that pressed the button, and the maskrom device is
    /// opened only once there is a loader to upload. The bootstrap job itself
    /// starts in `collect_bootstrap`, once both are in hand.
    pub fn upload_loader(&mut self) {
        if self.is_bootstrapping() || self.session.job.is_some() {
            return;
        }
        let Some(handle) = self.session.target.handle.clone() else {
            return;
        };

        let slot: Slot<BootstrapPick> = share(None);
        self.bootstrapping = Some(slot.clone());

        let wake = self.wake();
        platform::spawn_answering(
            "picking a loader and opening the board",
            &slot,
            wake,
            move || async move {
                let Some(picked) = platform::ask_for_loader().await? else {
                    return Ok(None);
                };
                let transport = platform::open_maskrom(&handle).await?;
                Ok(Some((transport, picked.loader)))
            },
        );
    }

    /// Pick one of the bare maskrom stages: the 471, or the 472.
    pub fn pick_maskrom_stage(&mut self, which: MaskromStageKind) {
        if self.is_picking() {
            return;
        }
        // Remembered until the dialog answers, because one dialog fills both.
        self.picking_maskrom_which = which;

        let slot: Slot<Picked<PickedBlob>> = share(None);
        self.picking_maskrom_stage = Some(slot.clone());

        let wake = self.wake();
        platform::spawn_answering(
            "picking a maskrom stage file",
            &slot,
            wake,
            platform::ask_for_blob,
        );
    }

    /// A raw maskrom stage somebody picked.
    fn collect_maskrom_stage(&mut self) {
        let Some(picked) = self
            .picking_maskrom_stage
            .as_ref()
            .and_then(|slot| lock(slot).take())
        else {
            return;
        };
        self.picking_maskrom_stage = None;

        let blob = match picked {
            Ok(Some(blob)) => Some(blob),
            // A dialog somebody closed leaves the file as it was; "Forget" is what
            // clears one, the same shape every other file row uses.
            Ok(None) => return,
            Err(error) => {
                self.failed(error);
                return;
            }
        };

        match self.picking_maskrom_which {
            MaskromStageKind::Code471 => self.maskrom.code_471 = blob,
            MaskromStageKind::Code472 => self.maskrom.code_472 = blob,
        }
    }

    /// Upload bare 471/472 stages into a maskrom board, in place of a container.
    ///
    /// The window's form of `db --code471/--code472`. It joins the container path
    /// at the earliest point the two can merge. [`LoaderImage::from_raw`] builds
    /// the same value [`rkboot::parse`] does, so from there on there is one
    /// bootstrap, one gate and one job. Unlike [`upload_loader`](Self::upload_loader),
    /// the files are already in hand, picked into the form, so this task only
    /// opens the board.
    ///
    /// [`LoaderImage::from_raw`]: pyrographer_core::codec::rkboot::LoaderImage::from_raw
    /// [`rkboot::parse`]: pyrographer_core::codec::rkboot::parse
    pub fn upload_raw_stages(&mut self) {
        if self.is_bootstrapping() || self.session.job.is_some() {
            return;
        }
        let Some(handle) = self.session.target.handle.clone() else {
            return;
        };
        let named =
            |blob: Option<&PickedBlob>| blob.map(|blob| (blob.name.clone(), blob.bytes.clone()));
        let code_471 = named(self.maskrom.code_471.as_ref());
        let code_472 = named(self.maskrom.code_472.as_ref());
        if code_471.is_none() && code_472.is_none() {
            self.failed(Error::InvalidRequest(
                "choose at least one stage to upload: a 471, a 472, or both".to_string(),
            ));
            return;
        }
        let loader = LoaderImage::from_raw(code_471, code_472);

        let slot: Slot<BootstrapPick> = share(None);
        self.bootstrapping = Some(slot.clone());

        let wake = self.wake();
        platform::spawn_answering(
            "opening the board for a raw stage upload",
            &slot,
            wake,
            move || async move {
                let transport = platform::open_maskrom(&handle).await?;
                Ok(Some((transport, loader)))
            },
        );
    }

    /// Start a job, provided the boards it needs are free.
    ///
    /// **A refused start is reported, not dropped.** [`Session::start`] returns
    /// `None` when a board it needs is busy, closed, or finished. The task it was
    /// handed goes with it, and that task can be a minted `ConfirmedWrite`. A
    /// silent refusal would make a confirmation a person just typed vanish without
    /// a word. A refused start therefore reports a busy error.
    pub fn run(&mut self, task: Task, now: f64) -> bool {
        let wake = self.wake();
        match self.session.start(task, now, wake) {
            Some(work) => {
                platform::spawn(move || work.run());
                true
            }
            None => {
                self.failed(Error::InvalidRequest(
                    "the board is busy with another job. Wait for it to finish, then try again"
                        .to_string(),
                ));
                false
            }
        }
    }

    /// Ask where to put a dump of `sectors` sectors from `lba`, and dump there.
    pub fn dump(&mut self, lba: u64, sectors: u64, suggested: String) {
        if self.is_picking() {
            return;
        }
        let slot: Slot<Picked<PickedSink>> = share(None);
        self.picking_sink = Some(slot.clone());
        self.dump_range = Some((lba, sectors));

        let wake = self.wake();
        platform::spawn_answering(
            "choosing where to put the dump",
            &slot,
            wake,
            move || async move { platform::ask_for_sink(suggested).await },
        );
    }

    /// Where the form says a write or a verify is aimed.
    pub fn aim(&self) -> Option<Aim> {
        if self.form.by_name {
            let name = self.form.partition.trim();
            if name.is_empty() {
                return None;
            }
            return Some(Aim::Partition(name.to_string()));
        }
        self.form.lba.trim().parse().ok().map(Aim::Lba)
    }

    /// The SoC the form names, parsed, or `Ok(None)` for an empty field.
    ///
    /// The parse is [`Soc::parse`], which refuses names with no pinned chipver
    /// reply. An `Err` here is therefore a name the wrong-loader gate cannot
    /// compare against. It belongs next to the field, not first on the plan screen.
    pub fn named_soc(&self) -> Result<Option<Soc>> {
        let text = self.form.soc.trim();
        if text.is_empty() {
            return Ok(None);
        }
        Soc::parse(text).map(Some)
    }

    /// Whether the target slot holds a disk rather than a board.
    pub fn target_is_disk(&self) -> bool {
        matches!(self.session.target.device, Some(Chosen::Block(_)))
    }

    /// The SoC the wrong-loader gate compares against, for whatever is in the
    /// target slot.
    ///
    /// **Always `Ok(None)` for a disk.** The CLI refuses `--soc` on a block write
    /// rather than ignoring it. A flag quietly dropped leaves a person believing in
    /// a gate that is not there. The window's equivalent is not to ask for a SoC at
    /// all. This function also ensures that a name left over from the board open a
    /// moment ago cannot reach a disk's plan unseen.
    ///
    /// Core would ignore the name either way, because `loader_refusal` returns
    /// early on a `FlashAgent::Block`. A value that travels only to be ignored will
    /// eventually be read as meaning something.
    pub fn gate_soc(&self) -> Result<Option<Soc>> {
        if self.target_is_disk() {
            return Ok(None);
        }
        self.named_soc()
    }

    /// The SoC to put on a plan, with an unparseable name treated as none named.
    ///
    /// **The discard is deliberate.** [`gate_soc`](Self::gate_soc) refuses a name
    /// the gate has no pinned reply for. `ui::actions` draws that refusal beside
    /// the field as soon as the name is typed, so the error is already on the
    /// screen. A plan built here is refused at the gate as a plan naming no SoC.
    /// That is the accurate reading of a name the gate cannot use. The field and
    /// the plan cannot disagree, because both come from `gate_soc`.
    ///
    /// `collect_bootstrap` treats the same parse failure as a *reportable* error
    /// instead. That is consistent: the act began in a file dialog, so no field on
    /// the screen has shown the error.
    pub fn planned_soc(&self) -> Option<Soc> {
        self.gate_soc().ok().flatten()
    }

    /// Confirm the plan on the screen.
    ///
    /// This is the only place in the GUI a [`ConfirmedWrite`] is minted. It is
    /// reachable only through a plan somebody read and an act they performed.
    /// [`Session::confirm`] checks the act. If the person has not performed it,
    /// the plan stays on the screen.
    ///
    /// [`ConfirmedWrite`]: pyrographer_core::verbs::ConfirmedWrite
    pub fn confirm(&mut self, now: f64) {
        let Some(confirmed) = self.session.confirm() else {
            return;
        };

        match confirmed {
            Confirmed::Write(confirmed) => {
                let Some(image) = self.session.image.as_ref() else {
                    // The image went away between the plan and the yes, which
                    // `set_image` is supposed to have made impossible.
                    self.failed(Error::InvalidRequest(
                        "there is no image to write. Choose one and plan again".to_string(),
                    ));
                    return;
                };

                match image.reader() {
                    Ok(reader) => {
                        self.run(
                            Task::Write {
                                confirmed,
                                image: reader,
                            },
                            now,
                        );
                    }
                    Err(error) => self.failed(error),
                }
            }

            Confirmed::Clone(confirmed) => {
                self.run(Task::Clone { confirmed }, now);
            }

            // A table write carries its own bytes -- the rebuilt copies, or the
            // authored table -- so unlike an image write it needs no image
            // alongside the confirmation.
            Confirmed::Table(confirmed) => {
                self.run(Task::WriteTable { confirmed }, now);
            }
        }
    }
}

/// The two serial flows' acts: a StarFive recovery, and a bootloader console.
impl App {
    /// The port one of the two serial forms names, ready to be opened.
    ///
    /// This is the one place a caller meets the acquisition seam. Natively the
    /// form's text *is* the handle, so this function trims it and refuses the
    /// empty string.
    #[cfg(not(target_arch = "wasm32"))]
    fn port_handle(&self, which: PortField) -> Result<platform::PortHandle> {
        let (typed, what) = match which {
            PortField::Recovery => (&self.recover.port, "the board is on"),
            PortField::Console => (&self.console.port, "the board's console is on"),
        };
        let port = typed.trim().to_string();
        if port.is_empty() {
            return Err(Error::InvalidRequest(format!(
                "name the serial port {what}, for example /dev/ttyUSB0"
            )));
        }
        Ok(port)
    }

    /// The port one of the two serial forms names: the object a chooser handed
    /// back.
    ///
    /// In a tab there is nothing to type, and no field to type it in.
    #[cfg(target_arch = "wasm32")]
    fn port_handle(&self, which: PortField) -> Result<platform::PortHandle> {
        let (held, what) = match which {
            PortField::Recovery => (&self.recover_port, "the board is on"),
            PortField::Console => (&self.console_port, "the board's console is on"),
        };
        held.clone().ok_or_else(|| {
            Error::InvalidRequest(format!(
                "choose the serial port {what}. A browser does not accept a port path. Pick \
                 the adapter the board is connected to from the ports it lists"
            ))
        })
    }

    /// The handle for a port a plan already named, rather than for whatever the
    /// form says now.
    ///
    /// **The port a person confirmed is the port that gets opened.** A plan
    /// remembers its port, and nothing stops a person editing the form between
    /// confirming the plan and the job running. A recovery and a boot override
    /// therefore reach their line through this function. A session with no plan
    /// before it reaches its line through [`port_handle`](Self::port_handle).
    /// Natively the remembered string *is* the handle, so this function returns it.
    #[cfg(not(target_arch = "wasm32"))]
    fn planned_port(&self, _which: PortField, planned: String) -> Result<platform::PortHandle> {
        Ok(planned)
    }

    /// The handle for a port a plan already named.
    ///
    /// In a tab a plan remembers a *name*, which cannot be opened, so the handle
    /// held for that form answers instead. The name and the handle cannot drift
    /// apart. Choosing a different port discards the plans made against the old
    /// one. That is the web build's equivalent of the native gate re-reading a
    /// string nobody can change without invalidating what they typed.
    #[cfg(target_arch = "wasm32")]
    fn planned_port(&self, which: PortField, _planned: String) -> Result<platform::PortHandle> {
        self.port_handle(which)
    }

    /// Whether a serial port is being opened for a job that has not started yet.
    ///
    /// This is the gap the two "busy" questions, [`is_recovering`](Self::is_recovering)
    /// and [`is_console_busy`](Self::is_console_busy), have to cover. Between the
    /// open going out and the transport coming back, there is no job to point at.
    /// A second click in that interval would open the port twice.
    pub fn is_opening_port(&self) -> bool {
        self.opening_serial.is_some()
    }

    /// Open a port on a task, and remember what it is being opened for.
    ///
    /// **This is the one place a serial job reaches a line.** The open is a task in
    /// both builds: natively a future that is ready on its first poll, and in a
    /// tab a promise. All three flows therefore come through here, and the shape
    /// is the same in both builds.
    fn open_port(&mut self, port: platform::PortHandle, baud: u32, pending: PendingSerial) {
        // **A refused open is reported, not dropped.** Both callers that reach
        // here with a `ConfirmedRecovery` or a `ConfirmedBoot` have already
        // consumed the plan that minted it, so returning quietly would take a
        // confirmation somebody just typed off the screen with no word at all --
        // the rule [`run`](Self::run) states, applied to the seam that has the
        // most to lose by breaking it. Narrow natively, where the open is ready on
        // its first poll; genuinely reachable in a tab, where it is a promise.
        if self.is_opening_port() {
            self.failed(Error::InvalidRequest(format!(
                "a serial port is already being opened for {}. Wait for that to finish, then \
                 confirm again",
                pending.what()
            )));
            return;
        }
        let slot: Slot<Result<SerialWire>> = share(None);
        self.opening_serial = Some(slot.clone());
        let what = pending.what();
        self.pending_serial = Some(pending);

        let wake = self.wake();
        platform::spawn_answering(what, &slot, wake, move || async move {
            platform::open_serial_at(port, baud).await
        });
    }

    /// Start whichever job was waiting on a port, once the port is open.
    ///
    /// An open that failed is reported like any other failure. The work it was for
    /// is dropped with its confirmation, as a write whose image will not open loses
    /// its plan. A person who still wants the work confirms again.
    fn collect_serial(&mut self, now: f64) {
        let Some(opened) = self
            .opening_serial
            .as_ref()
            .and_then(|slot| lock(slot).take())
        else {
            return;
        };
        self.opening_serial = None;
        let Some(pending) = self.pending_serial.take() else {
            return;
        };

        let serial = match opened {
            Ok(serial) => serial,
            Err(error) => {
                self.failed(error);
                return;
            }
        };

        let wake = self.wake();
        // The same rule, at the other end: a start refused because something is
        // already running would otherwise drop the opened transport *and* the
        // confirmation it was opened for, silently. `collect_bootstrap` reports
        // its own refusal, and these two now say the same thing.
        let started = match pending {
            PendingSerial::Recovery { request, confirmed } => {
                match self
                    .session
                    .start_recovery(serial, request, confirmed, now, wake)
                {
                    Some(work) => {
                        platform::spawn(move || work.run());
                        true
                    }
                    None => false,
                }
            }
            PendingSerial::Console { line, task } => {
                match self.session.start_console(serial, &line, task, now, wake) {
                    Some(work) => {
                        platform::spawn(move || work.run());
                        true
                    }
                    None => false,
                }
            }
        };
        if !started {
            self.failed(Error::InvalidRequest(
                "a serial job is already running. Wait for it to finish, then confirm again"
                    .to_string(),
            ));
        }
    }

    /// Whether a recovery file dialog is open.
    ///
    /// It applies the whole rule, not just this form's slot. See
    /// [`is_picking`](Self::is_picking).
    pub fn is_picking_recovery_file(&self) -> bool {
        self.is_picking()
    }

    /// Whether a recovery is running, or its port is still being opened.
    pub fn is_recovering(&self) -> bool {
        self.session.recovery.job.is_some()
            || matches!(self.pending_serial, Some(PendingSerial::Recovery { .. }))
    }

    /// Pick one of a recovery's files: the agent, the SPL, or the U-Boot payload.
    pub fn pick_recovery_file(&mut self, which: RecoveryFileKind) {
        if self.is_picking_recovery_file() {
            return;
        }
        // Remembered until the dialog answers, because one dialog fills all three.
        self.picking_recovery_which = which;

        let slot: Slot<Picked<PickedBlob>> = share(None);
        self.picking_recovery_file = Some(slot.clone());

        let wake = self.wake();
        platform::spawn_answering(
            "picking a recovery file",
            &slot,
            wake,
            platform::ask_for_blob,
        );
    }

    /// A recovery file somebody picked. Picking one discards any recovery plan
    /// made against the old files. See [`Session::set_recovery_agent`].
    ///
    /// [`Session::set_recovery_agent`]: crate::state::Session::set_recovery_agent
    fn collect_recovery_file(&mut self) {
        let Some(picked) = self
            .picking_recovery_file
            .as_ref()
            .and_then(|slot| lock(slot).take())
        else {
            return;
        };
        self.picking_recovery_file = None;

        let blob = match picked {
            Ok(Some(blob)) => Some(blob),
            // A dialog somebody closed leaves the file as it was: to clear one, the
            // "Forget" button sets it to `None` explicitly.
            Ok(None) => return,
            Err(error) => {
                self.failed(error);
                return;
            }
        };

        match self.picking_recovery_which {
            RecoveryFileKind::Agent => self.session.set_recovery_agent(blob),
            RecoveryFileKind::Spl => self.session.set_recovery_spl(blob),
            RecoveryFileKind::Uboot => self.session.set_recovery_uboot(blob),
        }
    }

    /// Plan a StarFive recovery from the form and the picked files.
    ///
    /// It is synchronous, because the plan touches no serial line: there is nothing
    /// to ask a receiver in recovery. A refusal (no port, no agent, or nothing to
    /// write) is shown like any other failure, and no plan screen appears.
    pub fn plan_recovery(&mut self) {
        // **Not while a write plan is waiting.** Two gates can be pending at once
        // and only one is drawn -- the write gate wins -- so a recovery planned
        // underneath one is a screen nobody can see, and dismissing the first
        // would drop a person into the second as though the same screen had
        // changed under them. Refused here, where the reason can be said.
        if self.session.pending.is_some() {
            self.failed(Error::InvalidRequest(
                "a write plan is waiting to be answered. Confirm or cancel it first, because \
                 only one plan can be on the screen at a time."
                    .to_string(),
            ));
            return;
        }
        // Asked for here rather than at the open, so a form naming no port is
        // refused before a plan screen appears rather than after one is agreed to.
        if let Err(error) = self.port_handle(PortField::Recovery) {
            self.failed(error);
            return;
        }
        let port = self.recover.port.clone();
        let target = self.recover.target;
        if let Err(error) = self.session.plan_recovery(port, target) {
            self.failed(error);
        }
    }

    /// Confirm the recovery plan, and run it.
    ///
    /// This is the only place a [`ConfirmedRecovery`] is minted. Confirming
    /// consumes the plan. The port is then opened on a task, and the job starts in
    /// `collect_serial` once the open answers. An open that fails is reported, and
    /// the plan is already spent, as it is for a write whose image will not open.
    ///
    /// The port opened is the one the *seam* names, not the string the plan
    /// carries. In a tab those differ: the string is a label, and the seam's handle
    /// is an object. Only the object can be opened.
    ///
    /// [`ConfirmedRecovery`]: pyrographer_core::recovery::ConfirmedRecovery
    pub fn recover(&mut self, _now: f64) {
        let Some((planned, request, confirmed)) = self.session.confirm_recovery() else {
            return;
        };
        let port = match self.planned_port(PortField::Recovery, planned) {
            Ok(port) => port,
            Err(error) => {
                self.failed(error);
                return;
            }
        };

        self.open_port(
            port,
            pyrographer_core::transport::DEFAULT_BAUD,
            PendingSerial::Recovery { request, confirmed },
        );
    }

    /// Whether a console session is running, or its port is still being opened.
    pub fn is_console_busy(&self) -> bool {
        self.session.console.job.is_some()
            || matches!(self.pending_serial, Some(PendingSerial::Console { .. }))
    }

    /// The serial line the console form names, as a session runs over it.
    ///
    /// It is read from the form once, at the start of a session, so what runs is
    /// what was asked for. It refuses a budget of zero reads, one of the two ways a
    /// form can name a session that could never end. The other, a port the seam
    /// will not give, is checked by [`port_handle`](Self::port_handle), which
    /// [`start_console`](Self::start_console) calls alongside this. The `port` here
    /// is the port's *name*, which the transcript and a boot plan quote.
    fn console_line(&self) -> Result<ConsoleLine> {
        if self.console.reads == 0 {
            return Err(Error::InvalidRequest(
                "reads per wait must be at least 1, or the session cannot receive any output"
                    .to_string(),
            ));
        }
        Ok(ConsoleLine {
            port: self.console.port.trim().to_string(),
            baud: self.console.baud,
            prompt: self.console.prompt.clone(),
            reads: self.console.reads,
        })
    }

    /// Read a box of patterns, one per line, into the bytes they name.
    ///
    /// Blank lines are skipped, so trailing newlines in a text box are not empty
    /// patterns. An escape that names nothing is refused where it was typed, rather
    /// than becoming a pattern that can never match.
    fn console_patterns(typed: &str) -> Result<Vec<Vec<u8>>> {
        typed
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(console::pattern)
            .collect()
    }

    /// Open the port and run one console task on a task of its own.
    ///
    /// This is the one place a console session starts. The open runs on a task,
    /// and the session starts in [`collect_serial`](Self::collect_serial) once the
    /// open answers. An open that fails is reported like any other failure.
    fn start_console(&mut self, task: ConsoleTask, _now: f64) {
        if self.is_console_busy() || self.is_opening_port() {
            return;
        }
        let (line, port) = match self.console_line().and_then(|line| {
            let port = self.port_handle(PortField::Console)?;
            Ok((line, port))
        }) {
            Ok(both) => both,
            Err(error) => {
                self.failed(error);
                return;
            }
        };
        let baud = line.baud;

        self.open_port(port, baud, PendingSerial::Console { line, task });
    }

    /// Watch the console for the text that says a board worked, or that it did not.
    ///
    /// The passive half: no prompt, nothing typed, and nothing the far end has to
    /// cooperate with.
    pub fn watch_console(&mut self, now: f64) {
        let expect = match Self::console_patterns(&self.console.expect) {
            Ok(patterns) => patterns,
            Err(error) => return self.failed(error),
        };
        let fail = match Self::console_patterns(&self.console.fail) {
            Ok(patterns) => patterns,
            Err(error) => return self.failed(error),
        };
        if expect.is_empty() && fail.is_empty() {
            return self.failed(Error::InvalidRequest(
                "enter at least one pattern to watch for: the text that means the board worked, \
                 or the text that means it reported a failure"
                    .to_string(),
            ));
        }
        self.start_console(ConsoleTask::Watch { expect, fail }, now);
    }

    /// Start a U-Boot gadget, handing the board's flash to the USB side.
    pub fn start_gadget(&mut self, gadget: Gadget, now: f64) {
        let device = match GadgetDevice::parse(self.console.gadget_dev.trim()) {
            Ok(device) => device,
            Err(error) => return self.failed(error),
        };
        self.start_console(ConsoleTask::Gadget(gadget, device), now);
    }

    /// Type one line at the prompt.
    ///
    /// **It is ungated, and the screen that offers it says so.** U-Boot runs
    /// whatever is typed. Nothing about it is planned or confirmed, because typing
    /// at a prompt through pyrographer is the same act as typing at the prompt
    /// directly.
    pub fn run_console_command(&mut self, now: f64) {
        let line = self.console.command.trim().to_string();
        if line.is_empty() {
            return;
        }
        self.start_console(ConsoleTask::Command(line), now);
    }

    /// Ask the board what it boots from, and what an override would set: the dry
    /// run.
    ///
    /// It is async, unlike the recovery plan, because a U-Boot prompt is the one
    /// write target that can be asked what it currently believes. The plan is
    /// therefore a job, and its answer becomes the screen a person confirms.
    pub fn plan_boot_override(&mut self, now: f64) {
        let targets = self.console.targets.trim().to_string();
        self.start_console(ConsoleTask::PlanBoot(targets), now);
    }

    /// Confirm the boot override, and run it.
    ///
    /// This is the only place a `ConfirmedBoot` is minted. It runs over the line
    /// the plan was asked over, not over whatever the form says now. The port is
    /// opened afresh, because the plan and the override are two sessions. Between
    /// them, the board waits at its prompt where the first session left it.
    pub fn boot_override(&mut self, _now: f64) {
        let Some((line, confirmed)) = self.session.confirm_boot() else {
            return;
        };
        // The line the plan was asked over, not whatever the form says now.
        let port = match self.planned_port(PortField::Console, line.port.clone()) {
            Ok(port) => port,
            Err(error) => {
                self.failed(error);
                return;
            }
        };
        let baud = line.baud;

        self.open_port(
            port,
            baud,
            PendingSerial::Console {
                line,
                task: ConsoleTask::Boot(confirmed),
            },
        );
    }
}

/// The Ingenic boot-ROM bootstrap's acts.
///
/// They are cross-platform, and only the acquisition seam differs.
/// `platform::open_bootrom` claims the bulk pair that nusb finds in the descriptors
/// natively, or the pair WebUSB reports in a tab. Everything from the form through
/// the two-stage upload is this code in both builds.
impl App {
    /// Whether an Ingenic stage file dialog is open.
    ///
    /// It applies the whole rule, not just this form's slot. See
    /// [`is_picking`](Self::is_picking).
    pub fn is_picking_ingenic_stage(&self) -> bool {
        self.is_picking()
    }

    /// Whether an Ingenic bootstrap is in flight: its board being opened, or the
    /// upload running.
    pub fn is_bootstrapping_ingenic(&self) -> bool {
        self.ingenic_bootstrapping.is_some() || self.session.ingenic_bootstrap.is_some()
    }

    /// Pick one of the bootstrap's two stages: the SPL, or the U-Boot.
    pub fn pick_ingenic_stage(&mut self, which: IngenicStageKind) {
        if self.is_picking_ingenic_stage() {
            return;
        }
        // Remembered until the dialog answers, because one dialog fills both.
        self.ingenic_picking_which = which;

        let slot: Slot<Picked<PickedBlob>> = share(None);
        self.ingenic_picking = Some(slot.clone());

        let wake = self.wake();
        platform::spawn_answering(
            "picking an Ingenic stage",
            &slot,
            wake,
            platform::ask_for_blob,
        );
    }

    /// A stage file somebody picked. A dialog somebody closed leaves the stage as
    /// it was. The "Forget" button clears one explicitly.
    fn collect_ingenic_stage(&mut self) {
        let Some(picked) = self
            .ingenic_picking
            .as_ref()
            .and_then(|slot| lock(slot).take())
        else {
            return;
        };
        self.ingenic_picking = None;

        let blob = match picked {
            Ok(Some(blob)) => Some(blob),
            Ok(None) => return,
            Err(error) => {
                self.failed(error);
                return;
            }
        };

        match self.ingenic_picking_which {
            IngenicStageKind::Stage1 => self.ingenic.stage1 = blob,
            IngenicStageKind::Stage2 => self.ingenic.stage2 = blob,
        }
    }

    /// Bootstrap the target boot-ROM board to DFU from the form.
    ///
    /// The Ingenic counterpart of [`upload_loader`](Self::upload_loader), differing
    /// where the two flows differ. The loader is built here, synchronously, from
    /// stages already in memory and addresses already typed. The only off-frame
    /// step is therefore opening the board. In the maskrom flow, the file dialog
    /// and the open share one task.
    ///
    /// A form that does not parse (no stage1, a bad address, or a bad settle time)
    /// is reported, and nothing opens. The bootstrap job itself starts in
    /// `collect_ingenic_bootstrap` once the board is open.
    pub fn bootstrap_ingenic(&mut self) {
        if self.is_bootstrapping_ingenic() || self.session.job.is_some() {
            return;
        }
        let Some(handle) = self.session.target.handle.clone() else {
            return;
        };
        let loader = match self.build_ingenic_loader() {
            Ok(loader) => loader,
            Err(error) => {
                self.failed(error);
                return;
            }
        };

        let slot: Slot<IngenicBootstrapPick> = share(None);
        self.ingenic_bootstrapping = Some(slot.clone());

        let wake = self.wake();
        platform::spawn_answering(
            "opening the boot-ROM board",
            &slot,
            wake,
            move || async move {
                let transport = platform::open_bootrom(&handle).await?;
                Ok(Some((transport, loader)))
            },
        );
    }

    /// Assemble the [`IngenicLoader`] from the form, or say what is missing.
    ///
    /// The bytes are cloned from the picked blobs so the loader owns them. A job
    /// outlives the frame that started it, and the form can change while it runs.
    ///
    /// [`IngenicLoader`]: pyrographer_core::bootstrap::ingenic::IngenicLoader
    fn build_ingenic_loader(&self) -> Result<pyrographer_core::bootstrap::ingenic::IngenicLoader> {
        use pyrographer_core::bootstrap::ingenic::{IngenicLoader, Stage};

        let stage1 = self.ingenic.stage1.as_ref().ok_or_else(|| {
            Error::InvalidRequest(
                "choose a stage1 (the DRAM-init SPL) before bootstrapping".to_string(),
            )
        })?;
        let stage1_addr = parse_load_address(self.ingenic.stage1_addr.trim())
            .map_err(|e| Error::InvalidRequest(format!("the stage1 load address {e}")))?;
        let settle_ms = self.ingenic.settle_ms.trim().parse::<u32>().map_err(|e| {
            Error::InvalidRequest(format!(
                "the DRAM settle time is a number of milliseconds: {e}"
            ))
        })?;

        let stage2 = match self.ingenic.stage2.as_ref() {
            Some(blob) => {
                let addr = parse_load_address(self.ingenic.stage2_addr.trim())
                    .map_err(|e| Error::InvalidRequest(format!("the stage2 load address {e}")))?;
                Some(Stage {
                    name: blob.name.clone(),
                    data: blob.bytes.clone(),
                    load_address: addr,
                    entry_address: addr,
                    settle_ms: 0,
                })
            }
            None => None,
        };

        Ok(IngenicLoader {
            stage1: Stage {
                name: stage1.name.clone(),
                data: stage1.bytes.clone(),
                load_address: stage1_addr,
                entry_address: stage1_addr,
                settle_ms,
            },
            stage2,
        })
    }

    /// A boot-ROM board opened for a bootstrap: start the upload.
    ///
    /// `Ok(None)` does not occur here, because there is no dialog to close, only an
    /// open that returns a transport or fails. The shape matches
    /// `collect_bootstrap`, so adding a gesture that can be closed costs nothing.
    fn collect_ingenic_bootstrap(&mut self, now: f64) {
        let Some(result) = self
            .ingenic_bootstrapping
            .as_ref()
            .and_then(|slot| lock(slot).take())
        else {
            return;
        };
        self.ingenic_bootstrapping = None;

        match result {
            Ok(Some((transport, loader))) => {
                let wake = self.wake();
                // The board the transport was opened on, carried into the job so
                // the harvest can tell whether the slot still holds it.
                let board = self.session.target.handle.clone();
                match self
                    .session
                    .start_ingenic_bootstrap(transport, loader, board, now, wake)
                {
                    Some(work) => platform::spawn(move || work.run()),
                    None => self.failed(Error::InvalidRequest(
                        "a job is already running. Wait for it to finish, then bootstrap again"
                            .to_string(),
                    )),
                }
            }
            Ok(None) => {}
            Err(error) => self.failed(error),
        }
    }
}

/// Parse a `0x`-prefixed (or bare) 32-bit hex load address, the way the CLI's
/// `usbboot` does. The error reads as the end of a sentence, which the caller
/// begins by naming the stage.
fn parse_load_address(s: &str) -> std::result::Result<u32, String> {
    if s.is_empty() {
        return Err("is required: a hex address like 0x80000000".to_string());
    }
    let digits = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    u32::from_str_radix(digits, 16).map_err(|e| format!("'{s}' is not a 32-bit hex address: {e}"))
}

/// The web build's two extra acts, both of which open a chooser the page did not
/// draw.
#[cfg(target_arch = "wasm32")]
impl App {
    /// Whether the browser's serial-port chooser is open, for either purpose:
    /// naming a port, or picking it again to confirm a recovery.
    pub fn is_choosing_port(&self) -> bool {
        self.choosing_port.is_some() || self.repicking_port.is_some()
    }

    /// Ask the person to pick a serial port for one of the two serial forms.
    ///
    /// This is the serial acquisition seam, and the counterpart of
    /// [`choose`](Self::choose). There is no field to type into, because a page
    /// cannot be given a path. `requestPort` opens a chooser that only a user
    /// gesture can open, and what comes back is an object rather than a place.
    ///
    /// The chooser is unfiltered, which [`platform::ask_for_port`] explains: what
    /// is plugged in is a USB-serial adapter, and its identity is the adapter's
    /// rather than the board's.
    pub fn choose_port(&mut self, which: PortField) {
        if self.is_choosing_port() {
            return;
        }
        // Which form it fills is remembered until the chooser answers, the same
        // shape `choosing_for` and `picking_recovery_which` use.
        self.choosing_port_for = which;

        let slot = share(None);
        self.choosing_port = Some(slot.clone());

        let wake = self.wake();
        platform::spawn_answering(
            "choosing a serial port",
            &slot,
            wake,
            platform::ask_for_port,
        );
    }

    /// A serial port somebody picked.
    ///
    /// The handle is held for the open, and its *name* goes into the form. A plan
    /// quotes that name, and the transcript is headed with it. Everything that
    /// reads the form therefore reads a port the same way in both builds. A
    /// chooser somebody closed leaves the port as it was.
    ///
    /// Picking a recovery port discards any recovery plan made against the old one,
    /// for the same reason picking a new file does. A plan names the port it was
    /// made for. A plan for a port that is no longer chosen would be confirmed
    /// against the wrong line.
    fn collect_chosen_port(&mut self) {
        let Some(picked) = self
            .choosing_port
            .as_ref()
            .and_then(|slot| lock(slot).take())
        else {
            return;
        };
        self.choosing_port = None;

        let port = match picked {
            Ok(Some(port)) => port,
            Ok(None) => return,
            Err(error) => {
                self.failed(error);
                return;
            }
        };

        let name = platform::port_name(&port);
        match self.choosing_port_for {
            PortField::Recovery => {
                self.recover.port = name;
                self.recover_port = Some(port);
                self.session.dismiss_recovery_plan();
            }
            PortField::Console => {
                self.console.port = name;
                self.console_port = Some(port);
                // A boot override is planned against a line, so one planned over
                // the port nobody chose any more is agreed to about the wrong
                // board. `planned_port` rests on this.
                self.session.dismiss_boot_plan();
            }
        }
    }

    /// Whether the browser's device chooser is open.
    pub fn is_choosing(&self) -> bool {
        self.choosing.is_some() || self.repicking.is_some()
    }

    /// Ask the person to pick a board.
    ///
    /// This is the acquisition seam. There is no list to click a row in. A page
    /// cannot enumerate a bus, so it is handed the one device somebody picked, from
    /// a chooser that only a user gesture can open.
    pub fn choose(&mut self, which: Which) {
        if self.is_choosing() {
            return;
        }
        // Which board it becomes is remembered until the chooser answers.
        self.choosing_for = which;

        let slot = share(None);
        self.choosing = Some(slot.clone());

        let wake = self.wake();
        platform::spawn_answering("choosing a board", &slot, wake, platform::ask_for_board);
    }

    /// A board somebody picked.
    fn collect_chosen(&mut self) {
        let Some(picked) = self.choosing.as_ref().and_then(|slot| lock(slot).take()) else {
            return;
        };
        self.choosing = None;

        match picked {
            // The board slot is refused if a job holds it (`select_*` returns
            // false); the chooser is gated on `job.is_none()`, so in normal use it
            // is free, and the dropped device on the rare race is the safe outcome.
            Ok(Some((handle, device))) => {
                match self.choosing_for {
                    Which::Target => self.session.select_target(handle, device),
                    Which::Source => self.session.select_source(handle, device),
                };
                // A grant is the one thing that changes what `getDevices` will
                // answer, so this is where the list is worth taking again: the
                // board just picked becomes a row like any other, and stays one
                // across the next reload.
                self.relist();
            }
            Ok(None) => {}
            Err(error) => self.failed(error),
        }
    }

    /// Whether the granted-board list is being fetched.
    ///
    /// The Refresh button is disabled meanwhile. A person therefore cannot start a
    /// second `getDevices` before the first answers, and have the answers land out
    /// of order.
    pub fn is_listing(&self) -> bool {
        self.listing.is_some()
    }

    /// Fetch the list of boards this origin has already been granted.
    ///
    /// This is neither a scan nor a second chooser. It asks the browser which
    /// devices this page already has permission for. That question needs no user
    /// gesture, so it can run at startup, where [`choose`](Self::choose) cannot.
    /// [`platform::list_permitted`](crate::platform::list_permitted) describes what
    /// the answer contains and omits.
    pub fn relist(&mut self) {
        if self.is_listing() {
            return;
        }

        let slot = share(None);
        self.listing = Some(slot.clone());

        let wake = self.wake();
        platform::spawn_answering("listing the boards", &slot, wake, platform::list_permitted);
    }

    /// The granted-board list, once it has come back.
    ///
    /// It goes through the same `devices_seen` the native scan feeds, so a board
    /// that has gone is forgotten by the same rule. On the web that rule compares
    /// object identity rather than a bus address, because object identity is what
    /// identifies a device in a tab. A list that cannot be fetched is reported, and
    /// the old list stays. Its rows can then be stale, which is better than a page
    /// that empties itself over a transient failure.
    fn collect_listed(&mut self, now: f64) {
        let Some(listed) = self.listing.as_ref().and_then(|slot| lock(slot).take()) else {
            return;
        };
        self.listing = None;

        match listed {
            Ok(devices) => self.session.devices_seen(devices, now),
            Err(error) => self.failed(error),
        }
    }

    /// Pick the destination again, to confirm a write.
    ///
    /// This is the web build's confirmation. Picking is not stronger than typing,
    /// but a tab has nothing short and stable to transcribe. `requestDevice` also
    /// opens a dialog that the page did not draw and cannot script. The page cannot
    /// pre-select a row in it, and cannot open it without a real user gesture.
    pub fn repick(&mut self) {
        if self.is_choosing() {
            return;
        }
        let Some(destination) = self.session.target.handle.clone() else {
            return;
        };

        let slot: Slot<Result<bool>> = share(None);
        self.repicking = Some(slot.clone());
        // About the last answer, not about the session.
        self.repick_missed = false;

        let wake = self.wake();
        platform::spawn_answering(
            "confirming the destination",
            &slot,
            wake,
            move || async move {
                platform::ask_for_board().await.map(|picked| match picked {
                    Some((handle, _)) => platform::same_board(&handle, &destination),
                    // A chooser somebody closed is not a confirmation.
                    None => false,
                })
            },
        );
    }

    /// Confirm a recovery by picking the port again.
    ///
    /// This is the serial counterpart of [`repick`](Self::repick), and it depends
    /// on the same property for the same reason. Natively a person transcribes the
    /// port path. The path names the destination and, with several adapters
    /// plugged in, *which* adapter it is. In a tab there is no path to transcribe,
    /// so the person goes back through the chooser. The pick is a confirmation,
    /// rather than a click, because the port picked can be told apart from the
    /// port not picked.
    ///
    /// It depends on object identity, and is **\[UNVERIFIED\]** for the same reason
    /// [`platform::same_board`] is. The specification implies that the browser
    /// returns the same `SerialPort` instance for a port it has already granted,
    /// and nothing here has observed it.
    pub fn repick_port(&mut self) {
        if self.is_choosing_port() {
            return;
        }
        let Some(destination) = self.recover_port.clone() else {
            return;
        };

        let slot: Slot<Result<bool>> = share(None);
        self.repicking_port = Some(slot.clone());
        self.repick_missed = false;

        let wake = self.wake();
        platform::spawn_answering("confirming the port", &slot, wake, move || async move {
            platform::ask_for_port().await.map(|picked| match picked {
                Some(port) => platform::same_port(&port, &destination),
                // A chooser somebody closed is not a confirmation.
                None => false,
            })
        });
    }

    /// Whether the port picked again is the port the recovery would write to.
    fn collect_repick_port(&mut self) {
        let Some(same) = self
            .repicking_port
            .as_ref()
            .and_then(|slot| lock(slot).take())
        else {
            return;
        };
        self.repicking_port = None;

        match same {
            Ok(same) => {
                // A `false` here is a correct refusal that nobody could see: the
                // same button, redrawn. The flag is what lets the gate say so.
                self.repick_missed = !same;
                if let Some(pending) = self.session.recovery.pending.as_mut()
                    && let crate::state::Confirmation::Repicked { done } = &mut pending.confirmation
                {
                    *done = same;
                }
            }
            Err(error) => self.failed(error),
        }
    }

    /// Whether the board picked again is the board the plan would destroy.
    fn collect_repick(&mut self) {
        let Some(same) = self.repicking.as_ref().and_then(|slot| lock(slot).take()) else {
            return;
        };
        self.repicking = None;

        match same {
            Ok(same) => {
                self.repick_missed = !same;
                if let Some(pending) = self.session.pending.as_mut()
                    && let crate::state::Confirmation::Repicked { done } = &mut pending.confirmation
                {
                    *done = same;
                }
            }
            Err(error) => self.failed(error),
        }
    }
}

impl eframe::App for App {
    /// The window's background, tracking the theme.
    ///
    /// eframe's default clear color is a fixed near-black that ignores the
    /// palette. It shows through wherever the content frame does not reach, such as
    /// the strip below a short screen. In a light theme it appears as a dark band.
    /// This returns the active theme's panel fill, so the window clears to the same
    /// background as the content.
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        visuals.panel_fill.to_normalized_gamma_f32()
    }

    /// Everything that is not drawing.
    fn logic(&mut self, _ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let now = self.now();
        self.collect(now);

        #[cfg(not(target_arch = "wasm32"))]
        self.rescan(now);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui::draw(self, ui);
        // After the screen, and over it: the ring is drawn from the focused
        // widget's own rectangle, so it costs no call site anything and no call
        // site can forget it. See [`theme::focus_ring`] for why it exists at all.
        crate::theme::focus_ring(ui.ctx());
    }
}
