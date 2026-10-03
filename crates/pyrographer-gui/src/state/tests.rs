//! The GUI's reasoning, pinned without a board and without a window.
//!
//! Every test here runs a real job, over core's real verbs, against core's scripted
//! transport. The scripted transport asserts the exact bytes of every transfer. A job
//! whose transfers differ from the script is therefore caught here, before it
//! reaches a board. No egui type appears here, because [`state`](super) has none to
//! test.

use std::io::Write;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use pyrographer_core::Error;
use pyrographer_core::agent::{DfuAgent, FlashAgent, FlashInfo, ReadBack, RockusbAgent};
use pyrographer_core::codec::bot::Direction;
use pyrographer_core::codec::dfu_alt::AltSetting;
use pyrographer_core::codec::rkboot::{self, CHUNK_SIZE, CodeBlob, LoaderImage};
use pyrographer_core::codec::rockusb::{self, Opcode, ResetMode};
use pyrographer_core::codec::{splhdr, xmodem};
use pyrographer_core::discovery::{DeviceInfo, Mode, Vendor};
use pyrographer_core::image::SyncWriter;
use pyrographer_core::partition::TableFormat;
use pyrographer_core::recovery::RecoveryTarget;
use pyrographer_core::testing::{
    CHIP_VERSION, FLASH_SECTORS, a_boards_partitions, cbw, cbw_with_subcode, gpt_copy, gpt_table,
    scripted_chip_version, scripted_gpt, scripted_info, scripted_plan, scripted_plan_with_gpt,
    scripted_read, sector,
};
use pyrographer_core::transport::testing::{ScriptedSerial, ScriptedTransport, SerialStep, Step};
use pyrographer_core::uboot::{Gadget, GadgetDevice};
use pyrographer_core::verbs::{TableAction, Touches, WritePlan};

use super::*;
use crate::platform::{BoxedWriter, PickedBlob, PickedImage};

/// A board on the bus, as a scan would report one.
fn a_device(bus: &str, address: u8) -> DeviceInfo {
    DeviceInfo {
        vendor: Vendor::Rockchip,
        vendor_id: 0x2207,
        product_id: 0x350e,
        bcd_usb: 0x0201, // odd: a loader is running
        mode: Mode::Loader,
        bus_id: bus.to_string(),
        device_address: address,
    }
}

/// An agent over a scripted conversation.
fn an_agent(steps: Vec<Step>) -> FlashAgent<ScriptedTransport> {
    FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(steps)))
}

/// A wake callback that does nothing, because a test has no frame loop to poke.
fn no_wake() -> Wake {
    Box::new(|| {})
}

/// A session with a board open, and a second board on the bus beside it.
fn a_session(steps: Vec<Step>) -> Session<ScriptedTransport> {
    let mut session = Session::new(ConfirmBy::Coordinate);
    session.devices_seen(vec![a_device("003", 12), a_device("003", 14)], 0.0);
    session.select_target(a_device("003", 12), a_device("003", 12));
    session.target.opened(an_agent(steps));
    session
}

/// Run a job to completion on the test's own thread, with no window and no board.
fn run(session: &mut Session<ScriptedTransport>, task: Task) {
    let work = session
        .start(task, 0.0, no_wake())
        .expect("the board is idle, so a job can start on it");
    pollster::block_on(work.run());
    assert!(session.harvest(), "the job ended, so it was harvested");
}

/// An image, as a person picks one.
fn an_image(name: &str, bytes: u64) -> PickedImage {
    PickedImage {
        name: name.to_string(),
        bytes,
        path: name.into(),
    }
}

/// A dump's sink, and a way to look inside it afterwards.
fn a_sink() -> (BoxedWriter, Arc<Mutex<Vec<u8>>>) {
    let landed = Arc::new(Mutex::new(Vec::new()));
    let sink = Shared(Arc::clone(&landed));
    (Box::new(SyncWriter::new(sink)), landed)
}

/// A sink whose bytes a test can read back.
struct Shared(Arc<Mutex<Vec<u8>>>);

impl Write for Shared {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        lock(&self.0).extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// One 1 MiB window, as a device answers it: sixty-four reads of thirty-two
/// sectors, which is the most one rockusb command moves.
fn one_window(fill: u8) -> Vec<Step> {
    (0..64u32)
        .flat_map(|i| scripted_read(i + 1, u64::from(i) * 32, vec![fill; 32 * 512]))
        .collect()
}

/// A job borrows the agent, and the connection reports *busy* while it has it.
///
/// The agent goes out with the job and comes back at the job's end. For that whole
/// time the connection reports busy, not absent. The CLI never needs this. A GUI
/// does, because an agent has to outlive the errors it reports.
#[test]
fn a_job_borrows_the_agent_and_hands_it_back() {
    let mut session = a_session(one_window(0xaa));
    let (sink, landed) = a_sink();

    let work = session
        .start(
            Task::Dump {
                lba: 0,
                sectors: 2048, // one 1 MiB window
                sink,
                name: "dump.img".to_string(),
            },
            0.0,
            no_wake(),
        )
        .expect("the board is idle");

    assert!(
        session.target.connection.is_busy(),
        "the job has the agent, and the connection says so"
    );
    assert!(session.job.is_some());
    assert!(
        session.start(Task::Info, 0.0, no_wake()).is_none(),
        "and nothing else can start on it while it does"
    );

    pollster::block_on(work.run());
    assert!(session.harvest());

    assert!(
        session.target.connection.is_idle(),
        "the agent came back, and the board is free again"
    );
    assert!(session.job.is_none());
    assert!(
        matches!(
            session.last,
            Some(Ok(Report::Dumped {
                bytes: 1_048_576,
                ..
            }))
        ),
        "a megabyte landed"
    );
    assert_eq!(lock(&landed).len(), 1024 * 1024);
}

/// The fill finding comes back with the dump's report.
///
/// `one_window(0xcc)` is a whole 1 MiB window of one byte, the read wall in
/// miniature. Every read is answered with a success status and a buffer of poison.
/// The report carries the finding, so the results screen can warn a person that the
/// dump is suspect.
#[test]
fn a_dump_that_reads_constant_fill_carries_the_finding_back() {
    let mut session = a_session(one_window(0xcc));
    let (sink, _landed) = a_sink();

    let work = session
        .start(
            Task::Dump {
                lba: 0,
                sectors: 2048, // one 1 MiB window, all 0xcc
                sink,
                name: "dump.img".to_string(),
            },
            0.0,
            no_wake(),
        )
        .expect("the board is idle");

    pollster::block_on(work.run());
    assert!(session.harvest());

    let Some(Ok(Report::Dumped { fill, .. })) = &session.last else {
        panic!("expected a successful dump report");
    };
    assert!(
        fill.has_suspicious(),
        "0xcc across a whole window is flagged"
    );
    let run = fill.suspicious().next().expect("one suspicious run");
    assert_eq!(run.byte(), 0xcc);
    assert_eq!(run.first_lba(), 0);
    assert_eq!(run.sectors(), 2048);
}

/// A sink that stops on the first window and waits to be let go.
///
/// A verb checks the cancellation token at each window boundary, and this sink
/// pauses the job exactly at one.
struct Pausing {
    landed: mpsc::Sender<()>,
    go: mpsc::Receiver<()>,
    written: Arc<Mutex<usize>>,
}

impl Write for Pausing {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        *lock(&self.written) += buf.len();
        let _ = self.landed.send(());
        let _ = self.go.recv();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A job canceled mid-flight stops at the next window boundary, and the agent comes
/// back.
///
/// Canceling is not a crash, and the board the job was talking to is still there.
///
/// The script holds one window and the job is asked for a hundred. A job that
/// ignored the token would run the transport off the end of its script and panic, so
/// the test can fail. The sink holds the job at the boundary. The first window lands
/// in it, the token is flipped while it waits, and the second window is never
/// *read*.
///
/// The job runs on a real thread, as it does in the window. The test therefore
/// compiles against the `Send` bounds a job needs.
#[test]
fn a_job_canceled_mid_flight_stops_at_the_next_window_and_gives_the_agent_back() {
    let (landed_tx, landed_rx) = mpsc::channel();
    let (go_tx, go_rx) = mpsc::channel();
    let written = Arc::new(Mutex::new(0usize));

    let mut session = a_session(one_window(0x5a));
    let sink = Pausing {
        landed: landed_tx,
        go: go_rx,
        written: Arc::clone(&written),
    };

    let work = session
        .start(
            Task::Dump {
                lba: 0,
                sectors: 100 * 2048, // a hundred windows, and one window of script
                sink: Box::new(SyncWriter::new(sink)),
                name: "dump.img".to_string(),
            },
            0.0,
            no_wake(),
        )
        .expect("the board is idle");

    let running = std::thread::spawn(move || pollster::block_on(work.run()));

    landed_rx.recv().expect("the first window reached the sink");
    session.job.as_ref().expect("a job is running").cancel();
    go_tx.send(()).expect("and now the sink lets it go");

    running.join().expect("the job ended rather than panicking");
    assert!(session.harvest());

    assert!(
        matches!(session.last, Some(Err(Error::Canceled))),
        "the job stopped because it was asked to"
    );
    assert!(
        session.target.connection.is_idle(),
        "and the board is still there, and still usable"
    );
    assert_eq!(
        *lock(&written),
        1024 * 1024,
        "one window landed, and a second was never read"
    );
}

/// A sink that panics the moment it is written to.
///
/// It stands in for any panic on a job thread: a verb, a progress sink, or a reader
/// that panics mid-window.
struct Exploding;

impl Write for Exploding {
    fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
        panic!("a sink/reader/verb blew up mid-job");
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A job whose thread panics does not wedge the connection forever.
///
/// The agents are dropped in the unwind and cannot come back, so there is nothing
/// to restore. The ending is a drop obligation, though. The frame loop therefore
/// still learns the job is over, moves the connection to `Desynchronized` (reopen
/// it), and reports a fault. Without the obligation, the shared cell stays empty and
/// `harvest` reads it as *still running*. The board then stays `Busy` for the life
/// of the process.
///
/// The job runs on a real thread and is allowed to panic there, as it would in the
/// window. The panic is expected, so the thread's join is an `Err`.
#[test]
fn a_job_whose_thread_panics_lands_the_connection_instead_of_wedging_it() {
    let mut session = a_session(one_window(0xaa));

    let work = session
        .start(
            Task::Dump {
                lba: 0,
                sectors: 2048,
                sink: Box::new(SyncWriter::new(Exploding)),
                name: "dump.img".to_string(),
            },
            0.0,
            no_wake(),
        )
        .expect("the board is idle");
    assert!(session.target.connection.is_busy(), "the job has the agent");

    // The default panic hook would print a backtrace; quiet it for the one panic
    // this test provokes on purpose, then put it back.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let died = std::thread::spawn(move || pollster::block_on(work.run())).join();
    std::panic::set_hook(hook);
    assert!(
        died.is_err(),
        "the job thread panicked, as the sink made it"
    );

    assert!(
        session.harvest(),
        "the ending was published on the way out, even though the job died"
    );
    assert!(session.job.is_none(), "the job is cleared");
    assert!(
        session.target.connection.is_desynchronized(),
        "the connection is finished and asks to be reopened, not left Busy forever"
    );
    assert!(
        matches!(session.last, Some(Err(Error::Internal(_)))),
        "and the death is reported, not swallowed"
    );
}

/// A desynchronized agent is finished, and the connection reports it at once.
///
/// The connection reports it at the moment of desynchronization, not at the next
/// button press. A data phase that comes back short leaves the device holding bytes
/// the host cannot count, so the agent refuses every command after it. A CLI never
/// notices, because an error ends the process. A GUI holds the agent, and this test
/// pins what the GUI does with an agent that is finished.
#[test]
fn an_agent_that_falls_out_of_step_leaves_a_connection_that_has_to_be_reopened() {
    let mut session = a_session(vec![
        cbw(
            1,
            rockusb::FLASH_ID_LEN as u32,
            Direction::In,
            Opcode::ReadFlashId,
            0,
            0,
        ),
        Step::Reply(vec![0; 3]), // three of the five bytes it announced
    ]);

    run(&mut session, Task::Info);

    assert!(
        matches!(session.last, Some(Err(Error::Protocol(_)))),
        "the conversation broke"
    );
    assert!(
        session.target.connection.is_desynchronized(),
        "and the connection is finished, rather than idle and lying about it"
    );
    assert!(
        session.start(Task::Info, 0.0, no_wake()).is_none(),
        "so nothing can be started on it"
    );
    assert!(
        Error::Desynchronized.hint().is_some(),
        "and there is a way out, which the hint gives"
    );
}

/// The dry run asks the board what a write would touch.
///
/// The answer becomes a screen waiting for a person to agree to it. The dry run is
/// the same call as the write, ended one step early. The plan it shows therefore
/// describes the write that would run.
#[test]
fn planning_a_write_asks_the_board_and_leaves_a_plan_waiting_to_be_agreed_to() {
    let mut session = a_session(scripted_plan_with_gpt(1, &a_boards_partitions()));

    run(
        &mut session,
        Task::PlanWrite {
            aim: Aim::Partition("uboot".to_string()),
            image_bytes: 4096,
            soc: None,
        },
    );

    let pending = session.pending.as_ref().expect("a plan is waiting");
    let Plan::Write(plan) = &pending.plan else {
        panic!("a write was planned");
    };

    assert_eq!(plan.lba, 16384, "the range came off the board's own table");
    assert_eq!(plan.image_bytes, 4096);
    assert_eq!(
        plan.chip_version, CHIP_VERSION,
        "and the loader's own account of itself is in the plan, raw"
    );
    assert_eq!(plan.flash.size_bytes, FLASH_SECTORS * 512);

    // The line the partition table exists for.
    let Touches::Partitions(overlaps) = &plan.touches else {
        panic!("the board has a table, so the plan says what the write lands in");
    };
    assert_eq!(overlaps.len(), 1);
    assert_eq!(overlaps[0].name, "uboot");

    assert!(
        session.target.connection.is_idle(),
        "and nothing was written: the board is idle and unchanged"
    );
}

/// A plan that named no SoC is refused on its screen, rather than asking for a
/// confirmation it would then discard.
///
/// Core is asked before any person is asked. `verbs::plan_refusal` gives the same
/// answer `verbs::flash` would give a moment later. Nobody therefore types a
/// coordinate for a write that was never going to happen. A discarded confirmation
/// teaches a person that the gate is theater.
#[test]
fn a_plan_that_named_no_soc_cannot_be_confirmed_at_all() {
    let mut session = a_session(scripted_plan(1));

    run(
        &mut session,
        Task::PlanWrite {
            aim: Aim::Lba(0),
            image_bytes: 4096,
            soc: None,
        },
    );

    let pending = session.pending.as_mut().expect("a plan is waiting");
    assert!(
        pending.refused.is_some(),
        "no SoC was named, so the gate has nothing to compare, and the plan says so"
    );

    // Even typed exactly right.
    if let Confirmation::Typed { expected, typed } = &mut pending.confirmation {
        *typed = expected.clone();
    }
    assert!(
        session
            .pending
            .as_ref()
            .unwrap()
            .confirmation
            .is_satisfied()
    );
    assert!(!session.pending.as_ref().unwrap().can_confirm());
    assert!(
        session.confirm().is_none(),
        "nothing is minted, because nothing would be written"
    );
    assert!(
        session.pending.is_some(),
        "and the plan stays on the screen rather than vanishing"
    );
}

/// The armed gate opens in the state machine.
///
/// The plan named the SoC the scripted loader answers as, so nothing refuses. The
/// typed coordinate is then the only step left before the plan mints a
/// `ConfirmedWrite`.
#[test]
fn a_plan_whose_named_soc_matches_the_loader_can_be_confirmed() {
    let mut session = a_session(scripted_plan(1));
    let soc = Soc::parse("rk3576").expect("pinned");

    run(
        &mut session,
        Task::PlanWrite {
            aim: Aim::Lba(0),
            image_bytes: 4096,
            soc: Some(soc),
        },
    );

    let pending = session.pending.as_mut().expect("a plan is waiting");
    assert!(
        pending.refused.is_none(),
        "the loader answered as the named SoC: {:?}",
        pending.refused
    );

    if let Confirmation::Typed { expected, typed } = &mut pending.confirmation {
        *typed = expected.clone();
    }
    assert!(
        session.confirm().is_some(),
        "typed exactly right, the plan mints the write"
    );
}

/// A board with a damaged primary GPT and an intact backup, scripted for a repair
/// plan.
///
/// The script answers the geometry and the loader, then the damaged primary and the
/// intact backup that follows it.
fn scripted_repair(entries: &[Vec<u8>]) -> Vec<Step> {
    let (mut primary, _) = gpt_table(entries);
    primary[0x28] ^= 0xff; // the primary's own header CRC no longer holds

    let backup_lba = FLASH_SECTORS - 1;
    let backup_array_lba = backup_lba - 1;
    let (backup_header, backup_array) = gpt_copy(entries, backup_lba, backup_array_lba);

    let mut steps = scripted_info(1); // geometry: tags 1, 2
    steps.extend(scripted_chip_version(3)); // the loader: tag 3
    steps.extend(scripted_read(4, 1, primary)); // the damaged primary, at sector 1
    steps.extend(scripted_read(5, backup_lba, backup_header)); // the backup header
    steps.extend(scripted_read(6, backup_array_lba, sector(&backup_array))); // its array
    steps
}

/// Planning a repair reads both GPT copies and leaves a repair plan waiting to be
/// agreed to.
///
/// The repair goes through the same plan-then-confirm gate a write does. The loader
/// match is checked, and a typed coordinate is the only step left before the
/// rewrite. A repair carries its own bytes, so no image is picked. Confirming it
/// mints a `Confirmed::Table`, the form every table write takes.
#[test]
fn planning_a_repair_leaves_a_plan_waiting_to_be_agreed_to() {
    let mut session = a_session(scripted_repair(&a_boards_partitions()));
    let soc = Soc::parse("rk3576").expect("pinned");

    run(&mut session, Task::PlanRepairTable { soc: Some(soc) });

    let pending = session.pending.as_mut().expect("a plan is waiting");
    assert!(
        pending.refused.is_none(),
        "the loader answered as the named SoC: {:?}",
        pending.refused
    );
    let Plan::Table(plan) = &pending.plan else {
        panic!("a repair was planned");
    };
    assert_eq!(plan.segments.len(), 1, "a GPT repair is one segment");
    assert_eq!(
        plan.segments[0].lba, 1,
        "the primary GPT starts at sector 1"
    );
    assert_eq!(
        plan.partitions
            .iter()
            .map(|partition| partition.name.as_str())
            .collect::<Vec<_>>(),
        ["uboot", "trust", "boot"],
        "the plan names what the recovered table restores"
    );

    // The typed coordinate is the only thing left, exactly as for a write -- and a
    // repair needs no image beside it.
    if let Confirmation::Typed { expected, typed } = &mut pending.confirmation {
        *typed = expected.clone();
    }
    assert!(
        matches!(session.confirm(), Some(Confirmed::Table(_))),
        "typed right, the plan mints the repair"
    );
    assert!(
        session.pending.is_none(),
        "and the plan is spent: one confirmation buys one repair"
    );
}

/// A repair with nothing to repair comes back as a failed job, not a plan waiting on
/// the screen.
///
/// Both copies are healthy and agree, so there is nothing to agree to.
#[test]
fn planning_a_repair_on_a_healthy_table_fails_rather_than_waiting() {
    // A whole, healthy GPT: the plan reads the geometry, the loader, the intact
    // primary, and the agreeing backup behind it.
    let entries = a_boards_partitions();
    let backup_lba = FLASH_SECTORS - 1;
    let backup_array_lba = backup_lba - 1;
    let (backup_header, backup_array) = gpt_copy(&entries, backup_lba, backup_array_lba);

    let mut steps = scripted_info(1);
    steps.extend(scripted_chip_version(3));
    steps.extend(scripted_gpt(4, &entries));
    steps.extend(scripted_read(6, backup_lba, backup_header));
    steps.extend(scripted_read(7, backup_array_lba, sector(&backup_array)));
    let mut session = a_session(steps);
    let soc = Soc::parse("rk3576").expect("pinned");

    run(&mut session, Task::PlanRepairTable { soc: Some(soc) });

    assert!(session.pending.is_none(), "there is nothing to agree to");
    assert!(
        matches!(session.last, Some(Err(Error::InvalidRequest(_)))),
        "a healthy table has nothing to repair, and that is a failed job not a plan"
    );
}

/// Authoring a GPT parses the layout on the job thread and leaves an author plan
/// waiting.
///
/// The picked layout text is carried into the job and parsed there, where the
/// device's own sector count is in reach. This test pins that the parse runs against
/// the board's geometry and produces a `TableAction::Author` plan, with no board and
/// no window. Authoring then goes through the same gate a repair does.
#[test]
fn authoring_a_gpt_parses_the_layout_and_leaves_a_plan_waiting() {
    // `execute` reads the geometry to parse the layout, then `plan_author_gpt` reads
    // the geometry and the loader again: info is two commands, chip_version one.
    let mut steps = scripted_info(1); // execute's info: tags 1, 2
    steps.extend(scripted_info(3)); // the verb's info: tags 3, 4
    steps.extend(scripted_chip_version(5)); // the loader: tag 5

    let mut session = a_session(steps);
    let soc = Soc::parse("rk3576").expect("pinned");
    let source = LayoutSource::Native("data 2048 1024\n".to_string());

    run(
        &mut session,
        Task::PlanAuthorGpt {
            source,
            soc: Some(soc),
        },
    );

    let pending = session.pending.as_ref().expect("a plan is waiting");
    assert!(
        pending.refused.is_none(),
        "the loader answered as the named SoC: {:?}",
        pending.refused
    );
    let Plan::Table(plan) = &pending.plan else {
        panic!("authoring produces a table plan");
    };
    assert_eq!(plan.format, TableFormat::Gpt);
    assert!(
        matches!(plan.action, TableAction::Author),
        "it authors rather than repairs: {:?}",
        plan.action
    );
    assert!(
        plan.partitions.iter().any(|part| part.name == "data"),
        "the layout's partition is in the planned table: {:?}",
        plan.partitions
    );
}

/// A plan, with the wrong-loader refusal cleared.
///
/// The plan names no SoC, and a real board's plan that names no SoC is refused.
/// `a_plan_that_named_no_soc_cannot_be_confirmed_at_all` pins that refusal, and
/// `a_plan_whose_named_soc_matches_the_loader_can_be_confirmed` pins the armed gate
/// opening. This helper sets no refusal, so the tests that use it pin what follows
/// the gate: the comparison.
fn a_plan_awaiting_a_coordinate(destination: &DeviceInfo) -> Pending {
    Pending {
        plan: Plan::Write(WritePlan {
            lba: 0,
            image_bytes: 4096,
            padding_bytes: 0,
            sectors: 8,
            flash: FlashInfo {
                size_bytes: FLASH_SECTORS * 512,
                sector_size: 512,
                medium: None,
                chip_id: None,
            },
            chip_version: CHIP_VERSION.to_vec(),
            soc: None,
            touches: Touches::NoTable,
            read_back: ReadBack::PerWindow,
        }),
        confirmation: Confirmation::asked_of(
            ConfirmBy::Coordinate,
            &Chosen::Usb(destination.clone()),
        ),
        refused: None,
    }
}

/// A running write's panel takes its read-back sentence from the plan.
///
/// The sentence "Every window is read back as it is written" is false on a DFU
/// board. A download is one session per region, nothing can be read until it is
/// committed, and the check runs once the region is written. A panel that printed
/// that sentence for every destructive job would therefore be wrong for DFU. The
/// error is latent while the DFU write refuses at the gate, and stops being latent
/// once an Ingenic SoC is pinned. `ReadBack` words the fact, so the job carries
/// that wording rather than wording it a second time.
#[test]
fn a_jobs_read_back_comes_from_the_plan_it_was_confirmed_against() {
    let mut session = a_session(vec![]);
    session.pending = Some(a_plan_awaiting_a_coordinate(&a_device("003", 12)));
    if let Some(Plan::Write(plan)) = session.pending.as_mut().map(|p| &mut p.plan) {
        plan.read_back = ReadBack::AfterCommit;
    }
    if let Confirmation::Typed { expected, typed } =
        &mut session.pending.as_mut().expect("a plan").confirmation
    {
        *typed = expected.clone();
    }
    let Some(Confirmed::Write(confirmed)) = session.confirm() else {
        panic!("typed right, so a write is authorized");
    };

    let task = Task::Write {
        confirmed,
        image: Box::new(pyrographer_core::image::SyncReader::new(
            std::io::Cursor::new(Vec::new()),
        )),
    };
    assert_eq!(
        task.read_back(),
        Some(ReadBack::AfterCommit),
        "the job carries the backend's own answer, not a rockusb-shaped assumption"
    );

    // And a task that overwrites nothing carries none, so its panel says nothing
    // about a read-back it is not doing.
    assert_eq!(Task::Info.read_back(), None);
}

/// A confirmation typed wrong mints nothing, and the plan stays on the screen.
///
/// A button press that authorizes nothing must not remove the plan a person was
/// reading.
#[test]
fn a_confirmation_typed_wrong_mints_nothing_and_leaves_the_plan_where_it_was() {
    let mut session = a_session(vec![]);
    session.pending = Some(a_plan_awaiting_a_coordinate(&a_device("003", 12)));

    let typed = |session: &mut Session<ScriptedTransport>, what: &str| {
        let pending = session.pending.as_mut().expect("a plan is waiting");
        let Confirmation::Typed { typed, .. } = &mut pending.confirmation else {
            panic!("the native window asks for the coordinate");
        };
        *typed = what.to_string();
    };

    // The other board on the bus. Both are real, and only one of them is the one
    // this plan would destroy.
    typed(&mut session, "003:14");
    assert!(!session.pending.as_ref().unwrap().can_confirm());
    assert!(session.confirm().is_none());
    assert!(session.pending.is_some(), "the plan is still on the screen");

    // Close is not close enough. A coordinate one character off names either no
    // board or the wrong one.
    typed(&mut session, "003:1");
    assert!(session.confirm().is_none());

    // A trailing space is not a different board, though.
    typed(&mut session, " 003:12 ");
    assert!(session.pending.as_ref().unwrap().can_confirm());

    assert!(
        matches!(session.confirm(), Some(Confirmed::Write(_))),
        "typed right, and exactly one write is authorized"
    );
    assert!(
        session.pending.is_none(),
        "and the plan is spent: one confirmation buys one write"
    );
    assert!(
        session.confirm().is_none(),
        "so it cannot be replayed into a second"
    );
}

/// A clone with its source and destination swapped asks for a different string.
///
/// This is the mistake the typed confirmation exists to catch. Careful reading does
/// not catch it, because both boards are real and both halves of the plan are true.
/// The string to be typed is the *destination's*. When the two boards are swapped,
/// the plan asks for a different string, so the string in a person's muscle memory
/// is refused.
#[test]
fn swapping_a_clones_source_and_destination_asks_for_a_different_string() {
    let kept = a_device("003", 12);
    let destroyed = a_device("003", 14);

    let asked_of = |device: &DeviceInfo| match Confirmation::asked_of(
        ConfirmBy::Coordinate,
        &Chosen::Usb(device.clone()),
    ) {
        Confirmation::Typed { expected, .. } => expected,
        Confirmation::Repicked { .. } => panic!("the native window asks for the coordinate"),
    };

    assert_eq!(asked_of(&destroyed), "003:14");
    assert_ne!(
        asked_of(&kept),
        asked_of(&destroyed),
        "two boards, two strings -- which is what a shared product ID could never give"
    );

    // The plan that would destroy `destroyed`, with the coordinate of the board
    // they meant to keep typed into it.
    let mut confirmation =
        Confirmation::asked_of(ConfirmBy::Coordinate, &Chosen::Usb(destroyed.clone()));
    if let Confirmation::Typed { typed, .. } = &mut confirmation {
        *typed = coordinate(&Chosen::Usb(kept.clone()));
    }
    assert!(
        !confirmation.is_satisfied(),
        "the swap is caught by the one thing that differs between the two boards"
    );
}

/// Changing the image discards the plan that was made against it.
///
/// A plan says how many bytes go where, and `verbs::flash` streams exactly that many
/// from whatever it is handed. A plan confirmed against one image and fed another
/// would write the wrong file, at the wrong length. It would write into sectors a
/// person approved for something else. The plan and the image are therefore agreed
/// to together.
#[test]
fn changing_the_image_throws_away_the_plan_that_was_made_against_it() {
    let mut session = a_session(vec![]);
    session.set_image(Some(an_image("u-boot.img", 4096)));
    session.pending = Some(a_plan_awaiting_a_coordinate(&a_device("003", 12)));

    session.set_image(Some(an_image("something-else.img", 8192)));
    assert!(
        session.pending.is_none(),
        "the plan described a write of the old image, and it is not that image any more"
    );

    // And taking the image away entirely does the same.
    session.pending = Some(a_plan_awaiting_a_coordinate(&a_device("003", 12)));
    session.set_image(None);
    assert!(session.pending.is_none());
}

/// A board whose agent is out on a job cannot be re-pointed at a different device.
///
/// The agent returns to the board it left, not to a board selected while it was out.
///
/// This is a cross-attach the typed-coordinate gate cannot catch on its own. A job
/// holds board A's agent on a thread, and board B is clicked mid-job. If the
/// selection went through, the ending's `restore` would install A's agent under B's
/// identity. The plan screen would name B while the write reached A. Both boards are
/// real, so careful reading of the coordinate would not reveal it. The session
/// therefore refuses to re-point a busy board at all, and the agent restores to A.
#[test]
fn a_busy_board_cannot_be_re_pointed_and_its_agent_comes_home_to_it() {
    // Board A (003:12) open, and a long dump running on it.
    let mut session = a_session(one_window(0xaa));
    let (sink, _landed) = a_sink();
    let work = session
        .start(
            Task::Dump {
                lba: 0,
                sectors: 2048, // one 1 MiB window
                sink,
                name: "dump.img".to_string(),
            },
            0.0,
            no_wake(),
        )
        .expect("the board is idle, so the dump can start");
    assert!(session.target.connection.is_busy(), "the job has the agent");

    // Mid-job, board B (003:14) is clicked. The session refuses: A's agent is out
    // on a thread and will come home, and it must come home to A.
    let selected = session.select_target(a_device("003", 14), a_device("003", 14));
    assert!(
        !selected,
        "a board whose agent is out on a job cannot be re-pointed"
    );
    assert_eq!(
        session.target.device.as_ref().map(coordinate),
        Some("003:12".to_string()),
        "the target is still board A, not the board that was clicked mid-job"
    );

    // The job ends, and A's agent comes home to A -- where the write would have
    // gone, and where the plan screen would have named it.
    pollster::block_on(work.run());
    assert!(session.harvest());
    assert!(session.target.connection.is_idle(), "the agent came back");
    assert_eq!(
        session.target.device.as_ref().map(coordinate),
        Some("003:12".to_string()),
        "and it is board A, not a board that was clicked while it was out"
    );
}

/// A clone borrows both boards and hands both back.
///
/// The dry run reads the source's geometry and surveys the destination. Both boards
/// are `Busy` for the length of the job and return `Idle`, and a clone plan is left
/// waiting. This is the two-agent lifecycle, which the single-board tests do not
/// cover.
#[test]
fn a_clone_plan_borrows_both_boards_and_hands_both_back() {
    // The target (destination) answers a full survey; the source answers one info
    // read, which is all `plan_clone` asks of the board being copied.
    let mut session = a_session(scripted_plan_with_gpt(1, &a_boards_partitions()));
    session.select_source(a_device("003", 14), a_device("003", 14));
    session.source.opened(an_agent(scripted_info(1)));

    let work = session
        .start(Task::PlanClone { soc: None }, 0.0, no_wake())
        .expect("both boards are idle, so a clone plan can start");
    assert!(
        session.target.connection.is_busy() && session.source.connection.is_busy(),
        "both boards are lent to the clone for the length of the job"
    );

    pollster::block_on(work.run());
    assert!(session.harvest());

    assert!(
        session.target.connection.is_idle() && session.source.connection.is_idle(),
        "and both come home when it ends"
    );
    assert!(
        matches!(
            session.pending,
            Some(Pending {
                plan: Plan::Clone(_),
                ..
            })
        ),
        "a clone plan is waiting to be agreed to"
    );
}

/// A clone cannot start while the board it would copy is busy.
///
/// The destination it briefly took is handed straight back, not left `Busy` with no
/// job to return it.
#[test]
fn a_clone_whose_source_is_busy_gives_the_target_back() {
    let mut session = a_session(vec![]);
    session.select_source(a_device("003", 14), a_device("003", 14));
    // The source is held by something else -- a job would hold both, but this pins
    // the give-back path in isolation.
    session.source.connection = Connection::Busy;

    assert!(
        session
            .start(Task::PlanClone { soc: None }, 0.0, no_wake())
            .is_none(),
        "a clone needs both boards, and one is busy"
    );
    assert!(
        session.target.connection.is_idle(),
        "so the target it briefly took is handed straight back, not stranded Busy"
    );
    assert!(session.job.is_none(), "and no job was left half-started");
}

/// Choosing a different board discards the plan, because the plan named the board
/// that was chosen before.
#[test]
fn choosing_a_different_board_throws_away_the_plan() {
    let mut session = a_session(vec![]);
    session.pending = Some(a_plan_awaiting_a_coordinate(&a_device("003", 12)));

    session.select_target(a_device("003", 14), a_device("003", 14));
    assert!(session.pending.is_none());
    assert!(
        session.target.flash.is_none() && matches!(session.target.table, Table::Unknown),
        "and nothing the last board said is carried across to this one"
    );
}

/// What a board answers is kept where the next frame can find it.
///
/// Three jobs run one after another over one scripted conversation. The test
/// therefore pins what a board is *asked*, and not only what the session did with
/// the answers. `partitions` reads the geometry again before it looks for a table,
/// because the geometry says where to look. The script includes that read.
#[test]
fn what_a_board_answers_is_remembered() {
    let mut steps = scripted_info(1); // Info: tags 1, 2
    steps.extend(scripted_chip_version(3)); // ChipVersion: tag 3
    steps.extend(scripted_info(4)); // Partitions asks the geometry first: tags 4, 5
    steps.extend(scripted_gpt(6, &a_boards_partitions())); // and then the table: tags 6, 7
    let mut session = a_session(steps);

    run(&mut session, Task::Info);
    assert_eq!(
        session.target.flash.as_ref().map(|flash| flash.size_bytes),
        Some(FLASH_SECTORS * 512)
    );

    run(&mut session, Task::ChipVersion);
    assert_eq!(
        session.target.chip_version.as_deref(),
        Some(&CHIP_VERSION[..])
    );

    run(&mut session, Task::Partitions);
    let table = session.target.table.get().expect("the board has a GPT");
    assert_eq!(table.names(), ["uboot", "trust", "boot"]);
}

/// A DFU board's partitions load through the GUI from its alt-settings, with no
/// flash read.
///
/// The agent is a DFU one and the script is empty, so a probe read would panic. A
/// passing test therefore proves the table came from the descriptors the agent
/// already held. This is the whole GUI path (start, run, harvest, store) over the
/// DFU backend. The aim dropdown and a plan read the partitions from the table it
/// stores.
#[test]
fn a_dfu_boards_partitions_load_from_its_alt_settings_through_the_gui() {
    let alts = vec![
        AltSetting {
            index: 0,
            name: "uboot".to_string(),
            size: Some(256 * 1024),
        },
        AltSetting {
            index: 1,
            name: "rootfs".to_string(),
            size: None,
        },
    ];

    let mut session = Session::new(ConfirmBy::Coordinate);
    session.devices_seen(vec![a_device("003", 12)], 0.0);
    session.select_target(a_device("003", 12), a_device("003", 12));
    // Transfer size 512, so a sector is 512 bytes; the script is empty on purpose.
    session.target.opened(FlashAgent::Dfu(DfuAgent::new(
        ScriptedTransport::new(vec![]),
        0,
        pyrographer_core::testing::dfu_capable(512),
        alts,
    )));

    run(&mut session, Task::Partitions);
    let table = session
        .target
        .table
        .get()
        .expect("a DFU board's alt-settings are its partition table");
    assert_eq!(table.format, TableFormat::DfuAltInfo);
    assert_eq!(table.names(), ["uboot", "rootfs"]);

    // The caps the aim form and the clone button gray on: DFU reads (so it
    // verifies) but addresses no raw LBA.
    let caps = session.target.caps.as_ref().expect("caps cached at open");
    assert!(caps.can_verify);
    assert!(!caps.can_address_raw_lba);

    // And core's sentence for that, which the partition table tools gray on and
    // draw, cached with the caps so a job holding the agent does not blank it.
    assert!(
        session
            .target
            .raw_lba_reason
            .is_some_and(|why| why.contains("named region")),
        "{:?}",
        session.target.raw_lba_reason
    );
}

/// The mode a person chose is the subcode the board is sent, and the report names
/// that mode.
///
/// Without this test, the GUI's selector could be drawn, read and ignored, and every
/// mode would reboot.
#[test]
fn the_chosen_reset_mode_reaches_the_wire() {
    for mode in ResetMode::ALL {
        let mut session = a_session(vec![
            cbw_with_subcode(1, 0, Direction::Out, Opcode::Reset, mode as u8, 0, 0),
            Step::Disconnect,
        ]);

        run(&mut session, Task::Reset { mode });

        let Some(Ok(Report::Reset { mode: reported })) = session.last else {
            panic!("{} should have been reported", mode.name());
        };
        assert_eq!(reported, mode);
        assert!(matches!(
            session.target.connection,
            Connection::Disconnected
        ));
    }
}

/// A board that is rebooting has gone, and everything it reported describes a board
/// that is no longer there.
#[test]
fn resetting_a_board_closes_it() {
    let mut session = a_session(vec![
        cbw(1, 0, Direction::Out, Opcode::Reset, 0, 0),
        Step::Disconnect,
    ]);
    session.target.flash = Some(FlashInfo {
        size_bytes: 1024,
        sector_size: 512,
        medium: None,
        chip_id: None,
    });

    run(
        &mut session,
        Task::Reset {
            mode: ResetMode::Reset,
        },
    );

    assert!(matches!(
        session.last,
        Some(Ok(Report::Reset {
            mode: ResetMode::Reset
        }))
    ));
    assert!(matches!(
        session.target.connection,
        Connection::Disconnected
    ));
    assert!(
        session.target.flash.is_none(),
        "and what it told us went with it"
    );
}

/// The bus is polled at rest and **never during a job**.
///
/// A job needs the bus quiet, and a person watching one needs the list to hold
/// still. A device list that reshuffles during a running write can move the row a
/// person is watching.
#[test]
fn the_bus_is_not_rescanned_while_a_job_is_running() {
    let mut session = a_session(one_window(0));
    let (sink, _) = a_sink();

    assert!(
        session.wants_rescan(2.0),
        "a second has gone by, and nothing is running"
    );

    let work = session
        .start(
            Task::Dump {
                lba: 0,
                sectors: 2048,
                sink,
                name: "dump.img".to_string(),
            },
            0.0,
            no_wake(),
        )
        .expect("the board is idle");

    assert!(
        !session.wants_rescan(100.0),
        "however long it runs for, the list holds still"
    );

    pollster::block_on(work.run());
    session.harvest();
    assert!(session.wants_rescan(100.0), "and moves again when it ends");
}

/// A board that has left the bus and is not open is forgotten.
///
/// Its row would otherwise name a device that is not there. A board that is still
/// *open* is left alone, because the agent decides whether it still works.
#[test]
fn a_board_that_leaves_the_bus_is_forgotten_unless_it_is_open() {
    let mut session = Session::new(ConfirmBy::Coordinate);
    session.devices_seen(vec![a_device("003", 12)], 0.0);
    session.select_target(a_device("003", 12), a_device("003", 12));

    session.devices_seen(Vec::new(), 1.0);
    assert!(
        session.target.device.is_none(),
        "it is not on the bus, and it was not open"
    );

    // The same board, opened this time.
    session.devices_seen(vec![a_device("003", 12)], 2.0);
    session.select_target(a_device("003", 12), a_device("003", 12));
    session.target.opened(an_agent(vec![]));

    session.devices_seen(Vec::new(), 3.0);
    assert!(
        session.target.device.is_some(),
        "an open board is not forgotten because a scan blinked"
    );
}

/// The download-boot control-OUT steps that a faithful upload of `payload` to
/// `index` makes: 4096-byte chunks, and an empty terminator for an exact multiple.
fn expect_control(index: u16, payload: &[u8]) -> Vec<Step> {
    let mut steps: Vec<Step> = payload
        .chunks(CHUNK_SIZE)
        .map(|chunk| Step::ExpectControlOut {
            request_type: 0x40,
            request: 0x0c,
            value: 0,
            index,
            data: chunk.to_vec(),
        })
        .collect();
    if payload.len().is_multiple_of(CHUNK_SIZE) {
        steps.push(Step::ExpectControlOut {
            request_type: 0x40,
            request: 0x0c,
            value: 0,
            index,
            data: Vec::new(),
        });
    }
    steps
}

/// A maskrom bootstrap runs without an agent, and forgets its board.
///
/// The upload uses control transfers to a board that is not a `FlashAgent`. When the
/// upload ends, the board re-enumerates. The session therefore hands nothing back,
/// forgets the maskrom device, and reports it bootstrapped.
#[test]
fn a_maskrom_bootstrap_uploads_the_loader_and_forgets_the_board() {
    let data_471 = vec![0x11u8; 100];
    let data_472 = vec![0x22u8; 50];
    let loader = LoaderImage {
        chip: None,
        code_471: vec![CodeBlob {
            name: "UsbHead".to_string(),
            data: data_471.clone(),
            delay_ms: 0,
        }],
        code_472: vec![CodeBlob {
            name: "Loader".to_string(),
            data: data_472.clone(),
            delay_ms: 0,
        }],
        rc4_disabled: true,
    };

    let mut steps = expect_control(0x0471, &rkboot::download_payload(&data_471));
    steps.extend(expect_control(0x0472, &rkboot::download_payload(&data_472)));

    // A maskrom board, selected as the target but never opened as an agent.
    let mut session: Session<ScriptedTransport> = Session::new(ConfirmBy::Coordinate);
    let mut maskrom = a_device("003", 12);
    maskrom.mode = Mode::Maskrom;
    maskrom.bcd_usb = 0x0200; // even: the BootROM is running
    session.devices_seen(vec![maskrom.clone()], 0.0);
    session.select_target(maskrom.clone(), maskrom.clone());

    let work = session
        .start_bootstrap(
            ScriptedTransport::new(steps),
            loader,
            None,
            Some(maskrom.clone()),
            0.0,
            no_wake(),
        )
        .expect("nothing else is running, so a bootstrap can start");
    assert!(session.bootstrap.is_some(), "the bootstrap is in flight");
    assert!(
        session
            .start_bootstrap(
                ScriptedTransport::new(vec![]),
                a_bare_loader(),
                None,
                Some(maskrom.clone()),
                0.0,
                no_wake()
            )
            .is_none(),
        "a second bootstrap cannot start while one runs"
    );

    pollster::block_on(work.run());
    assert!(
        session.harvest_bootstrap(),
        "the upload ended, so it was harvested"
    );

    assert!(session.bootstrap.is_none(), "the bootstrap is done");
    assert!(
        session.target.device.is_none(),
        "the maskrom board is forgotten -- it re-enumerates as a different device"
    );
    assert!(
        matches!(session.last, Some(Ok(Report::Bootstrapped { .. }))),
        "and it is reported bootstrapped"
    );
}

/// A bootstrap forgets the board it uploaded to, and only that board.
///
/// Nothing stops a person re-pointing the slot while an upload runs. A maskrom board
/// is not a `FlashAgent`, so the target's connection reads `Disconnected` throughout
/// and every `Use` button stays live. Resetting the slot unconditionally at the end
/// of the upload would replace the whole `Board`, whatever it held by then. That
/// would drop a second board's identity while its agent is still held. The agent
/// would hold an open interface, or a disk under `O_EXCL`, with no panel drawn and
/// no way to close it.
///
/// The handle is therefore compared. This test runs the same upload twice: once with
/// the slot left where it was, and once with it re-pointed at another board partway
/// through.
#[test]
fn a_bootstrap_forgets_the_board_it_uploaded_to_and_leaves_any_other_alone() {
    fn run(repoint_to: Option<DeviceInfo>) -> Session<ScriptedTransport> {
        let data_471 = vec![0xa5; 8];
        let loader = LoaderImage {
            code_471: vec![CodeBlob {
                name: "471".to_string(),
                data: data_471.clone(),
                delay_ms: 0,
            }],
            code_472: Vec::new(),
            chip: None,
            rc4_disabled: true,
        };
        let steps = expect_control(0x0471, &rkboot::download_payload(&data_471));

        let mut session: Session<ScriptedTransport> = Session::new(ConfirmBy::Coordinate);
        let mut maskrom = a_device("003", 12);
        maskrom.mode = Mode::Maskrom;
        session.devices_seen(vec![maskrom.clone()], 0.0);
        session.select_target(maskrom.clone(), maskrom.clone());

        let work = session
            .start_bootstrap(
                ScriptedTransport::new(steps),
                loader,
                None,
                Some(maskrom.clone()),
                0.0,
                no_wake(),
            )
            .expect("nothing else is running");

        // The window in which the mistake happens: the upload is in flight and
        // the slot can still be re-pointed.
        if let Some(other) = repoint_to {
            assert!(
                session.select_target(other.clone(), other),
                "the slot is re-pointed while the upload runs"
            );
        }

        pollster::block_on(work.run());
        assert!(session.harvest_bootstrap(), "the upload ended");
        session
    }

    let left_alone = run(None);
    assert!(
        left_alone.target.device.is_none(),
        "the board that was uploaded to is forgotten -- it re-enumerates as a different device"
    );

    let other = a_device("003", 14);
    let repointed = run(Some(other.clone()));
    assert_eq!(
        repointed
            .target
            .device
            .as_ref()
            .and_then(Chosen::usb)
            .map(|device| device.device_address),
        Some(14),
        "a slot somebody re-pointed mid-upload keeps the board they pointed it at"
    );
}

/// A bootstrap holds the flow, so no verb starts on another board while it runs.
///
/// `start_bootstrap` refuses while a job runs, and this test pins the other half.
/// Without that guard, a person could point the target at a second board
/// mid-upload, open it, and start a verb. The harvest would then put that board's
/// agent back on a slot the bootstrap had emptied.
#[test]
fn no_job_starts_while_a_bootstrap_holds_the_flow() {
    let mut session = a_session(scripted_info(1));
    let work = session
        .start_bootstrap(
            ScriptedTransport::new(Vec::new()),
            a_bare_loader(),
            None,
            None,
            0.0,
            no_wake(),
        )
        .expect("nothing else is running");

    assert!(
        session.start(Task::Info, 0.0, no_wake()).is_none(),
        "a verb cannot start while a bootstrap holds the flow"
    );
    assert!(
        session.target.connection.is_idle(),
        "and the board it would have used was not taken"
    );

    drop(work);
}

/// An Ingenic boot-ROM bootstrap identifies, uploads, and forgets its board.
///
/// This is the Ingenic counterpart of the maskrom bootstrap test. The upload sends
/// `VR_*` control transfers and bulk chunks to a board that is not a `FlashAgent`,
/// and reads the SoC magic on the way. On the `PROGRAM_START2` jump the board
/// re-enumerates. The session therefore hands nothing back, forgets the boot-ROM
/// device, and reports the magic it read for a person to pin.
#[test]
fn an_ingenic_bootstrap_identifies_uploads_and_forgets_the_board() {
    use pyrographer_core::bootstrap::ingenic::{IngenicLoader, Stage};
    use pyrographer_core::codec::ingenic_boot::{self, Setup};

    const MAGIC: [u8; 8] = *b"T31\0\0\0\0\0";

    let stage1 = Stage {
        name: "spl".to_string(),
        data: vec![0x11u8; 100],
        load_address: 0x8000_0000,
        entry_address: 0x8000_0000,
        settle_ms: 0, // no real sleep in a test
    };
    let stage2 = Stage {
        name: "uboot".to_string(),
        data: vec![0x22u8; 50],
        load_address: 0x8010_0000,
        entry_address: 0x8010_0000,
        settle_ms: 0,
    };
    let loader = IngenicLoader {
        stage1: stage1.clone(),
        stage2: Some(stage2.clone()),
    };

    // The conversation a faithful bootstrap makes: identify, then each stage's
    // address, length, bulk payload, and jump -- the exact bytes the scripted
    // transport asserts as core's own driver test does.
    let control = |setup: Setup| Step::ExpectControlOut {
        request_type: setup.request_type,
        request: setup.request,
        value: setup.value,
        index: setup.index,
        data: Vec::new(),
    };
    let identify = {
        let setup = ingenic_boot::get_cpu_info();
        Step::ExpectControlIn {
            request_type: setup.request_type,
            request: setup.request,
            value: setup.value,
            index: setup.index,
            length: setup.length,
            reply: MAGIC.to_vec(),
        }
    };
    let mut steps = vec![identify];
    for (stage, start) in [
        (&stage1, ingenic_boot::program_start1 as fn(u32) -> Setup),
        (&stage2, ingenic_boot::program_start2 as fn(u32) -> Setup),
    ] {
        steps.push(control(ingenic_boot::set_data_address(stage.load_address)));
        steps.push(control(ingenic_boot::set_data_length(
            stage.data.len() as u32
        )));
        steps.push(Step::ExpectWrite(stage.data.clone()));
        steps.push(control(start(stage.entry_address)));
    }

    // A boot-ROM board, selected as the target but never opened as an agent.
    let mut session: Session<ScriptedTransport> = Session::new(ConfirmBy::Coordinate);
    let mut bootrom = a_device("003", 12);
    bootrom.vendor = Vendor::Ingenic;
    bootrom.vendor_id = 0xa108;
    bootrom.product_id = 0xc309;
    bootrom.mode = Mode::BootRom;
    session.devices_seen(vec![bootrom.clone()], 0.0);
    session.select_target(bootrom.clone(), bootrom.clone());

    let work = session
        .start_ingenic_bootstrap(
            ScriptedTransport::new(steps),
            loader,
            Some(bootrom.clone()),
            0.0,
            no_wake(),
        )
        .expect("nothing else is running, so a bootstrap can start");
    assert!(
        session.ingenic_bootstrap.is_some(),
        "the bootstrap is in flight"
    );

    pollster::block_on(work.run());
    assert!(
        session.harvest_ingenic_bootstrap(),
        "the upload ended, so it was harvested"
    );

    assert!(session.ingenic_bootstrap.is_none(), "the bootstrap is done");
    assert!(
        session.target.device.is_none(),
        "the boot-ROM board is forgotten -- it re-enumerates as a DFU gadget"
    );
    let Some(Ok(Report::IngenicBootstrapped(cpu))) = &session.last else {
        panic!("expected a bootstrapped report carrying the magic");
    };
    assert_eq!(cpu.magic, MAGIC, "the SoC magic rode out on the report");
}

/// A loader with nothing to upload, for the second-start refusal in
/// `a_maskrom_bootstrap_uploads_the_loader_and_forgets_the_board`.
fn a_bare_loader() -> LoaderImage {
    LoaderImage {
        chip: None,
        code_471: vec![CodeBlob {
            name: "UsbHead".to_string(),
            data: vec![0u8; 8],
            delay_ms: 0,
        }],
        code_472: vec![],
        rc4_disabled: true,
    }
}

/// A recovery file, as a person picks one.
fn a_blob(bytes: Vec<u8>) -> PickedBlob {
    PickedBlob {
        name: "blob.bin".to_string(),
        bytes,
    }
}

/// The steps a receiver plays for a clean XMODEM send of `data`.
///
/// The receiver sends a short `C` storm, then an `ACK` for every 128-byte block and
/// for the `EOT`. It is the same conversation `recovery.rs` scripts in core, so the
/// bytes this asserts are the bytes a real recovery sends.
fn scripted_send(data: &[u8]) -> Vec<SerialStep> {
    let mut steps = vec![SerialStep::Rx(vec![xmodem::CRC_REQUEST; 4])];
    let mut seq = 1u8;
    for chunk in data.chunks(xmodem::BLOCK_DATA) {
        steps.push(SerialStep::ExpectTx(xmodem::block(seq, chunk)));
        steps.push(SerialStep::Rx(vec![xmodem::ACK]));
        seq = seq.wrapping_add(1);
    }
    steps.push(SerialStep::ExpectTx(vec![xmodem::EOT]));
    steps.push(SerialStep::Rx(vec![xmodem::ACK]));
    steps
}

/// Type `what` into a waiting recovery plan's port confirmation.
fn type_recovery_port(session: &mut Session<ScriptedTransport>, what: &str) {
    let pending = session
        .recovery
        .pending
        .as_mut()
        .expect("a recovery plan is waiting");
    let Confirmation::Typed { typed, .. } = &mut pending.confirmation else {
        panic!("the native window asks for the port");
    };
    *typed = what.to_string();
}

/// A confirmed recovery, minted through the whole gate (plan, type the port,
/// confirm), for a test that needs one to hand to `start_recovery`.
fn a_confirmed_recovery() -> (OwnedRecoveryRequest, ConfirmedRecovery) {
    let mut session: Session<ScriptedTransport> = Session::new(ConfirmBy::Coordinate);
    session.set_recovery_agent(Some(a_blob(vec![0u8; 8])));
    session.set_recovery_spl(Some(a_blob(vec![0u8; 8])));
    session
        .plan_recovery("/dev/ttyUSB0".to_string(), RecoveryTarget::NorFlash)
        .expect("a plan");
    type_recovery_port(&mut session, "/dev/ttyUSB0");
    let (_, request, confirmed) = session.confirm_recovery().expect("the port was typed");
    (request, confirmed)
}

/// The StarFive recovery flow, end to end, with neither a board nor a window.
///
/// Files are picked, and a plan is made without touching a serial line. The port is
/// typed to confirm, and the recovery runs against a scripted serial that asserts
/// every byte. The agent is uploaded first. The SPL is then headered for NOR and
/// sent under menu option `0`, and the menu is exited with `5`.
///
/// This test pins the state wiring (plan, confirm, start, harvest), and core's
/// `recovery` tests pin the framing. Together they pin that the GUI sends exactly
/// the bytes a recovery requires.
#[test]
fn a_recovery_plans_confirms_and_runs_against_a_scripted_serial() {
    let agent = vec![0xa9u8; 200]; // two blocks
    let spl_body = vec![0x5au8; 50];
    let framed_spl = splhdr::build(&spl_body, splhdr::Target::NorFlash);

    let mut session: Session<ScriptedTransport> = Session::new(ConfirmBy::Coordinate);
    session.set_recovery_agent(Some(a_blob(agent.clone())));
    session.set_recovery_spl(Some(a_blob(spl_body.clone())));

    session
        .plan_recovery("/dev/ttyUSB0".to_string(), RecoveryTarget::NorFlash)
        .expect("a plan");

    {
        let pending = session
            .recovery
            .pending
            .as_ref()
            .expect("a plan is waiting");
        assert!(
            !pending.plan.verified,
            "a StarFive recovery is never verified"
        );
        assert_eq!(pending.port, "/dev/ttyUSB0");
        assert_eq!(pending.plan.stages.len(), 1);
        assert_eq!(pending.plan.stages[0].menu_option, 0);
        assert_eq!(
            pending.plan.stages[0].image_bytes,
            (splhdr::HEADER_LEN + spl_body.len()) as u64,
            "the plan counts the headered size that crosses the wire, not the file on disk"
        );
    }

    type_recovery_port(&mut session, "/dev/ttyUSB0");
    let (port, request, confirmed) = session.confirm_recovery().expect("the port was typed");
    assert_eq!(port, "/dev/ttyUSB0", "the port is handed back to open");

    // The scripted receiver: agent, menu 0, framed SPL, menu 5 (exit). The
    // `ExpectTx` steps assert the exact bytes, so a wrong block fails here.
    let mut steps = scripted_send(&agent);
    steps.push(SerialStep::ExpectTx(vec![b'0']));
    steps.extend(scripted_send(&framed_spl));
    steps.push(SerialStep::ExpectTx(vec![b'5']));
    let serial = ScriptedSerial::new(steps);

    let work = session
        .start_recovery(serial, request, confirmed, 0.0, no_wake())
        .expect("nothing else is running, so a recovery can start");
    assert!(session.recovery.job.is_some(), "the recovery is in flight");

    let (other_request, other_confirmed) = a_confirmed_recovery();
    assert!(
        session
            .start_recovery(
                ScriptedSerial::new(vec![]),
                other_request,
                other_confirmed,
                0.0,
                no_wake(),
            )
            .is_none(),
        "a second recovery cannot start while one runs"
    );

    pollster::block_on(work.run());
    assert!(
        session.harvest_recovery(),
        "the recovery ended, so it was harvested"
    );
    assert!(session.recovery.job.is_none(), "the recovery is done");
    assert!(
        matches!(session.last, Some(Ok(Report::Recovered))),
        "and it is reported recovered"
    );
}

/// A recovery plan refuses what it can before a serial line is opened.
///
/// It refuses a plan with no agent, with nothing to write, or with no port named.
/// Each is a usage problem the plan catches, not a failure a board reports.
#[test]
fn planning_a_recovery_refuses_what_it_should() {
    let mut session: Session<ScriptedTransport> = Session::new(ConfirmBy::Coordinate);

    // No agent.
    assert!(matches!(
        session.plan_recovery("/dev/ttyUSB0".to_string(), RecoveryTarget::NorFlash),
        Err(Error::InvalidRequest(_))
    ));

    // An agent, but nothing to write.
    session.set_recovery_agent(Some(a_blob(vec![0u8; 16])));
    assert!(matches!(
        session.plan_recovery("/dev/ttyUSB0".to_string(), RecoveryTarget::NorFlash),
        Err(Error::InvalidRequest(_))
    ));

    // Something to write, but no port.
    session.set_recovery_spl(Some(a_blob(vec![0u8; 16])));
    assert!(matches!(
        session.plan_recovery("   ".to_string(), RecoveryTarget::NorFlash),
        Err(Error::InvalidRequest(_))
    ));

    // All three: a plan, and nothing was refused.
    session
        .plan_recovery("/dev/ttyUSB0".to_string(), RecoveryTarget::NorFlash)
        .expect("a plan");
    assert!(session.recovery.pending.is_some());
}

/// The serial flow has the same two guards as the USB write gate.
///
/// Changing any recovery input discards a plan made against the old files. A port
/// typed wrong confirms nothing and leaves the plan on the screen.
#[test]
fn a_recovery_plan_is_thrown_away_by_a_changed_file_and_confirms_nothing_on_a_wrong_port() {
    let mut session: Session<ScriptedTransport> = Session::new(ConfirmBy::Coordinate);
    session.set_recovery_agent(Some(a_blob(vec![0xa9u8; 32])));
    session.set_recovery_spl(Some(a_blob(vec![0x5au8; 32])));
    session
        .plan_recovery("/dev/ttyUSB0".to_string(), RecoveryTarget::NorFlash)
        .expect("a plan");

    // A port typed wrong confirms nothing, and the plan stays.
    type_recovery_port(&mut session, "/dev/ttyUSB1");
    assert!(session.confirm_recovery().is_none());
    assert!(
        session.recovery.pending.is_some(),
        "the plan is still on the screen"
    );

    // Changing any file throws the plan away: it described a write of the old ones.
    session.set_recovery_uboot(Some(a_blob(vec![0x33u8; 16])));
    assert!(
        session.recovery.pending.is_none(),
        "the plan described a write of the old files, and these are not those files"
    );
}

/// The serial line a console test runs over: the defaults, on a named port.
fn a_console_line() -> ConsoleLine {
    ConsoleLine {
        port: "/dev/ttyUSB0".to_string(),
        baud: 115_200,
        prompt: pyrographer_core::uboot::DEFAULT_PROMPT.to_string(),
        reads: 8,
    }
}

/// Run a console task to completion against a scripted serial, and harvest it.
fn run_console(
    session: &mut Session<ScriptedTransport>,
    steps: Vec<SerialStep>,
    task: ConsoleTask,
) {
    let work = session
        .start_console(
            ScriptedSerial::new(steps),
            &a_console_line(),
            task,
            0.0,
            no_wake(),
        )
        .expect("nothing else is running, so a session can start");
    pollster::block_on(work.run());
    assert!(
        session.harvest_console(),
        "the session ended, so it was harvested"
    );
}

/// The bytes a console puts back for a typed `command`: the echo, the output, and
/// the next prompt.
fn echoed(command: &str, output: &str) -> SerialStep {
    SerialStep::Rx(format!("{command}\r\n{output}=> ").into_bytes())
}

/// The passive watch, end to end, with neither a board nor a window.
///
/// A node prints its verdict to the console and the host reads it. The watch needs
/// no prompt and no echo handling, and uses no login or credentials.
#[test]
fn a_console_watch_reports_what_appeared_and_keeps_the_transcript() {
    let mut session: Session<ScriptedTransport> = Session::new(ConfirmBy::Coordinate);

    run_console(
        &mut session,
        vec![
            SerialStep::Rx(b"[    0.00] booting\r\n".to_vec()),
            SerialStep::Timeout,
            SerialStep::Rx(b"selftest: PASS\r\n".to_vec()),
        ],
        ConsoleTask::Watch {
            expect: vec![b"PASS".to_vec()],
            fail: vec![b"FAIL".to_vec()],
        },
    );

    let Some(Ok(Report::Console(ConsoleReport::Watched(watched)))) = &session.last else {
        panic!(
            "a watch reports what appeared: {:?}",
            session.last.is_some()
        );
    };
    assert_eq!(watched.seen, pyrographer_core::console::Seen::Expected);
    assert_eq!(watched.pattern, b"PASS");

    // The transcript outlives the job that read it, because a console's answer is
    // what it printed.
    assert!(session.console.job.is_none());
    assert!(
        session.console.transcript.contains("selftest: PASS"),
        "{}",
        session.console.transcript
    );
}

/// A failure pattern is a **finding, not a fault**.
///
/// The board answered, and what it said is what the watch was looking for. The
/// failure comes back as a report rather than an error, and the front-end decides
/// what to make of it.
#[test]
fn a_watched_failure_is_reported_rather_than_raised() {
    let mut session: Session<ScriptedTransport> = Session::new(ConfirmBy::Coordinate);

    run_console(
        &mut session,
        vec![SerialStep::Rx(b"stage1: FAIL\r\nstage2: PASS\r\n".to_vec())],
        ConsoleTask::Watch {
            expect: vec![b"PASS".to_vec()],
            fail: vec![b"FAIL".to_vec()],
        },
    );

    let Some(Ok(Report::Console(ConsoleReport::Watched(watched)))) = &session.last else {
        panic!("the failure is a report");
    };
    assert_eq!(
        watched.seen,
        pyrographer_core::console::Seen::Failed,
        "the FAIL came first in the stream, so it is what is reported"
    );
}

/// The boot override through the whole gate, and the bytes it puts on the wire.
///
/// The plan is produced by *asking the board*, so it arrives as a job's result. It
/// becomes a screen, not an entry in `session.last`. Confirming it runs the override
/// over the line the plan was asked over. The scripted serial asserts every byte:
/// `setenv` and `boot`, and **no `saveenv`**.
#[test]
fn a_boot_override_plans_confirms_and_sets_the_order_without_saving_it() {
    let mut session: Session<ScriptedTransport> = Session::new(ConfirmBy::Coordinate);

    // The dry run: interrupt the autoboot, ask what it boots from now.
    run_console(
        &mut session,
        vec![
            SerialStep::ExpectTx(b"\r".to_vec()),
            SerialStep::Rx(b"\r\n=> ".to_vec()),
            SerialStep::ExpectTx(b"printenv boot_targets\r".to_vec()),
            echoed("printenv boot_targets", "boot_targets=mmc1 usb0\r\n"),
        ],
        ConsoleTask::PlanBoot("mmc0".to_string()),
    );

    let pending = session
        .console
        .pending
        .as_ref()
        .expect("the plan is a screen waiting to be agreed to");
    assert_eq!(pending.plan.current.as_deref(), Some("mmc1 usb0"));
    assert_eq!(pending.plan.targets, "mmc0");
    assert!(!pending.plan.persistent, "the environment stays in RAM");
    assert_eq!(pending.line.port, "/dev/ttyUSB0");
    assert!(
        session.last.is_none(),
        "a plan is a screen, not something that happened"
    );

    let (line, confirmed) = session.confirm_boot().expect("a plain yes");
    assert_eq!(
        line,
        a_console_line(),
        "the override runs over the line the plan was asked over, not over whatever a form says \
         now"
    );

    // The override: interrupt again (the board is at its prompt), set, boot. Every
    // written byte is asserted, so a `saveenv` slipped in here would fail.
    run_console(
        &mut session,
        vec![
            SerialStep::ExpectTx(b"\r".to_vec()),
            SerialStep::Rx(b"\r\n=> ".to_vec()),
            SerialStep::ExpectTx(b"setenv boot_targets mmc0\r".to_vec()),
            echoed("setenv boot_targets mmc0", ""),
            SerialStep::ExpectTx(b"boot\r".to_vec()),
            SerialStep::Rx(b"boot\r\nstarting USB...\r\n".to_vec()),
            // The drain that follows a `boot`, spent on a line that has gone quiet.
            SerialStep::Timeout,
            SerialStep::Timeout,
            SerialStep::Timeout,
            SerialStep::Timeout,
            SerialStep::Timeout,
            SerialStep::Timeout,
        ],
        ConsoleTask::Boot(confirmed),
    );

    assert!(
        matches!(
            &session.last,
            Some(Ok(Report::Console(ConsoleReport::Booted(targets)))) if targets == "mmc0"
        ),
        "the board is booting from the order that was set"
    );
    assert!(session.console.pending.is_none(), "the plan is spent");
}

/// A boot override nobody agreed to mints nothing, and a dismissed plan leaves
/// nothing behind.
///
/// The confirmation is a plain yes. There is no coordinate to type, because an
/// override has no second board and destroys nothing.
#[test]
fn a_boot_override_that_is_dismissed_mints_nothing() {
    let mut session: Session<ScriptedTransport> = Session::new(ConfirmBy::Coordinate);
    assert!(
        session.confirm_boot().is_none(),
        "there is no plan to say yes to"
    );

    run_console(
        &mut session,
        vec![
            SerialStep::ExpectTx(b"\r".to_vec()),
            SerialStep::Rx(b"\r\n=> ".to_vec()),
            SerialStep::ExpectTx(b"printenv boot_targets\r".to_vec()),
            echoed("printenv boot_targets", "boot_targets=mmc1 usb0\r\n"),
        ],
        ConsoleTask::PlanBoot("mmc0".to_string()),
    );
    assert!(session.console.pending.is_some());

    session.dismiss_boot_plan();
    assert!(session.console.pending.is_none());
    assert!(session.confirm_boot().is_none());
}

/// A gadget that keeps the console is a successful start.
///
/// The signal is the wait for a prompt running out, because a running gadget never
/// gives the prompt back. The report carries the exact line that was typed.
#[test]
fn starting_the_gadget_reports_the_line_it_typed() {
    let mut session: Session<ScriptedTransport> = Session::new(ConfirmBy::Coordinate);

    run_console(
        &mut session,
        vec![
            SerialStep::ExpectTx(b"\r".to_vec()),
            SerialStep::Rx(b"\r\n=> ".to_vec()),
            SerialStep::ExpectTx(b"rockusb 0 mmc 0\r".to_vec()),
            SerialStep::Rx(b"rockusb 0 mmc 0\r\n".to_vec()),
            SerialStep::Timeout,
            SerialStep::Timeout,
            SerialStep::Timeout,
        ],
        ConsoleTask::Gadget(Gadget::Rockusb, GadgetDevice::default()),
    );

    assert!(
        matches!(
            &session.last,
            Some(Ok(Report::Console(ConsoleReport::GadgetStarted {
                gadget: Gadget::Rockusb,
                command,
            }))) if command == "rockusb 0 mmc 0"
        ),
        "the gadget is running and the report says what was typed"
    );
}

/// Mass storage runs through the same job, and its report names the gadget.
///
/// The board's flash then appears among the disks, not among the boards.
#[test]
fn starting_mass_storage_reports_which_gadget_it_was() {
    let mut session: Session<ScriptedTransport> = Session::new(ConfirmBy::Coordinate);

    run_console(
        &mut session,
        vec![
            SerialStep::ExpectTx(b"\r".to_vec()),
            SerialStep::Rx(b"\r\n=> ".to_vec()),
            SerialStep::ExpectTx(b"ums 0 mmc 0\r".to_vec()),
            SerialStep::Rx(b"ums 0 mmc 0\r\nUMS: LUN 0, dev mmc 0, hwpart 0\r\n".to_vec()),
            SerialStep::Timeout,
            SerialStep::Timeout,
            SerialStep::Timeout,
        ],
        ConsoleTask::Gadget(Gadget::Ums, GadgetDevice::default()),
    );

    assert!(
        matches!(
            &session.last,
            Some(Ok(Report::Console(ConsoleReport::GadgetStarted {
                gadget: Gadget::Ums,
                command,
            }))) if command == "ums 0 mmc 0"
        ),
        "the report names mass storage and the line that was typed"
    );
}

/// One session at a time runs on one line, and a second start takes nothing.
///
/// The transport the second start was handed is dropped, which closes the port it
/// would have used.
#[test]
fn a_second_console_session_cannot_start_while_one_runs() {
    let mut session: Session<ScriptedTransport> = Session::new(ConfirmBy::Coordinate);
    let line = a_console_line();

    let work = session
        .start_console(
            ScriptedSerial::new(vec![SerialStep::Rx(b"PASS".to_vec())]),
            &line,
            ConsoleTask::Watch {
                expect: vec![b"PASS".to_vec()],
                fail: vec![],
            },
            0.0,
            no_wake(),
        )
        .expect("the first one starts");

    assert!(
        session
            .start_console(
                ScriptedSerial::new(vec![]),
                &line,
                ConsoleTask::Watch {
                    expect: vec![b"PASS".to_vec()],
                    fail: vec![],
                },
                0.0,
                no_wake(),
            )
            .is_none(),
        "a second session cannot start while one runs"
    );

    pollster::block_on(work.run());
    assert!(session.harvest_console());
}

// ---------------------------------------------------------------------------
// A disk in the slot.
//
// The Block backend is the first target the operating system also owns, and in
// the window it is not a flow of its own: it goes in the same slot a board goes
// in and every verb is drawn once. What that costs is that a handful of things
// which were always about a board now have to answer for two kinds of device,
// and these are those things.
//
// The agent under them is the real one, over an ordinary file -- so the range
// checks, the bounce buffer and the read-back are core's own code, and no test
// here needs a disk or a privilege. What a file cannot stand in for -- `O_EXCL`,
// `O_DIRECT`, the kernel's refusals, real geometry -- is the bench's, and
// `bench/README.md` is where.
// ---------------------------------------------------------------------------

/// A scratch file standing in for a disk, and a session with it in the target
/// slot.
///
/// The file is named for the test that asked for it, so a failure leaves
/// something identifiable behind rather than a number.
#[cfg(target_os = "linux")]
fn a_disk_session(what: &str, sectors: u64) -> (std::path::PathBuf, Session<ScriptedTransport>) {
    let path =
        std::env::temp_dir().join(format!("pyrographer-gui-{what}-{}.img", std::process::id()));
    std::fs::write(&path, vec![0u8; (sectors * 512) as usize]).expect("a scratch file");
    let agent = pyrographer_core::block::agent_over_file(
        &path,
        sectors * 512,
        512,
        pyrographer_core::block::Access::Write,
    )
    .expect("an agent");
    let disk = agent.device().clone();

    let mut session = Session::new(ConfirmBy::Coordinate);
    assert!(session.select_target_disk(disk));
    session.target.opened(FlashAgent::Block(agent));
    (path, session)
}

/// A disk is confirmed by its node path.
///
/// A node path is a place in the same sense a bus coordinate is. It is what the
/// list prints and what `--device` takes. It also differs between two card readers
/// that report the identical model.
#[test]
#[cfg(target_os = "linux")]
fn a_disk_is_confirmed_by_typing_its_node_path() {
    let (path, session) = a_disk_session("node-path", 64);
    let chosen = session.target.device.as_ref().expect("a disk is chosen");

    assert_eq!(coordinate(chosen), path.display().to_string());
    assert!(
        matches!(
            Confirmation::asked_of(ConfirmBy::Coordinate, chosen),
            Confirmation::Typed { expected, .. } if expected == path.display().to_string()
        ),
        "the act asked for is the node path, the same act the serial flow asks for a port"
    );

    let _ = std::fs::remove_file(&path);
}

/// A disk's write plan names no SoC and is not refused for it.
///
/// The confirmation it asks for is the disk's node path, not a board's coordinate.
///
/// This is the counterpart of `a_plan_that_named_no_soc_cannot_be_confirmed_at_all`.
/// On a board, an unnamed SoC is enough to refuse the plan. A disk runs no loader,
/// so the wrong-loader gate does not apply. Both verdicts come from
/// `verbs::plan_refusal`, through the same `Pending::refused`. What the window shows
/// and what the write does therefore cannot drift apart on either kind of device.
#[test]
#[cfg(target_os = "linux")]
fn a_disks_plan_is_not_refused_for_naming_no_soc() {
    let (path, mut session) = a_disk_session("plan-no-soc", 64);

    run(
        &mut session,
        Task::PlanWrite {
            aim: Aim::Lba(0),
            image_bytes: 4096,
            soc: None,
        },
    );

    let pending = session.pending.as_mut().expect("a plan is waiting");
    assert!(
        pending.refused.is_none(),
        "a disk runs no loader, so the wrong-loader gate has nothing to be about: {:?}",
        pending.refused
    );

    match &mut pending.confirmation {
        Confirmation::Typed { expected, typed } => {
            assert_eq!(expected, &path.display().to_string());
            *typed = expected.clone();
        }
        Confirmation::Repicked { .. } => panic!("the native window asks for the node path"),
    }
    assert!(
        session.confirm().is_some(),
        "typed exactly right, the plan mints the write"
    );

    let _ = std::fs::remove_file(&path);
}

/// A write to a disk runs the same loop, with its read-back, and the bytes are on
/// the device afterwards.
///
/// There is one write path for both kinds of device. `flash` over a disk runs the
/// same windowed write-then-read-back loop it runs over a board. The loop consumes a
/// `FlashAgent::Block` instead of a rockusb agent.
#[test]
#[cfg(target_os = "linux")]
fn a_disk_write_lands_and_reads_back() {
    let (path, mut session) = a_disk_session("write", 64);
    let bytes: Vec<u8> = (0..2048usize).map(|i| (i % 251) as u8).collect();

    session.set_image(Some(an_image("an-image.bin", bytes.len() as u64)));
    run(
        &mut session,
        Task::PlanWrite {
            aim: Aim::Lba(0),
            image_bytes: bytes.len() as u64,
            soc: None,
        },
    );

    let pending = session.pending.as_mut().expect("a plan is waiting");
    if let Confirmation::Typed { expected, typed } = &mut pending.confirmation {
        *typed = expected.clone();
    }
    let Some(Confirmed::Write(confirmed)) = session.confirm() else {
        panic!("the plan mints a write");
    };

    run(
        &mut session,
        Task::Write {
            confirmed,
            image: Box::new(pyrographer_core::image::SyncReader::new(
                std::io::Cursor::new(bytes.clone()),
            )),
        },
    );

    assert!(
        matches!(session.last, Some(Ok(Report::Wrote { .. }))),
        "the write finished, and did not report an error"
    );
    let landed = std::fs::read(&path).expect("the file is readable");
    assert_eq!(
        &landed[..bytes.len()],
        &bytes[..],
        "the bytes are on the device"
    );

    let _ = std::fs::remove_file(&path);
}

/// A bus rescan does not forget a chosen disk.
///
/// A board that has left the bus is forgotten, because its row would name a device
/// that is not there. A disk is not on the bus at all, so a scan says nothing about
/// it either way. The slot also holds the node path a person is about to type into
/// the write gate.
#[test]
#[cfg(target_os = "linux")]
fn a_chosen_disk_survives_a_bus_rescan() {
    let (path, mut session) = a_disk_session("rescan", 64);
    session.target.close();

    session.devices_seen(vec![a_device("003", 12)], 1.0);
    assert!(
        session.target.device.is_some(),
        "a scan of the bus says nothing about a disk"
    );
    session.devices_seen(Vec::new(), 2.0);
    assert!(
        session.target.device.is_some(),
        "and an empty bus says nothing about one either"
    );

    let _ = std::fs::remove_file(&path);
}

/// A disk listing that failed is reported as a sentence, not as an empty list.
///
/// An empty list says *this machine has no disks*, which is false on every machine.
/// It would send a person looking for the card reader they can see, rather than
/// telling them the backend is not built for this platform.
#[test]
fn a_disk_listing_that_failed_says_so_rather_than_showing_nothing() {
    let mut disks = Disks::default();
    disks.seen(Ok(vec![]));
    assert!(disks.asked);
    assert!(disks.problem.is_none(), "an empty machine is an empty list");

    disks.seen(Err(Error::NotImplemented(
        "the Block backend is built for Linux only",
    )));
    assert!(
        disks
            .problem
            .as_deref()
            .is_some_and(|why| why.contains("Linux only")),
        "the reason is kept whole: {:?}",
        disks.problem
    );
    assert!(disks.listed.is_empty());
}
