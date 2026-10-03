//! A bench pass over the Block backend's read and write path, on real media.
//!
//! `blocklist` reports what the guards say about this machine's devices without
//! opening anything. This one opens a device and *uses* it: the open with its
//! `O_EXCL`/`O_DIRECT` refusals, the geometry, the partition read, the `dump`
//! verb, the range checks against a live device, and -- only when asked -- the
//! whole gated write path, `plan_write` to `flash` to `verify`, with the
//! per-window read-back doing its work in the middle.
//!
//! It is the instrument, not the backend. Everything it exercises is
//! `pyrographer-core`'s; what is here is the driving, the timing, and the
//! ceremony that keeps a bench run off the wrong disk.
//!
//! # Running
//!
//! The inventory needs no privilege:
//!
//! ```text
//! cargo run --example blockbench
//! ```
//!
//! Everything past it needs an open, which needs root or membership of `disk`.
//! Point it at a loop device backed by a scratch file, never at real media:
//!
//! ```text
//! truncate -s 256M /tmp/scratch.img
//! DEV=$(udisksctl loop-setup -f /tmp/scratch.img | grep -o '/dev/loop[0-9]*')
//! sudo ./target/release/examples/blockbench --target "$DEV" --confirm "$DEV" --write
//! ```
//!
//! The target is named twice, its backing file is printed before anything is
//! written, and a device larger than `--max-bytes` (8 GiB by default) is refused
//! outright: a bench that can reach a 2 TB disk by a typo is not a bench.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use pyrographer_core::agent::FlashAgent;
use pyrographer_core::block::{self, BlockDevice};
use pyrographer_core::fill::FillReport;
use pyrographer_core::image::{SyncReader, SyncWriter};
use pyrographer_core::progress::{Cancel, Progress};
use pyrographer_core::transport::UsbTransport;
use pyrographer_core::{Error, verbs};

/// The transport a `FlashAgent` is generic over. A `FlashAgent::Block` holds no
/// transport at all -- the device is a file -- but the enum is generic, so a
/// type has to be named. Any `Transport` would do; this is the native one.
type Agent = FlashAgent<UsbTransport>;

fn main() {
    let args = match Args::parse() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };

    let mut bench = Bench::default();
    match run(&args, &mut bench) {
        Ok(()) => {}
        Err(message) => {
            println!();
            println!("stopped: {message}");
            bench.record("the pass ran to the end", false, message);
        }
    }
    bench.report();
    if bench.failed() {
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------

/// What this run was asked to do.
struct Args {
    /// The device to open: `loop3` or `/dev/loop3`. Absent runs the inventory
    /// and stops.
    target: Option<String>,
    /// The same name again. A write is refused unless it matches.
    confirm: Option<String>,
    /// Run the destructive sections.
    write: bool,
    /// Open a device with something mounted on it. Separate from `--write`
    /// because measuring the mounted case is a deliberate act.
    allow_mounted: bool,
    /// Expect the open to be refused for want of **privilege**, and treat that
    /// as the result. The unprivileged pass measures where the elevation
    /// boundary falls: everything before the open answers at uid 1000, and the
    /// open itself does not. A bench that printed that as a failure would be
    /// teaching whoever reads it to skim past the word.
    expect_unprivileged: bool,
    /// Expect the open to be *refused* as in-use, and treat that as the result
    /// rather than as a failure. This is how the mounted case is measured: the
    /// claim the whole backend rests on is that `O_EXCL` refuses a device the
    /// kernel holds, and a claim is only measured by a run that would notice it
    /// failing.
    expect_busy: bool,
    /// Refuse a target bigger than this. The guard against a typo landing on a
    /// real disk.
    max_bytes: u64,
    /// How many bytes of the device to dump in the read section.
    dump_bytes: u64,
    /// How many bytes the write section lays down.
    image_bytes: u64,
    /// Where the write section starts.
    lba: u64,
    /// Where the dump, the image and the restore copy are kept.
    work: PathBuf,
    /// Leave the inventory out. A run that is one of several against the same
    /// machine prints it once, not once per pass.
    no_inventory: bool,
}

impl Args {
    /// Parse, hand-rolled: core takes no argument-parsing dependency and an
    /// example is not the place to introduce one.
    fn parse() -> Result<Args, String> {
        let mut args = Args {
            target: None,
            confirm: None,
            write: false,
            allow_mounted: false,
            expect_busy: false,
            expect_unprivileged: false,
            max_bytes: 8 << 30,
            dump_bytes: 16 << 20,
            image_bytes: (4 << 20) + 100,
            lba: 0,
            work: std::env::temp_dir().join("pyrographer-blockbench"),
            no_inventory: false,
        };
        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            let mut value = || -> Result<String, String> {
                it.next().ok_or_else(|| format!("{arg} needs a value"))
            };
            match arg.as_str() {
                "--target" => args.target = Some(value()?),
                "--confirm" => args.confirm = Some(value()?),
                "--write" => args.write = true,
                "--allow-mounted" => args.allow_mounted = true,
                "--expect-busy" => args.expect_busy = true,
                "--expect-unprivileged" => args.expect_unprivileged = true,
                "--max-bytes" => args.max_bytes = number(&value()?)?,
                "--dump-bytes" => args.dump_bytes = number(&value()?)?,
                "--image-bytes" => args.image_bytes = number(&value()?)?,
                "--lba" => args.lba = number(&value()?)?,
                "--work" => args.work = PathBuf::from(value()?),
                "--no-inventory" => args.no_inventory = true,
                "--help" | "-h" => {
                    println!("{USAGE}");
                    std::process::exit(0);
                }
                other => return Err(format!("unknown argument `{other}`\n\n{USAGE}")),
            }
        }
        Ok(args)
    }
}

/// A byte count, with the suffixes a person actually types.
fn number(text: &str) -> Result<u64, String> {
    let (digits, scale) = match text.as_bytes().last() {
        Some(b'k') | Some(b'K') => (&text[..text.len() - 1], 1 << 10),
        Some(b'm') | Some(b'M') => (&text[..text.len() - 1], 1 << 20),
        Some(b'g') | Some(b'G') => (&text[..text.len() - 1], 1 << 30),
        _ => (text, 1),
    };
    digits
        .parse::<u64>()
        .map_err(|_| format!("`{text}` is not a number"))?
        .checked_mul(scale)
        .ok_or_else(|| format!("`{text}` is too large"))
}

const USAGE: &str = "\
blockbench -- exercise the Block backend against real media

    --target <dev>       the device to open: loop3, or /dev/loop3
    --confirm <dev>      the same name again; required by --write
    --write              run the destructive sections
    --allow-mounted      open a device with something mounted on it
    --expect-busy        expect the open to be refused as in-use, and pass on it
    --expect-unprivileged
                         expect the open to be refused for want of privilege
    --max-bytes <n>      refuse a target bigger than this (default 8G)
    --dump-bytes <n>     how much to dump in the read section (default 16M)
    --image-bytes <n>    how much the write section lays down (default 4M+100)
    --lba <n>            the sector the write section starts at (default 0)
    --work <dir>         where the dump and the image are kept
    --no-inventory       do not print the inventory table

With no --target it runs the inventory and stops. Everything past the inventory
needs root or membership of `disk`.";

// ---------------------------------------------------------------------------
// The pass
// ---------------------------------------------------------------------------

/// One expectation and what happened to it.
#[derive(Default)]
struct Bench {
    checks: Vec<(String, bool, String)>,
}

impl Bench {
    /// Note what a check expected and what it got.
    fn record(&mut self, what: &str, ok: bool, detail: impl Into<String>) {
        let detail = detail.into();
        println!("  [{}] {what}: {detail}", if ok { "ok" } else { "FAIL" });
        self.checks.push((what.to_string(), ok, detail));
    }

    /// Whether anything did not hold.
    fn failed(&self) -> bool {
        self.checks.iter().any(|(_, ok, _)| !ok)
    }

    /// The summary a report is pasted from.
    fn report(&self) {
        println!();
        println!("== summary ==");
        for (what, ok, detail) in &self.checks {
            println!("{:<5} {what}: {detail}", if *ok { "ok" } else { "FAIL" });
        }
        let failed = self.checks.iter().filter(|(_, ok, _)| !ok).count();
        println!();
        println!("{} checks, {failed} failed", self.checks.len());
    }
}

fn run(args: &Args, bench: &mut Bench) -> Result<(), String> {
    let devices = section_inventory(args)?;

    let Some(target) = &args.target else {
        println!();
        println!("no --target: the inventory is the whole of an unprivileged pass.");
        return Ok(());
    };

    let device = section_target(args, target, &devices, bench)?;
    std::fs::create_dir_all(&args.work).map_err(|e| {
        format!(
            "cannot make the work directory {}: {e}",
            args.work.display()
        )
    })?;

    let Some(mut agent) = section_open(args, &device, bench)? else {
        println!();
        println!("the open was refused as expected; there is nothing further to run.");
        return Ok(());
    };
    section_read(args, &device, &mut agent, bench)?;
    section_range(&device, &mut agent, bench);

    if args.write {
        section_write(args, &device, &mut agent, bench)?;
        // The exclusive handle is released *before* the next open asks for one:
        // the kernel is holding this device on our behalf and would refuse the
        // reopen exactly as it refuses anybody else.
        drop(agent);
        let fresh = block::open_for(&device, block::Access::Write)
            .map_err(|e| format!("re-opening `{}` after the write: {e}", device.name))?;
        section_durability(args, &device, &mut FlashAgent::Block(fresh), bench);
    } else {
        println!();
        println!("== 6. write ==");
        println!("  skipped: --write was not passed.");
    }
    Ok(())
}

/// Section 1: what this machine has, and what the guards say about it. No
/// privilege, nothing opened.
fn section_inventory(args: &Args) -> Result<Vec<BlockDevice>, String> {
    let devices = block::list().map_err(|e| format!("cannot list block devices: {e}"))?;
    if args.no_inventory {
        return Ok(devices);
    }
    println!("== 1. inventory (unprivileged) ==");
    println!(
        "  {:<12} {:>16} {:>6} {:>6} {:<8} {:<5} {:<5} mounts",
        "device", "bytes", "lblk", "pblk", "bus", "rm", "ro"
    );
    for d in &devices {
        println!(
            "  {:<12} {:>16} {:>6} {:>6} {:<8} {:<5} {:<5} {}",
            d.name,
            d.bytes,
            d.logical_block,
            d.physical_block,
            d.bus.name(),
            d.removable,
            d.read_only,
            if d.mounts.is_empty() {
                "-".to_string()
            } else {
                d.mounts.join(",")
            }
        );
        if let Some(why) = block::write_refusal(d) {
            println!("               refused: {why}");
        }
    }
    Ok(devices)
}

/// Section 2: which device this is, said out loud, and the guards that keep a
/// bench off anything that matters.
fn section_target(
    args: &Args,
    target: &str,
    devices: &[BlockDevice],
    bench: &mut Bench,
) -> Result<BlockDevice, String> {
    println!();
    println!("== 2. target ==");
    let wanted = target.trim_start_matches("/dev/");
    let device = devices
        .iter()
        .find(|d| d.name == wanted)
        .cloned()
        .ok_or_else(|| format!("no block device called `{wanted}`"))?;

    println!("  name         {}", device.name);
    println!("  node         {}", device.node);
    println!(
        "  capacity     {} bytes ({:.1} MiB)",
        device.bytes,
        device.bytes as f64 / (1 << 20) as f64
    );
    println!(
        "  geometry     {} logical / {} physical",
        device.logical_block, device.physical_block
    );
    println!("  bus          {}", device.bus.name());
    println!("  model        {}", device.model.as_deref().unwrap_or("-"));
    println!("  removable    {}", device.removable);
    println!("  read-only    {}", device.read_only);
    println!(
        "  mounts       {}",
        if device.mounts.is_empty() {
            "-".to_string()
        } else {
            device.mounts.join(", ")
        }
    );
    match backing_file(&device.name) {
        Some(file) => println!("  backing file {file}"),
        None => println!("  backing file - (not a loop device)"),
    }

    // The guards. Each is this harness's, not the backend's: core refuses the
    // running system's disks itself, and everything here is about not pointing a
    // bench at media somebody wanted.
    if let Some(why) = block::write_refusal(&device) {
        return Err(format!("the backend refuses this device: {why}"));
    }
    if device.bytes > args.max_bytes {
        return Err(format!(
            "`{}` holds {} bytes, past the {} of --max-bytes. If this really is the scratch \
             device, raise the cap deliberately",
            device.name, device.bytes, args.max_bytes
        ));
    }
    if device.is_mounted() && !args.allow_mounted {
        return Err(format!(
            "`{}` has something mounted on it ({}); pass --allow-mounted to open it anyway",
            device.name,
            device.mounts.join(", ")
        ));
    }
    if args.write {
        match &args.confirm {
            Some(confirm) if confirm.trim_start_matches("/dev/") == wanted => {}
            Some(confirm) => {
                return Err(format!(
                    "--confirm named `{confirm}` and --target named `{target}`; a write needs \
                     the same device twice"
                ));
            }
            None => return Err("--write needs --confirm naming the same device".to_string()),
        }
    }
    bench.record(
        "the target is named twice and is not the running system's",
        true,
        format!("{} ({} bytes)", device.name, device.bytes),
    );
    Ok(device)
}

/// Section 3: the open, and what the kernel says when a second one asks.
///
/// `Ok(None)` is the measured refusal: the caller asked for a device the kernel
/// holds and said so with `--expect-busy`, the open was refused, and that is the
/// result rather than a failure to get one.
fn section_open(
    args: &Args,
    device: &BlockDevice,
    bench: &mut Bench,
) -> Result<Option<Agent>, String> {
    println!();
    println!("== 3. open ==");
    let expect_refusal = args.expect_busy || args.expect_unprivileged;
    let what = if args.expect_busy {
        "block::open is refused on a device the kernel holds"
    } else if args.expect_unprivileged {
        "block::open is refused without privilege, and says so in those words"
    } else {
        "block::open takes the device O_EXCL|O_DIRECT"
    };
    let agent = match (
        block::open_for(device, block::Access::Write),
        expect_refusal,
    ) {
        (Ok(agent), false) => {
            bench.record(what, true, format!("opened `{}`", device.name));
            agent
        }
        (Ok(_), true) => {
            bench.record(
                what,
                false,
                format!(
                    "`{}` opened, where the pass expected a refusal",
                    device.name
                ),
            );
            return Ok(None);
        }
        (Err(e), true) => {
            // The refusal has to be the one that was asked for. A device that is
            // in use and a device that cannot be opened at all are different
            // findings, and a pass that accepted either would report the wrong
            // one as a success.
            let as_asked = if args.expect_busy {
                matches!(&e, Error::InvalidRequest(m) if m.contains("in use"))
            } else {
                matches!(&e, Error::Io(m) if m.contains("cannot be opened without root"))
            };
            bench.record(what, as_asked, e.to_string());
            return Ok(None);
        }
        (Err(e), false) => {
            bench.record(what, false, e.to_string());
            return Err(format!("cannot open `{}`: {e}", device.name));
        }
    };

    // The kernel is holding it for us now, so it should refuse the next asker --
    // and this process is as good an asker as any other.
    match block::open_for(device, block::Access::Write) {
        Ok(_) => bench.record(
            "a second exclusive open is refused while the first is held",
            false,
            "the second open succeeded, so O_EXCL is not excluding",
        ),
        Err(e) => {
            let refused = matches!(&e, Error::InvalidRequest(m) if m.contains("in use"));
            bench.record(
                "a second exclusive open is refused while the first is held",
                refused,
                e.to_string(),
            );
        }
    }

    Ok(Some(FlashAgent::Block(agent)))
}

/// Section 4: the read path -- geometry, capabilities, the partition table, and
/// a `dump` through the verb every front-end would call.
fn section_read(
    args: &Args,
    device: &BlockDevice,
    agent: &mut Agent,
    bench: &mut Bench,
) -> Result<(), String> {
    println!();
    println!("== 4. read ==");

    let info = pollster::block_on(agent.info()).map_err(|e| format!("info: {e}"))?;
    println!(
        "  info         {} bytes, {}-byte sectors, chip_id {:?}, medium {:?}",
        info.size_bytes, info.sector_size, info.chip_id, info.medium
    );
    bench.record(
        "the agent's geometry agrees with sysfs",
        info.size_bytes == device.bytes && info.sector_size == device.logical_block,
        format!("{} bytes / {} bytes", info.size_bytes, info.sector_size),
    );

    let caps = agent.caps();
    println!("  caps         {caps:?}");
    println!("  read-back    {}", agent.read_back().describe());

    // One raw sector, straight off the device, before any verb runs.
    let mut sector = vec![0u8; device.logical_block as usize];
    match pollster::block_on(agent.read(args.lba, &mut sector)) {
        Ok(()) => {
            println!(
                "  sector {:<5} {}",
                args.lba,
                hex(&sector[..32.min(sector.len())])
            );
            bench.record(
                "a raw read at the target sector answers",
                true,
                "16 bytes shown above",
            );
        }
        Err(e) => {
            bench.record(
                "a raw read at the target sector answers",
                false,
                e.to_string(),
            );
            return Err(format!("reading sector {}: {e}", args.lba));
        }
    }

    // The partition read, over a block device: the same codecs every other
    // backend's table goes through, with the host's own disk underneath.
    match pollster::block_on(verbs::partitions(agent)) {
        Ok(Some(table)) => {
            println!(
                "  table        {:?}, {} partitions",
                table.format,
                table.partitions.len()
            );
            for p in &table.partitions {
                println!(
                    "               {:<20} lba {:>12} sectors {:>12}",
                    p.name, p.first_lba, p.sectors
                );
            }
            bench.record(
                "verbs::partitions reads a table over the block device",
                true,
                format!("{:?}, {} partitions", table.format, table.partitions.len()),
            );
        }
        Ok(None) => bench.record(
            "verbs::partitions reads a table over the block device",
            true,
            "no table on this device, reported as a finding rather than an error",
        ),
        Err(e) => bench.record(
            "verbs::partitions reads a table over the block device",
            false,
            e.to_string(),
        ),
    }

    // The dump verb: the read path a front-end actually drives.
    let want = args.dump_bytes.min(device.bytes);
    let sectors = want / u64::from(device.logical_block);
    let path = args.work.join(format!("{}-dump.bin", device.name));
    let file = std::fs::File::create(&path)
        .map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    let mut out = SyncWriter::new(std::io::BufWriter::new(file));
    let cancel = Cancel::new();
    let started = Instant::now();
    let report = pollster::block_on(verbs::dump(
        agent,
        args.lba,
        sectors,
        &mut out,
        &mut progress("dump"),
        &cancel,
    ));
    match report {
        Ok(fill) => {
            let bytes = sectors * u64::from(device.logical_block);
            let seconds = started.elapsed().as_secs_f64();
            println!(
                "  dumped       {bytes} bytes to {} in {seconds:.2}s ({:.1} MiB/s)",
                path.display(),
                bytes as f64 / (1 << 20) as f64 / seconds.max(f64::EPSILON)
            );
            print_fill(&fill);
            bench.record(
                "verbs::dump streams the device to a file",
                true,
                format!(
                    "{bytes} bytes at {:.1} MiB/s",
                    bytes as f64 / (1 << 20) as f64 / seconds.max(f64::EPSILON)
                ),
            );
        }
        Err(e) => {
            bench.record(
                "verbs::dump streams the device to a file",
                false,
                e.to_string(),
            );
            return Err(format!("dump: {e}"));
        }
    }
    Ok(())
}

/// Section 5: the range checks, against a device that is open. They refuse
/// before any I/O, so the agent should come out of this still usable -- which is
/// the half that a unit test cannot show.
fn section_range(device: &BlockDevice, agent: &mut Agent, bench: &mut Bench) {
    println!();
    println!("== 5. range checks on a live device ==");
    let block = u64::from(device.logical_block);
    let last = device.bytes / block - 1;

    let mut buf = vec![0u8; device.logical_block as usize + 1];
    match pollster::block_on(agent.read(0, &mut buf)) {
        Ok(()) => bench.record(
            "a length that is not a whole sector is refused",
            false,
            "it was allowed",
        ),
        Err(e) => bench.record(
            "a length that is not a whole sector is refused",
            matches!(e, Error::InvalidRequest(_)),
            e.to_string(),
        ),
    }

    let mut buf = vec![0u8; device.logical_block as usize];
    match pollster::block_on(agent.read(last + 1, &mut buf)) {
        Ok(()) => bench.record(
            "a read one sector past the end is refused",
            false,
            "it was allowed",
        ),
        Err(e) => bench.record(
            "a read one sector past the end is refused",
            matches!(e, Error::InvalidRequest(_)),
            e.to_string(),
        ),
    }

    // The refusals are arithmetic, made before the syscall, so none of them can
    // have left the agent out of step with the device.
    let desynchronized = agent.is_desynchronized();
    bench.record(
        "a refused request leaves the agent in step",
        !desynchronized,
        format!("is_desynchronized = {desynchronized}"),
    );
    match pollster::block_on(agent.read(last, &mut buf)) {
        Ok(()) => bench.record(
            "the last sector still reads after the refusals",
            true,
            format!("sector {last}: {}", hex(&buf[..16.min(buf.len())])),
        ),
        Err(e) => bench.record(
            "the last sector still reads after the refusals",
            false,
            e.to_string(),
        ),
    }
}

/// Section 6: the gated write path, end to end. Plan, refusal, confirmation,
/// `flash` with its per-window read-back, `verify`, and then the two things a
/// bench can ask that a unit test cannot -- whether the bytes are still there
/// through a fresh open, and whether a verify actually fails when the device
/// disagrees with the image.
fn section_write(
    args: &Args,
    device: &BlockDevice,
    agent: &mut Agent,
    bench: &mut Bench,
) -> Result<(), String> {
    println!();
    println!("== 6. write ==");

    // What is there now, kept whole, and shown as hex the way the probe does:
    // the point of a scratch device is that this does not matter, and the point
    // of printing it is that a person can tell when it does.
    let block = u64::from(device.logical_block);
    let region = args.image_bytes.div_ceil(block) * block;
    let before = args.work.join(format!("{}-before.bin", device.name));
    save_region(agent, args.lba, region, &before)?;
    let head = std::fs::read(&before).map_err(|e| format!("re-reading the saved region: {e}"))?;
    println!("  before       {}", hex(&head[..32.min(head.len())]));
    println!("  saved to     {}", before.display());

    // An offset-stamped pattern: every 16-byte group carries the byte offset it
    // sits at, so a window written to the wrong place is recognisable in a hex
    // dump rather than merely unequal.
    let image = args.work.join("image.bin");
    write_pattern(&image, args.image_bytes)?;
    println!(
        "  image        {} bytes at {} (offset-stamped)",
        args.image_bytes,
        image.display()
    );

    // The plan. Nothing has been written yet, and nothing here writes.
    let plan = pollster::block_on(verbs::plan_write(agent, args.lba, args.image_bytes, None))
        .map_err(|e| format!("plan_write: {e}"))?;
    println!(
        "  plan         lba {} for {} sectors",
        plan.lba, plan.sectors
    );
    println!(
        "               image {} bytes + {} padding",
        plan.image_bytes, plan.padding_bytes
    );
    println!("               touches {:?}", plan.touches);
    println!("               read-back {}", plan.read_back.describe());
    println!("               chip version {:?}", plan.chip_version);
    bench.record(
        "plan_write pads a part-sector image up to whole sectors",
        plan.padding_bytes == (region - args.image_bytes),
        format!("{} bytes of padding", plan.padding_bytes),
    );

    // The gate, asked rather than tripped over. A block device runs no loader,
    // so the wrong-loader question has no counterpart and this should be None --
    // the refusal that matters here happened at the open.
    match verbs::plan_refusal(agent, &plan) {
        None => bench.record(
            "plan_refusal passes a block write with no SoC named",
            true,
            "no refusal: there is no loader to be wrong, and the guard ran at the open",
        ),
        Some(e) => bench.record(
            "plan_refusal passes a block write with no SoC named",
            false,
            e.to_string(),
        ),
    }

    // The write itself.
    let confirmed = plan.confirm();
    let mut reader = SyncReader::new(std::io::BufReader::new(
        std::fs::File::open(&image).map_err(|e| format!("opening the image: {e}"))?,
    ));
    let cancel = Cancel::new();
    let started = Instant::now();
    match pollster::block_on(verbs::flash(
        agent,
        confirmed,
        &mut reader,
        &mut progress("flash"),
        &cancel,
    )) {
        Ok(()) => {
            let seconds = started.elapsed().as_secs_f64();
            bench.record(
                "verbs::flash writes and reads back every window",
                true,
                format!(
                    "{} bytes in {seconds:.2}s ({:.1} MiB/s, read-back included)",
                    args.image_bytes,
                    args.image_bytes as f64 / (1 << 20) as f64 / seconds.max(f64::EPSILON)
                ),
            );
        }
        Err(e) => {
            bench.record(
                "verbs::flash writes and reads back every window",
                false,
                e.to_string(),
            );
            return Err(format!("flash: {e}"));
        }
    }

    // Everything through to the medium, not merely submitted.
    match pollster::block_on(agent.finish_write()) {
        Ok(_) => bench.record(
            "the agent flushes to the device",
            true,
            "finish_write reached sync_data",
        ),
        Err(e) => bench.record("the agent flushes to the device", false, e.to_string()),
    }

    verify_against(
        agent,
        args,
        &image,
        bench,
        "verbs::verify agrees with the image",
    );

    println!(
        "  note         the region holds the pattern now; {} has what was there before",
        before.display()
    );
    Ok(())
}

/// Section 7: the two questions only a device can answer.
///
/// Are the bytes still there through a handle opened *after* the write -- which
/// is what makes a read-back a statement about the device rather than about a
/// page the host was holding? And does a verify actually fail when the device
/// disagrees with the image? A checking instrument that cannot fail is not
/// checking.
fn section_durability(args: &Args, device: &BlockDevice, agent: &mut Agent, bench: &mut Bench) {
    println!();
    println!("== 7. through a fresh exclusive open ==");
    let image = args.work.join("image.bin");
    let block = u64::from(device.logical_block);
    let region = args.image_bytes.div_ceil(block) * block;
    let cancel = Cancel::new();

    verify_against(
        agent,
        args,
        &image,
        bench,
        "the write is still there through a fresh exclusive open",
    );

    // And the other direction: a verify that ought to fail. One sector in the
    // middle is overwritten raw -- no gate, no read-back -- and the verify is
    // asked again. A checking instrument that cannot fail is not checking.
    let middle = args.lba + (region / block) / 2;
    let poison = vec![0xA5u8; device.logical_block as usize];
    if let Err(e) = pollster::block_on(agent.write(middle, &poison)) {
        bench.record(
            "a verify fails when the device disagrees with the image",
            false,
            format!("the sector could not be poisoned: {e}"),
        );
        return;
    }
    if let Err(e) = pollster::block_on(agent.finish_write()) {
        bench.record(
            "a verify fails when the device disagrees with the image",
            false,
            format!("the poison could not be flushed: {e}"),
        );
        return;
    }
    let Ok(file) = std::fs::File::open(&image) else {
        bench.record(
            "a verify fails when the device disagrees with the image",
            false,
            "the image could not be re-opened",
        );
        return;
    };
    let mut reader = SyncReader::new(std::io::BufReader::new(file));
    match pollster::block_on(verbs::verify(
        agent,
        args.lba,
        &mut reader,
        args.image_bytes,
        &mut |_| {},
        &cancel,
    )) {
        Ok(_) => bench.record(
            "a verify fails when the device disagrees with the image",
            false,
            "it passed, with one sector deliberately wrong",
        ),
        Err(Error::VerifyMismatch {
            offset,
            found,
            expected,
        }) => {
            let want = (middle - args.lba) * block;
            bench.record(
                "a verify fails when the device disagrees with the image",
                offset == want,
                format!(
                    "VerifyMismatch at byte {offset} (found {found:#04x}, expected \
                     {expected:#04x}); the poisoned sector starts at {want}"
                ),
            );
        }
        Err(e) => bench.record(
            "a verify fails when the device disagrees with the image",
            false,
            format!("it failed, but not as a mismatch: {e}"),
        ),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Run `verify` over the image and record what it said.
fn verify_against(agent: &mut Agent, args: &Args, image: &Path, bench: &mut Bench, what: &str) {
    let file = match std::fs::File::open(image) {
        Ok(file) => file,
        Err(e) => {
            bench.record(what, false, format!("cannot open the image: {e}"));
            return;
        }
    };
    let mut reader = SyncReader::new(std::io::BufReader::new(file));
    let cancel = Cancel::new();
    match pollster::block_on(verbs::verify(
        agent,
        args.lba,
        &mut reader,
        args.image_bytes,
        &mut |_| {},
        &cancel,
    )) {
        Ok(fill) => {
            print_fill(&fill);
            bench.record(what, true, format!("{} bytes compared", args.image_bytes));
        }
        Err(e) => bench.record(what, false, e.to_string()),
    }
}

/// Keep what is on the device before the write, so a bench run on something
/// that turns out to have mattered is recoverable.
fn save_region(agent: &mut Agent, lba: u64, bytes: u64, path: &Path) -> Result<(), String> {
    let sector = u64::from(agent.sector_size());
    let file = std::fs::File::create(path)
        .map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    let mut out = SyncWriter::new(std::io::BufWriter::new(file));
    let cancel = Cancel::new();
    pollster::block_on(verbs::dump(
        agent,
        lba,
        bytes / sector,
        &mut out,
        &mut |_| {},
        &cancel,
    ))
    .map_err(|e| format!("saving the region: {e}"))?;
    Ok(())
}

/// An image whose every 16-byte group carries its own byte offset, so a window
/// that lands in the wrong place is visible as a wrong number rather than as a
/// difference.
fn write_pattern(path: &Path, bytes: u64) -> Result<(), String> {
    let file = std::fs::File::create(path)
        .map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    let mut out = std::io::BufWriter::new(file);
    let mut at = 0u64;
    while at < bytes {
        // Eight bytes of offset, then a tag, so a window that landed in the
        // wrong place reads as a wrong number in a hex dump rather than merely
        // as a difference. The tag is exactly the eight bytes left.
        let mut group = [0u8; 16];
        group[..8].copy_from_slice(&at.to_le_bytes());
        group[8..].copy_from_slice(b"pyrograf");
        let take = 16.min((bytes - at) as usize);
        out.write_all(&group[..take])
            .map_err(|e| format!("writing the pattern: {e}"))?;
        at += take as u64;
    }
    out.flush()
        .map_err(|e| format!("flushing the pattern: {e}"))?;
    Ok(())
}

/// The `/sys` entry that says which file a loop device is standing in for. The
/// loop number is not stable between runs; this is what identifies the target.
fn backing_file(name: &str) -> Option<String> {
    std::fs::read_to_string(format!("/sys/block/{name}/loop/backing_file"))
        .ok()
        .map(|s| s.trim().to_string())
}

/// A progress sink that prints a line every tenth, so a long run says something
/// without a line per window.
fn progress(what: &'static str) -> impl FnMut(Progress) {
    let mut last = 0u64;
    move |p| match p {
        Progress::Started { total_bytes } => println!("  {what}: {total_bytes} bytes"),
        Progress::Advanced {
            done_bytes,
            total_bytes,
        } => {
            let tenth = total_bytes / 10;
            if tenth > 0 && done_bytes / tenth > last {
                last = done_bytes / tenth;
                println!("  {what}: {done_bytes}/{total_bytes}");
            }
        }
        Progress::Finished { done_bytes } => println!("  {what}: done, {done_bytes} bytes"),
    }
}

/// What the fill scanner made of what came back.
fn print_fill(report: &FillReport) {
    for run in report.runs() {
        println!(
            "  fill         {} sectors of {:#04x} from lba {} ({}){}",
            run.sectors(),
            run.byte(),
            run.first_lba(),
            run.bytes(),
            if run.is_blank() {
                ", blank"
            } else {
                ", SUSPICIOUS"
            }
        );
    }
    if report.is_empty() {
        println!("  fill         nothing constant");
    }
}

/// Bytes, as a person reads them off a dump.
fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}
