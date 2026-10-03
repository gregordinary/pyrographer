//! Everything that differs between the native window and the browser tab.
//!
//! The GUI has three seams: acquiring a board, acquiring a serial port, and the
//! image file. The first two have the same shape. Natively a device is a *place*
//! the host can be told to open, such as a bus address or a port path. In a tab it
//! is an *object* that a chooser hands back after a user gesture, and that object
//! is itself the permission. Everything else is the same code in both builds: all
//! of core (the codecs, agents, verbs, errors, plan and gate) and the whole of
//! [`state`].
//!
//! [`blocks`] is here for a different reason. The operating system that is already
//! using a disk enumerates it, so a disk is not acquired the way a board is. The
//! backend exists only on Linux, so what differs there is the *host* rather than
//! the window. It lives in this module because this module keeps what differs
//! between builds. It is the only thing here that a non-Linux desktop refuses as
//! surely as a tab does.
//!
//! Spawning is here too, and it is not a seam, because its callers cannot tell the
//! builds apart. Natively a job is a `std::thread` running `pollster::block_on`. In
//! a tab it is a `spawn_local` on the browser's own event loop. In both builds it
//! is *one task per job*, holding what it needs because the closure carried it in.
//! There is no worker and no command channel. A channel that worked in both builds
//! would cost an async-mpsc dependency, to carry a message the closure already
//! carries.
//!
//! Both builds present the same items, so no caller outside this module knows which
//! build it is in:
//!
//! - [`Wire`]
//! - [`Handle`]
//! - [`SerialWire`]
//! - [`PortHandle`]
//! - [`Wake`]
//! - [`spawn`]
//! - [`open`]
//! - [`open_serial_at`]
//! - [`port_name`]
//! - A pair of picked files (`PickedImage` and `PickedSink`)
//!
//! The web half compiles, and is **\[UNVERIFIED\]** in a real browser. The code
//! that consumes it is the same code the native window runs and the tests pin.
//!
//! [`state`]: crate::state

use std::future::Future;

use pyrographer_core::image::{ImageReader, ImageWriter};
use pyrographer_core::{Error, Result};

/// The transport this build talks to a board over.
///
/// Natively it is nusb over the operating system's USB stack. In a tab it is
/// WebUSB. [`state`](crate::state) is generic over it and cannot tell which, so a
/// scripted mock stands in for both under test.
#[cfg(not(target_arch = "wasm32"))]
pub type Wire = pyrographer_core::transport::UsbTransport;

/// The transport this build talks to a board over.
#[cfg(target_arch = "wasm32")]
pub type Wire = pyrographer_core::transport::WebUsbTransport;

/// What it takes to open a board.
///
/// This type is the acquisition seam. Natively a board is a *place*, a bus and an
/// address, and opening it means finding the device there. A browser cannot
/// enumerate a bus, and cannot be told to open the device at `3:14`. It hands back
/// a device *object* instead, and that object is itself the permission. The two
/// builds therefore carry different types here, and [`open`] opens whichever one
/// the build carries.
#[cfg(not(target_arch = "wasm32"))]
pub type Handle = pyrographer_core::discovery::DeviceInfo;

/// What it takes to open a board: the device object the browser handed over.
#[cfg(target_arch = "wasm32")]
pub type Handle = web_sys::UsbDevice;

/// The serial line this build recovers a StarFive board over, and drives a
/// bootloader prompt on.
///
/// This is the second transport seam, and the serial counterpart of [`Wire`].
/// Natively it is [`SerialTransport`](pyrographer_core::transport::SerialTransport),
/// serial2 over the operating system's serial stack. In a tab it is
/// `WebSerialTransport`, Web Serial's implementation of the same
/// [`Serial`](pyrographer_core::transport::Serial) seam. The code that consumes it
/// is the same in both builds:
///
/// - The XMODEM blocks
/// - The SPL header
/// - The recovery sender
/// - The console's transcript and cursor
/// - The U-Boot driver
/// - All of the recovery and console state
///
/// A scripted serial stands in for both builds under test, as a scripted transport
/// stands in for the USB one.
#[cfg(not(target_arch = "wasm32"))]
pub type SerialWire = pyrographer_core::transport::SerialTransport;

/// The serial line this build drives: Web Serial's implementation of the seam.
#[cfg(target_arch = "wasm32")]
pub type SerialWire = pyrographer_core::transport::WebSerialTransport;

/// What it takes to open a serial port.
///
/// This type is the serial acquisition seam. It differs between the builds as
/// [`Handle`] does, and for the same reason. Natively a port is a *place*: a path
/// a person types, such as `/dev/ttyUSB0` or `COM3`, and the open goes to that
/// path. A page cannot open a path. It is *given* a port object by a chooser it
/// did not draw and cannot script, and that object is itself the permission.
///
/// Natively the open is a syscall that returns. In a tab both the pick and the open
/// are promises. [`open_serial_at`] is therefore `async` in both builds, and every
/// caller sees that. The three flows that open a port (a recovery, a console
/// session and a boot override) share one task-and-slot path rather than three.
/// The path has the same shape in both builds, and only this type differs.
#[cfg(not(target_arch = "wasm32"))]
pub type PortHandle = String;

/// What it takes to open a serial port: the port object the browser handed over.
#[cfg(target_arch = "wasm32")]
pub type PortHandle = web_sys::SerialPort;

/// How a job says something changed.
///
/// A job runs outside the frame loop (on another thread, or in a later turn of the
/// browser's event loop) and knows nothing of the frame loop. egui repaints on
/// input, and a transfer is not input. Without this closure, a progress bar would
/// stay still until the mouse moved over it. The app hands each job a closure that
/// calls `egui::Context::request_repaint`. The `Context` is `Send + Sync` and cheap
/// to clone for this use.
///
/// It is a closure rather than an egui type because [`state`](crate::state) holds
/// one, and [`state`](crate::state) has no egui in it.
#[cfg(not(target_arch = "wasm32"))]
pub type Wake = Box<dyn Fn() + Send + 'static>;

/// How a job says something changed.
///
/// It is not `Send`, because a tab has one thread. A job there runs in a later
/// turn of the event loop, not on another thread.
#[cfg(target_arch = "wasm32")]
pub type Wake = Box<dyn Fn() + 'static>;

/// Where a write's bytes come from, once somebody has picked them.
///
/// It is `Send` natively, because the job that reads it runs on a thread of its
/// own. The browser build's reader is not `Send`, and does not need to be.
#[cfg(not(target_arch = "wasm32"))]
pub type BoxedReader = Box<dyn ImageReader + Send>;

/// Where a write's bytes come from, once somebody has picked them.
#[cfg(target_arch = "wasm32")]
pub type BoxedReader = Box<dyn ImageReader>;

/// Where a dump's bytes go.
#[cfg(not(target_arch = "wasm32"))]
pub type BoxedWriter = Box<dyn ImageWriter + Send>;

/// Where a dump's bytes go.
#[cfg(target_arch = "wasm32")]
pub type BoxedWriter = Box<dyn ImageWriter>;

/// A cell a job and the frame loop both hold.
///
/// It is `Arc<Mutex<_>>` natively, because a job runs on a thread of its own. The
/// frame loop reads what the job writes across a real race. It is `Rc<RefCell<_>>`
/// in a tab, because a tab has one thread. There, an atomic refcount and a lock
/// would both pay to prevent a race that cannot happen. The borrow rules that
/// `RefCell` enforces are all that is needed.
#[cfg(not(target_arch = "wasm32"))]
pub type Shared<T> = std::sync::Arc<std::sync::Mutex<T>>;

/// A cell a job and the frame loop both hold.
#[cfg(target_arch = "wasm32")]
pub type Shared<T> = std::rc::Rc<std::cell::RefCell<T>>;

/// Put a value somewhere a job and the frame loop can both reach it.
#[cfg(not(target_arch = "wasm32"))]
pub fn share<T>(value: T) -> Shared<T> {
    std::sync::Arc::new(std::sync::Mutex::new(value))
}

/// Put a value somewhere a job and the frame loop can both reach it.
#[cfg(target_arch = "wasm32")]
pub fn share<T>(value: T) -> Shared<T> {
    std::rc::Rc::new(std::cell::RefCell::new(value))
}

/// Take a shared cell, whatever happened to the last holder of it.
///
/// A poisoned lock is taken anyway. Otherwise a job that panicked while holding
/// the lock would wedge the frame loop, and the window would freeze on the
/// poisoned mutex. Reading whatever the job wrote before it died is the better
/// outcome. What the lock guards is a progress event or a slot for the agents to
/// come back into. Neither holds an invariant across the lock that a panic can
/// break.
#[cfg(not(target_arch = "wasm32"))]
pub fn lock<T>(shared: &Shared<T>) -> std::sync::MutexGuard<'_, T> {
    shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Take a shared cell.
///
/// The borrows are all short and none of them nest, which is all a `RefCell`
/// requires.
#[cfg(target_arch = "wasm32")]
pub fn lock<T>(shared: &Shared<T>) -> std::cell::RefMut<'_, T> {
    shared.borrow_mut()
}

/// Run a job.
///
/// Each job is one task, and there is no worker. The parameters move in as the
/// task is made. They are the only thing a long-lived worker behind a command
/// channel would have carried.
///
/// It takes a closure that *makes* a future, rather than a future. The future is
/// then built on the thread that drives it, and never crosses a thread. The image
/// seam's boxed futures are deliberately not `Send`. A job's captures cross a
/// thread, and its awaits do not.
///
/// Natively the thread blocks, which suits the native USB transport. That
/// transport blocks the thread it is driven on, and `transport/usb.rs` explains
/// why. Under `pollster`, on a thread of the job's own, the blocking costs nothing.
#[cfg(not(target_arch = "wasm32"))]
pub fn spawn<Make, Fut>(work: Make)
where
    Make: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()>,
{
    std::thread::spawn(move || pollster::block_on(work()));
}

/// Run a job, on the browser's own event loop.
///
/// A tab has no threads, and needs none. The shape matches the native build's, for
/// the same reason: the future is built where it is driven.
#[cfg(target_arch = "wasm32")]
pub fn spawn<Make, Fut>(work: Make)
where
    Make: FnOnce() -> Fut + 'static,
    Fut: Future<Output = ()> + 'static,
{
    wasm_bindgen_futures::spawn_local(async move { work().await });
}

/// A task's obligation to answer into its slot, which the drop meets on a panic.
///
/// Every off-frame task ends by putting its answer into a slot. The frame loop
/// reads an empty slot as *still running*, so the button stays disabled and the
/// spinner stays on. A task that panics never reaches its write, and without this
/// value the spinner would run forever. The obligation is a value the task owns.
/// On the normal path the task delivers it. If the task dies first, the unwind
/// drops it, and the drop answers with the fault.
///
/// Natively, a thread's panic unwinds this way. On the web a panic halts the
/// whole instance, so nothing is left for a drop to rescue.
struct Answer<T> {
    /// What the task was doing, for the fault report: `"opening the board"`.
    what: &'static str,
    /// The slot the frame loop is watching.
    slot: Shared<Option<Result<T>>>,
    /// How to make the frame loop look.
    wake: Wake,
    /// Whether the answer has been delivered, so the drop knows whether the
    /// task died owing one.
    delivered: bool,
}

impl<T> Answer<T> {
    /// Deliver the answer and wake the frame loop. This consumes the obligation,
    /// so the drop that follows has nothing left to do.
    fn deliver(mut self, answer: Result<T>) {
        self.delivered = true;
        *lock(&self.slot) = Some(answer);
        (self.wake)();
    }
}

impl<T> Drop for Answer<T> {
    fn drop(&mut self) {
        if self.delivered {
            return;
        }
        *lock(&self.slot) = Some(Err(Error::Internal(format!(
            "the task {} stopped before it answered",
            self.what
        ))));
        (self.wake)();
    }
}

/// Run a task that answers into a slot, and answer for a task that dies.
///
/// This is the slot-filling counterpart of [`spawn`], and the only way the app
/// fills a slot. A task spawned bare can panic between starting and writing its
/// slot. The frame loop would then read the empty slot as *still running* forever.
///
/// `what` names the work for the fault report. `make` is the work itself.
/// Whatever it returns goes into the slot as the answer, including the `Ok(None)`
/// of a dialog somebody closed.
#[cfg(not(target_arch = "wasm32"))]
pub fn spawn_answering<T, Make, Fut>(
    what: &'static str,
    slot: &Shared<Option<Result<T>>>,
    wake: Wake,
    make: Make,
) where
    T: Send + 'static,
    Make: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = Result<T>>,
{
    let answer = Answer {
        what,
        slot: slot.clone(),
        wake,
        delivered: false,
    };
    spawn(move || async move { answer.deliver(make().await) });
}

/// Run a task that answers into a slot, and answer for a task that dies.
///
/// It has the native build's shape, and differs only in the bounds that [`spawn`]
/// differs in.
#[cfg(target_arch = "wasm32")]
pub fn spawn_answering<T, Make, Fut>(
    what: &'static str,
    slot: &Shared<Option<Result<T>>>,
    wake: Wake,
    make: Make,
) where
    T: 'static,
    Make: FnOnce() -> Fut + 'static,
    Fut: Future<Output = Result<T>> + 'static,
{
    let answer = Answer {
        what,
        slot: slot.clone(),
        wake,
        delivered: false,
    };
    spawn(move || async move { answer.deliver(make().await) });
}

/// A small file somebody picked, read whole.
///
/// The recovery agent, the SPL and the U-Boot payload are each a bootloader, tens
/// of KB to a few MB in size. An XMODEM transfer sends a whole file, so unlike a
/// [`PickedImage`] there is nothing here to stream. The bytes are read into memory
/// at pick time, as the CLI's `recover` reads them. It holds no platform type, so
/// it is one struct in both builds rather than one per seam.
pub struct PickedBlob {
    /// What to call it on screen.
    pub name: String,
    /// The bytes it holds.
    pub bytes: Vec<u8>,
}

#[cfg(not(target_arch = "wasm32"))]
mod native {
    //! Acquiring a device, and picking a file, on a desktop.

    use super::{BoxedReader, BoxedWriter, Handle, PickedBlob, PortHandle};
    use pyrographer_core::agent::{DfuAgent, FlashAgent, RockusbAgent};
    use pyrographer_core::codec::rkboot::{self, LoaderImage};
    use pyrographer_core::discovery::{DeviceInfo, Mode, Vendor};
    use pyrographer_core::image::{SyncReader, SyncWriter};
    use pyrographer_core::transport::{SerialTransport, UsbTransport};
    use pyrographer_core::{Error, Result, verbs};
    use std::fs::File;
    use std::io::{BufReader, BufWriter};
    use std::path::{Path, PathBuf};

    /// Scan the bus.
    ///
    /// This is the first seam. A browser cannot scan a bus. It is *given* a device
    /// by a chooser that only a user gesture can open. Discovery therefore differs
    /// in the seam itself, not only in the implementation. The shared part is the
    /// pair of mode rules, `discovery::classify_rockchip` and
    /// `discovery::classify_ingenic`, which both builds use to decide what a device
    /// in hand is.
    pub fn list() -> Result<Vec<DeviceInfo>> {
        verbs::list()
    }

    /// What it takes to open the board a listing names.
    ///
    /// Natively a listing is already the handle: a place on the bus is both what a
    /// row shows and what reopening the board needs. This function is therefore a
    /// clone. It exists because in a tab the listing and the handle differ, and it
    /// lets one piece of row-drawing code serve both builds.
    pub fn handle_of(device: &DeviceInfo) -> Handle {
        device.clone()
    }

    /// Whether two handles name the same board.
    ///
    /// It compares the bus and the address, which tell two identical boards apart.
    /// The product ID cannot, because identical boards share one. A maskrom board
    /// reports a generic serial or none at all.
    pub fn same_board(one: &Handle, other: &Handle) -> bool {
        one.bus_id == other.bus_id && one.device_address == other.device_address
    }

    /// Open a device for the flash path, by the vendor whose backend it speaks.
    ///
    /// A Rockchip board is opened and probed for a loader, because the bcdUSB mode
    /// flag is only a claim. The RK3576 SPL loader runs rockusb behind the even
    /// flag of the maskrom it replaced. The BootROM presents the same vendor
    /// interface and bulk pair, with nothing serving the endpoints. A
    /// maskrom-flagged board is therefore opened and sent `TEST_UNIT_READY`. Only
    /// mass storage, which the interface class names reliably, is refused without
    /// an open.
    ///
    /// An Ingenic board's flash is reachable over DFU once a DFU-capable U-Boot runs
    /// on it. Only a board already in DFU mode is opened here. The open is
    /// [`UsbTransport::open_dfu`], which reads the board's alt-settings (its
    /// partitions) from its descriptors. The boot-ROM bootstrap flow brings a
    /// boot-ROM board up to DFU first. A mass-storage gadget belongs to the
    /// operating system's block layer.
    pub async fn open(device: &Handle) -> Result<FlashAgent<UsbTransport>> {
        match device.vendor {
            Vendor::Rockchip => {
                if device.mode == Mode::MassStorage {
                    return Err(Error::WrongMode {
                        found: device.mode.name(),
                        needed: "loader",
                    });
                }
                let transport = UsbTransport::open(device).await?;
                let mut agent = RockusbAgent::new(transport);
                if device.mode == Mode::Maskrom && agent.test_unit_ready().await.is_err() {
                    return Err(Error::WrongMode {
                        found: "maskrom",
                        needed: "loader",
                    });
                }
                Ok(FlashAgent::Rockusb(agent))
            }
            Vendor::Ingenic => match device.mode {
                Mode::Dfu => {
                    let opened = UsbTransport::open_dfu(device).await?;
                    Ok(FlashAgent::Dfu(DfuAgent::new(
                        opened.transport,
                        opened.interface,
                        opened.functional,
                        opened.alts,
                    )))
                }
                Mode::BootRom => Err(Error::InvalidRequest(
                    "this Ingenic board is in its USB boot ROM, which has no reachable flash. \
                     Bring it up to DFU first with the boot-ROM bootstrap, then read its partitions"
                        .to_string(),
                )),
                other => Err(Error::WrongMode {
                    found: other.name(),
                    needed: "DFU",
                }),
            },
        }
    }

    /// Open a maskrom board for the download-boot.
    ///
    /// It returns a bare transport, not a [`FlashAgent`], because a maskrom board
    /// has no reachable flash. The bootstrap uploads a loader over the transport.
    /// The board re-enumerates once the loader runs, so the transport is used and
    /// dropped rather than kept.
    pub async fn open_maskrom(device: &Handle) -> Result<UsbTransport> {
        UsbTransport::open_maskrom(device).await
    }

    /// Open an Ingenic boot-ROM board for the `VR_*` upload.
    ///
    /// This is the Ingenic counterpart of [`open_maskrom`], with one difference.
    /// The boot ROM speaks over a bulk pair as well as endpoint 0, and the `VR_*`
    /// payload travels on the bulk pipe. The board is therefore opened with the
    /// ordinary [`UsbTransport::open`], which claims that pair, rather than the
    /// control-only maskrom open.
    ///
    /// It also returns a bare transport, not a [`FlashAgent`]. A boot-ROM board has
    /// no reachable flash until a DFU-capable U-Boot is running. The bootstrap
    /// uploads its two stages over the transport. The board then re-enumerates as
    /// a DFU gadget, and the transport is dropped. **\[UNVERIFIED\]** until an
    /// Ingenic board drives it.
    pub async fn open_bootrom(device: &Handle) -> Result<UsbTransport> {
        UsbTransport::open(device).await
    }

    /// Open a serial port at a named rate.
    ///
    /// This is the serial acquisition seam, and the one way the window opens a
    /// serial port. A serial port is a path a person names (`/dev/ttyUSB0` or
    /// `COM3`), so there is no bus to scan and nothing to classify. This is the
    /// serial counterpart of [`open`], with no discovery before it.
    ///
    /// The line is 8N1, with the read and write deadlines that bound every
    /// recovery and console transfer. The JH7110 ROM fixes its recovery UART at
    /// 115200, and a console form starts at that default. The rate is a parameter
    /// rather than a constant, because a board's *console* runs at whatever rate
    /// its build sets.
    ///
    /// It is `async` for the browser build's sake, not this one's. Natively it is a
    /// syscall that returns, claiming no interface and waiting for no kernel driver
    /// to let go. The future is ready on its first poll, and its task ends at
    /// once. In a tab the open is a promise. Both builds present the same
    /// signature, so the three flows that open a port reach it through one path
    /// instead of forking. That one path is worth a thread that finishes at once.
    pub async fn open_serial_at(port: PortHandle, baud: u32) -> Result<SerialTransport> {
        SerialTransport::open_at(&port, baud)
    }

    /// What to call a port on screen: natively, the path itself.
    ///
    /// The path a person typed already names the place, so this is the identity
    /// function. It exists because the web half has real work to do here, and both
    /// builds answer the same question.
    pub fn port_name(port: &PortHandle) -> String {
        port.clone()
    }

    /// A loader somebody picked, parsed and ready to upload.
    ///
    /// It is parsed at pick time. A file that is not a loader is therefore caught
    /// where a person picked it, rather than after a maskrom device has been opened.
    pub struct PickedLoader {
        /// What to call it on screen.
        pub name: String,
        /// The container, decoded into the sections a bootstrap uploads.
        pub loader: LoaderImage,
    }

    /// Ask for a loader blob, and parse it.
    ///
    /// `Ok(None)` is a dialog somebody closed. A file that is not an RKBOOT loader
    /// returns an [`Error`] like any other, so the button comes back and the reason
    /// is shown.
    pub async fn ask_for_loader() -> Result<Option<PickedLoader>> {
        let Some(handle) = rfd::AsyncFileDialog::new().pick_file().await else {
            return Ok(None);
        };
        let bytes = handle.read().await;
        let loader = rkboot::parse(&bytes)?;
        Ok(Some(PickedLoader {
            name: handle.file_name(),
            loader,
        }))
    }

    /// Ask for a file and read it whole.
    ///
    /// The recovery files (the agent, the SPL and the U-Boot payload) are each
    /// small enough to read into memory. This is therefore [`ask_for_loader`]
    /// without the parse: it picks a file, reads its bytes and names it. `Ok(None)`
    /// is a dialog somebody closed, and the button comes back.
    pub async fn ask_for_blob() -> Result<Option<PickedBlob>> {
        let Some(handle) = rfd::AsyncFileDialog::new().pick_file().await else {
            return Ok(None);
        };
        Ok(Some(PickedBlob {
            name: handle.file_name(),
            bytes: handle.read().await,
        }))
    }

    /// An image somebody picked.
    ///
    /// This is the second seam. It holds the file rather than a reader, so it can
    /// be read more than once. A plan, the write it authorizes and a verify
    /// afterwards are three passes over one file, which somebody picks once. The
    /// web build holds a `Blob` here and slices it. The shape is the same, and the
    /// verbs that consume it cannot tell the two apart.
    pub struct PickedImage {
        /// What to call it on screen.
        pub name: String,
        /// How many bytes it holds, measured at pick time.
        ///
        /// A plan is made against this number, and the write that plan authorizes
        /// streams exactly this many bytes. A file that shrank after it was picked
        /// comes up short, and the write stops with an error where the file ends.
        /// A file that grew, or changed in place at the same size, raises no error.
        /// The write streams the file's first `bytes` bytes as they stand then.
        pub bytes: u64,
        /// Where it is.
        pub path: PathBuf,
    }

    impl PickedImage {
        /// A reader over the image, from its first byte.
        pub fn reader(&self) -> Result<BoxedReader> {
            let file = File::open(&self.path)
                .map_err(|e| Error::Io(format!("cannot open {}: {e}", self.path.display())))?;
            Ok(Box::new(SyncReader::new(BufReader::new(file))))
        }
    }

    /// Somewhere somebody picked to put a dump, opened and ready.
    pub struct PickedSink {
        /// What to call it on screen.
        pub name: String,
        /// Where the bytes go.
        pub writer: BoxedWriter,
    }

    /// Ask for an image.
    ///
    /// The picker runs on the same job-spawn machinery as the verbs, which is what
    /// `AsyncFileDialog` is for. The blocking `FileDialog` would stall the frame
    /// loop for as long as somebody took to choose a file.
    ///
    /// `Ok(None)` is a dialog somebody closed. It is a finding rather than a
    /// failure, and the button comes back.
    pub async fn ask_for_image() -> Result<Option<PickedImage>> {
        let Some(handle) = rfd::AsyncFileDialog::new().pick_file().await else {
            return Ok(None);
        };
        let path = handle.path().to_path_buf();

        let file = File::open(&path)
            .map_err(|e| Error::Io(format!("cannot open {}: {e}", path.display())))?;
        let bytes = file
            .metadata()
            .map_err(|e| Error::Io(format!("cannot measure {}: {e}", path.display())))?
            .len();

        Ok(Some(PickedImage {
            name: name_of(&path),
            bytes,
            path,
        }))
    }

    /// Ask where to put a dump, and open it.
    ///
    /// The file is created here rather than at pick time, and a dialog somebody
    /// closed creates nothing at all.
    pub async fn ask_for_sink(suggested: String) -> Result<Option<PickedSink>> {
        let Some(handle) = rfd::AsyncFileDialog::new()
            .set_file_name(suggested)
            .save_file()
            .await
        else {
            return Ok(None);
        };
        let path = handle.path().to_path_buf();

        let file = File::create(&path)
            .map_err(|e| Error::Io(format!("cannot create {}: {e}", path.display())))?;

        Ok(Some(PickedSink {
            name: name_of(&path),
            writer: Box::new(SyncWriter::new(BufWriter::new(file))),
        }))
    }

    /// What to call a file on screen: its name, or its whole path for a path with
    /// no name.
    fn name_of(path: &Path) -> String {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string())
    }
}

/// The block layer, where there is one.
///
/// This is the third acquisition seam, and the one that is not about a board. A
/// USB device is found by scanning a bus. A serial port is a path a person names.
/// A disk is enumerated by the operating system that is already using it.
///
/// The Block backend is Linux-only, and this is the one place the GUI says so.
/// Every build draws the same section, and a build without the backend refuses
/// here with a sentence. Hiding the section behind a `cfg` would leave somebody
/// wondering where their card reader went.
///
/// The list needs no privilege, and the open does. That is how Linux behaves, and
/// it is the reverse of the Windows behavior the backend's requirements were
/// derived against. On Windows, reading capacity requires administrator rights,
/// which puts the whole plan behind elevation. On Linux, nothing is opened to read
/// it. The rows therefore draw for anybody, and the refusal comes at Open, in
/// core's own words.
pub mod blocks {
    use pyrographer_core::Result;
    use pyrographer_core::agent::FlashAgent;
    use pyrographer_core::block::BlockDevice;

    use super::Wire;

    /// Every whole block device the kernel knows about.
    ///
    /// It needs no privilege and opens nothing. Each of these comes from `/sys` and
    /// `/proc`:
    ///
    /// - Capacity
    /// - Both sector sizes
    /// - The bus
    /// - The mount points
    /// - The running-system refusal
    ///
    /// The rows therefore draw for anybody, and elevation is needed exactly one
    /// step later, at the open.
    #[cfg(all(target_os = "linux", not(target_arch = "wasm32")))]
    pub fn list() -> Result<Vec<BlockDevice>> {
        pyrographer_core::block::list()
    }

    /// Open a disk for exclusive, uncached use.
    ///
    /// It is synchronous, because opening a file is a syscall that returns. The
    /// board seam is async because a USB open is.
    ///
    /// **The returned agent holds the device for as long as the agent is held.**
    /// `O_EXCL` is the kernel's promise that nothing else has the disk. A window
    /// keeps it from the moment somebody opens a disk until they close it. The
    /// CLI keeps it for the length of one command.
    ///
    /// The exclusive hold makes the plan a plan *for this device*. A replug that
    /// renames `sdb` invalidates the handle rather than redirecting it. No identity
    /// has to be stamped into the plan to be re-checked later.
    ///
    /// A window keeps rows on screen while somebody reads them, so a listing can
    /// go stale before the exclusive open. In that gap a card can be pulled and
    /// another inserted under the same name. `block::open` lists again and refuses
    /// a device that is not the one described. It does so for every caller, so the
    /// same code closes the CLI's millisecond-wide gap and this one.
    ///
    /// It opens for the widest access the disk allows, decided from the
    /// description read at the moment of the open. A window opens a disk once and
    /// then offers every verb on it. A disk that will not take a write, such as a
    /// card with its lock switch on, is therefore opened for reading rather than
    /// refused. The write is grayed out with the sentence that says why.
    #[cfg(all(target_os = "linux", not(target_arch = "wasm32")))]
    pub fn open(device: &BlockDevice) -> Result<FlashAgent<Wire>> {
        // `FlashAgent::Block` carries no transport, so a disk opens into the same
        // agent type a board does and every verb below is written once for both.
        Ok(FlashAgent::Block(pyrographer_core::block::open(device)?))
    }

    /// The refusal both halves give where there is no backend.
    ///
    /// An empty list would say *this machine has no disks*, which is false. It
    /// would send somebody looking for the disk they can see, instead of telling
    /// them the backend is not built here.
    #[cfg(not(all(target_os = "linux", not(target_arch = "wasm32"))))]
    fn unbuilt<T>() -> Result<T> {
        Err(pyrographer_core::Error::NotImplemented(
            "the Block backend is built for Linux only. On this platform, it is not measured \
             whether an exclusive open is enforced, or whether a read-back comes from the device \
             or from a cache. pyrographer does not offer a write without those guarantees",
        ))
    }

    /// What a listing answers where there is no backend: [`unbuilt`], which the
    /// section draws as a sentence.
    #[cfg(not(all(target_os = "linux", not(target_arch = "wasm32"))))]
    pub fn list() -> Result<Vec<BlockDevice>> {
        unbuilt()
    }

    /// What an open answers where there is no backend. It is unreachable in
    /// practice, because a listing that refused offers no row to pick. It is
    /// written anyway, so the two halves cannot disagree about whether this
    /// platform has a Block backend.
    #[cfg(not(all(target_os = "linux", not(target_arch = "wasm32"))))]
    pub fn open(_device: &BlockDevice) -> Result<FlashAgent<Wire>> {
        unbuilt()
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::{
    PickedImage, PickedLoader, PickedSink, ask_for_blob, ask_for_image, ask_for_loader,
    ask_for_sink, handle_of, list, open, open_bootrom, open_maskrom, open_serial_at, port_name,
    same_board,
};

#[cfg(target_arch = "wasm32")]
mod web {
    //! Acquiring a device, and picking a file, in a browser tab.
    //!
    //! **\[UNVERIFIED\]** against a real browser.

    use super::{BoxedReader, BoxedWriter, Handle, PickedBlob, PortHandle};
    use js_sys::Uint8Array;
    use pyrographer_core::agent::{DfuAgent, FlashAgent, RockusbAgent};
    use pyrographer_core::codec::rkboot::{self, LoaderImage};
    use pyrographer_core::discovery::{self, DeviceInfo, Mode, USB_VENDOR_IDS, Vendor};
    use pyrographer_core::image::{BoxFuture, ImageReader, ImageWriter};
    use pyrographer_core::transport::{self, WebSerialTransport, WebUsbTransport};
    use pyrographer_core::{Error, Result};
    use wasm_bindgen::{JsCast, JsValue};
    use wasm_bindgen_futures::JsFuture;
    use web_sys::{
        Blob, FileSystemWritableFileStream, SaveFilePickerOptions, WritableStreamDefaultWriter,
    };

    /// Ask the person to pick a board.
    ///
    /// This is the first seam. A page cannot enumerate a bus, so there is no list
    /// here. `requestDevice` opens a chooser that the page did not draw and cannot
    /// script. The page cannot pre-select a row in it, and cannot open it without
    /// a real user gesture. The write confirmation depends on that property.
    ///
    /// What comes back is classified by the same rules the native scan uses,
    /// `discovery::classify_rockchip` and `discovery::classify_ingenic`. Those
    /// rules are the part of discovery the two builds share.
    ///
    /// One chooser offers every vendor. Natively, discovery makes a VID-filtered
    /// pass per vendor, and the pass that found a device gives its vendor. Here
    /// each chooser needs a user gesture. A chooser per vendor would ask a person
    /// which vendor their board is before they could pick it. The one chooser
    /// carries every filter, and `describe_webusb` reads the vendor from the device
    /// that comes back.
    pub async fn ask_for_board() -> Result<Option<(Handle, DeviceInfo)>> {
        let Some(device) = transport::request_device(&USB_VENDOR_IDS).await? else {
            return Ok(None);
        };
        let info = discovery::describe_webusb(&device)?;
        Ok(Some((device, info)))
    }

    /// List the boards this origin has already been granted.
    ///
    /// This is neither the native scan nor a second way through the chooser. A
    /// page still cannot enumerate a bus. `getDevices` returns the set of boards
    /// somebody has already granted this origin through [`ask_for_board`]. The
    /// list therefore *remembers* rather than discovers. A board never picked here
    /// is absent from it, even while it is plugged in.
    ///
    /// The grant outlives the tab. A board picked once comes back on the next page
    /// load as a row to click, instead of a chooser to open again. It needs no
    /// user gesture, so it can run at startup, where `requestDevice` cannot.
    ///
    /// **It is not a way to confirm a write.** The write gate re-picks through the
    /// chooser, because that takes a gesture the page cannot manufacture. A row in
    /// a list the page drew itself is only a click, and a gate that accepted it
    /// would confirm nothing. See `App::repick`.
    pub async fn list_permitted() -> Result<Vec<DeviceInfo>> {
        transport::list_permitted(&USB_VENDOR_IDS)
            .await?
            .iter()
            .map(discovery::describe_webusb)
            .collect()
    }

    /// What it takes to open the board a listing names.
    ///
    /// In a tab the listing and the handle differ. A row shows what a board *is*,
    /// and opening it needs the device object the browser handed over.
    /// `DeviceInfo` carries that object on the web for this reason. This function
    /// takes it back out, which lets one piece of row-drawing code serve both
    /// builds.
    pub fn handle_of(device: &DeviceInfo) -> Handle {
        device.device.clone()
    }

    /// Open a device for the verbs, by what its vendor's backend needs.
    ///
    /// It makes the same branch the native `open` makes, over WebUSB rather than
    /// nusb. A `FlashAgent` is generic over its transport, so the code that
    /// consumes the agent is the same in both builds.
    ///
    /// For Rockchip it probes rather than reading the flag, because the bcdUSB
    /// flag is only a claim. The RK3576 BootROM presents the same interface and
    /// bulk pair a loader does, and one `TEST_UNIT_READY` tells them apart. Only
    /// mass storage is refused outright.
    ///
    /// For Ingenic there is nothing to probe, because the mode is in the product
    /// ID. A DFU gadget is opened with `open_dfu`, which reads its alt-settings
    /// (its partitions) from its descriptors. A boot ROM has no reachable flash
    /// until the bootstrap has run. It is therefore refused here and pointed at
    /// [`open_bootrom`].
    pub async fn open(device: &Handle) -> Result<FlashAgent<WebUsbTransport>> {
        let info = discovery::describe_webusb(device)?;
        match info.vendor {
            Vendor::Rockchip => {
                if info.mode == Mode::MassStorage {
                    return Err(Error::WrongMode {
                        found: info.mode.name(),
                        needed: "loader",
                    });
                }
                let transport = WebUsbTransport::open(device.clone()).await?;
                let mut agent = RockusbAgent::new(transport);
                if info.mode == Mode::Maskrom && agent.test_unit_ready().await.is_err() {
                    return Err(Error::WrongMode {
                        found: "maskrom",
                        needed: "loader",
                    });
                }
                Ok(FlashAgent::Rockusb(agent))
            }
            Vendor::Ingenic => match info.mode {
                Mode::Dfu => {
                    let opened = WebUsbTransport::open_dfu(device.clone()).await?;
                    Ok(FlashAgent::Dfu(DfuAgent::new(
                        opened.transport,
                        opened.interface,
                        opened.functional,
                        opened.alts,
                    )))
                }
                Mode::BootRom => Err(Error::InvalidRequest(
                    "this Ingenic board is in its USB boot ROM, which has no reachable flash. \
                     Bring it up to DFU first with the boot-ROM bootstrap, then read its partitions"
                        .to_string(),
                )),
                other => Err(Error::WrongMode {
                    found: other.name(),
                    needed: "DFU",
                }),
            },
        }
    }

    /// Open a maskrom board for the download-boot.
    ///
    /// The browser's counterpart to the native `open_maskrom`. It is
    /// **\[UNVERIFIED\]** on two counts. Neither the maskrom re-enumeration nor the
    /// browser's willingness to pass endpoint-0 control transfers on a maskrom
    /// device has been run.
    pub async fn open_maskrom(device: &Handle) -> Result<WebUsbTransport> {
        WebUsbTransport::open_maskrom(device.clone()).await
    }

    /// Open an Ingenic boot-ROM board for the `VR_*` upload.
    ///
    /// The browser's counterpart to the native `open_bootrom`. It differs from
    /// [`open_maskrom`] as the native one does. The Ingenic boot ROM carries its
    /// payload on a bulk pipe as well as endpoint 0. The board is therefore opened
    /// with the ordinary `WebUsbTransport::open`, which claims the pair, rather
    /// than the control-only maskrom open.
    ///
    /// It returns a bare transport, not a `FlashAgent`, because a boot-ROM board
    /// has no reachable flash until a DFU-capable U-Boot is running. The bootstrap
    /// uploads its two stages over the transport, which is dropped as the board
    /// re-enumerates.
    ///
    /// What follows differs in a tab, and the flow says so. Natively the DFU gadget
    /// appears in the next scan of the bus. A browser grants permission per device,
    /// and the gadget that comes back is a *different* device: a different product
    /// ID from the same vendor. The grant the boot ROM was picked under does not
    /// cover it, so nothing appears on its own. The board is picked again from the
    /// chooser, with one gesture of the same kind that granted the boot ROM.
    ///
    /// **\[UNVERIFIED\]** on two counts: no Ingenic board has driven this flow in
    /// either build, and no browser has run this one.
    pub async fn open_bootrom(device: &Handle) -> Result<WebUsbTransport> {
        WebUsbTransport::open(device.clone()).await
    }

    /// A loader somebody picked, parsed and ready to upload.
    pub struct PickedLoader {
        /// What to call it on screen.
        pub name: String,
        /// The container, decoded into the sections a bootstrap uploads.
        pub loader: LoaderImage,
    }

    /// Ask for a loader blob, and parse it.
    ///
    /// The whole file is read into memory and parsed. A loader is under a
    /// megabyte, so unlike an image there is nothing to stream. A file that is not
    /// an RKBOOT loader returns an [`Error`], and the button comes back.
    pub async fn ask_for_loader() -> Result<Option<PickedLoader>> {
        let Some(handle) = rfd::AsyncFileDialog::new().pick_file().await else {
            return Ok(None);
        };
        let bytes = handle.read().await;
        let loader = rkboot::parse(&bytes)?;
        Ok(Some(PickedLoader {
            name: handle.file_name(),
            loader,
        }))
    }

    /// Ask for a file and read it whole.
    ///
    /// This is the file picker for the serial flows, and the web half of
    /// [`ask_for_blob`]. Every file it reads is small (a recovery agent, an SPL, a
    /// U-Boot payload or an Ingenic stage). Each is therefore read into memory like
    /// the loader rather than streamed.
    ///
    /// [`ask_for_blob`]: super::ask_for_blob
    pub async fn ask_for_blob() -> Result<Option<PickedBlob>> {
        let Some(handle) = rfd::AsyncFileDialog::new().pick_file().await else {
            return Ok(None);
        };
        Ok(Some(PickedBlob {
            name: handle.file_name(),
            bytes: handle.read().await,
        }))
    }

    /// Ask the person to pick a serial port, and hand back what they picked.
    ///
    /// This is the serial acquisition seam, and the counterpart of
    /// [`ask_for_board`]. There is no list here, for the same reason there is none
    /// for a board. The browser does not enumerate serial ports for a page, and
    /// `requestPort` is the only way in. It opens a chooser that the page did not
    /// draw, cannot script, and cannot open without a real user gesture.
    ///
    /// The chooser is deliberately **unfiltered**, unlike the board chooser. A
    /// Rockchip board on the bus *is* a Rockchip device, so filtering on its vendor
    /// ID hides nothing somebody wants. A serial line has no such identity. What is
    /// plugged into the host is a USB-serial adapter. Its vendor ID names FTDI, WCH
    /// or Silicon Labs, and never the board at the far end. A filter here could
    /// only hide the port somebody meant to pick.
    ///
    /// `Ok(None)` is a chooser somebody closed without picking. It is a person's
    /// decision, not a failure.
    pub async fn ask_for_port() -> Result<Option<PortHandle>> {
        transport::request_port().await
    }

    /// Open a port the browser handed over, at `baud`.
    ///
    /// This is the web half of the seam. It is `async` because the open is a
    /// promise, as is every read and write on the line afterwards. The native half
    /// has the same signature, so the flows that open a port reach it through one
    /// path.
    pub async fn open_serial_at(port: PortHandle, baud: u32) -> Result<WebSerialTransport> {
        WebSerialTransport::open_at(port, baud).await
    }

    /// What to call a port on screen, where there is no path to show.
    ///
    /// This is the one thing the web build has to invent. Natively a port's name
    /// is its path, and a plan and a confirmation both quote it. Here a port is an
    /// object. The only thing it reports about itself is `getInfo()`: the USB
    /// vendor and product IDs of the *adapter*, where it has them.
    ///
    /// Those IDs are what is shown, labeled as a USB-serial adapter (such as
    /// `0403:6001`) rather than presented as a board. The docs for [`ask_for_port`]
    /// say why those IDs cannot name the board. The adapter is FTDI's or WCH's,
    /// and the board is at the far end of it. An adapter that reports nothing (a
    /// built-in port, or one the browser will not describe) is "the serial port
    /// you chose". That name is true, and it is all that can be said.
    ///
    /// **\[UNVERIFIED\]**: the specification says `getInfo` answers before the port
    /// is opened, and nothing here has observed it.
    pub fn port_name(port: &PortHandle) -> String {
        let info = port.get_info();
        match (info.get_usb_vendor_id(), info.get_usb_product_id()) {
            (Some(vid), Some(pid)) => format!("USB-serial adapter {vid:04x}:{pid:04x}"),
            _ => "the serial port you chose".to_string(),
        }
    }

    /// Whether two picks are the same board.
    ///
    /// **The web write gate depends on this comparison**, and the comparison is
    /// **\[UNVERIFIED\]**. A person confirms a write by picking the destination
    /// again from the chooser. The pick is a confirmation, rather than a click,
    /// because of one property: the board picked can be told apart from the board
    /// not picked.
    ///
    /// Field values cannot tell them apart. WebUSB exposes `vendorId`,
    /// `productId`, `serialNumber` and `productName`. Two identical boards match on
    /// the first two, and typically report no serial at all. That is the clone-swap
    /// case the gate exists to catch. The comparison therefore uses object
    /// identity. Chrome is believed to return the *same* `USBDevice` instance for
    /// an already-permitted device, which makes `Object.is` a real comparison.
    ///
    /// That is not confirmed. Settling it needs two identical boards, one Chrome,
    /// and one `requestDevice()` call. If it does not hold, the web flasher's
    /// clone gate is weaker than the native one.
    pub fn same_board(one: &Handle, other: &Handle) -> bool {
        js_sys::Object::is(one.as_ref(), other.as_ref())
    }

    /// Whether two picks are the same serial port.
    ///
    /// **The web recovery gate depends on this comparison**, and it is
    /// **\[UNVERIFIED\]** for the same reason [`same_board`] is. A person confirms
    /// a recovery by picking the port again from the chooser. The pick is a
    /// confirmation, rather than a click, because of one property: the port picked
    /// can be told apart from the port not picked.
    ///
    /// Field values can tell ports apart even less than boards. `getInfo()` gives
    /// the *adapter's* USB vendor and product IDs and nothing else. Two identical
    /// FTDI cables, the ordinary case on a bench with two boards, match on
    /// everything there is to compare. The comparison therefore uses object
    /// identity, the same assumption `same_board` makes.
    pub fn same_port(one: &PortHandle, other: &PortHandle) -> bool {
        js_sys::Object::is(one.as_ref(), other.as_ref())
    }

    /// An image somebody picked: a `Blob`, read a window at a time.
    ///
    /// This is the second seam. It holds the file rather than a reader, so it can
    /// be read more than once. A `Blob` can be sliced, so the image streams, and
    /// the tab never holds a whole-eMMC image.
    pub struct PickedImage {
        /// What to call it on screen.
        pub name: String,
        /// How many bytes it holds.
        pub bytes: u64,
        /// The file itself.
        pub blob: Blob,
    }

    impl PickedImage {
        /// A reader over the image, from its first byte.
        pub fn reader(&self) -> Result<BoxedReader> {
            Ok(Box::new(BlobReader {
                blob: self.blob.clone(),
                at: 0.0,
            }))
        }
    }

    /// Reads a `Blob`, a window at a time.
    ///
    /// Core's image seam is async because of this reader. A `Blob` has no blocking
    /// read, and yields its bytes through a promise. A synchronous `std::io::Read`
    /// over it would have to buffer the entire image in memory first. That would
    /// break the invariant the windowed design exists to protect: the image is
    /// never held in memory.
    struct BlobReader {
        blob: Blob,
        /// The byte offset of the next read. It is an `f64`, because a `Blob`
        /// measures itself in one, and an eMMC holds more bytes than an `i32` can
        /// count.
        at: f64,
    }

    impl ImageReader for BlobReader {
        fn read_exact<'a>(&'a mut self, buf: &'a mut [u8]) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                let end = self.at + buf.len() as f64;

                let slice = self
                    .blob
                    .slice_with_f64_and_f64(self.at, end)
                    .map_err(|e| Error::Io(format!("cannot slice the image: {}", describe(&e))))?;

                let buffer = JsFuture::from(slice.array_buffer())
                    .await
                    .map_err(|e| Error::Io(format!("cannot read the image: {}", describe(&e))))?;

                let array = Uint8Array::new(&buffer);

                // A short slice is the file being shorter than it said it was.
                // The plan was made against its length, and the write that plan
                // authorized streams exactly that many bytes -- so this is the
                // image contradicting the plan, and it stops the write.
                if array.length() as usize != buf.len() {
                    return Err(Error::Io(format!(
                        "the image gave {} of the {} bytes that were asked for",
                        array.length(),
                        buf.len()
                    )));
                }

                array.copy_to(buf);
                self.at = end;
                Ok(())
            })
        }
    }

    /// Somewhere somebody picked to put a dump, opened and ready.
    pub struct PickedSink {
        /// What to call it on screen.
        pub name: String,
        /// Where the bytes go.
        pub writer: BoxedWriter,
    }

    /// Writes a dump out through the browser's own file stream.
    ///
    /// The counterpart of [`BlobReader`], and async for the same reason: the
    /// stream takes a window and returns a promise. A dump of a 58 GiB eMMC goes to
    /// disk a megabyte at a time, and is never in the tab's memory.
    struct StreamWriter {
        writer: WritableStreamDefaultWriter,
    }

    impl ImageWriter for StreamWriter {
        fn write_all<'a>(&'a mut self, buf: &'a [u8]) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                // Copied into the browser's heap, because the stream takes the
                // chunk and may hold it past this call.
                let chunk = Uint8Array::from(buf);

                JsFuture::from(self.writer.write_with_chunk(chunk.as_ref()))
                    .await
                    .map(|_| ())
                    .map_err(|e| Error::Io(format!("cannot write the dump: {}", describe(&e))))
            })
        }

        fn flush(&mut self) -> BoxFuture<'_, Result<()>> {
            // Closing is what commits it. A browser's writable stream holds bytes
            // until it is told not to, and a dump that returned before they landed
            // would report a file that is not there -- which is why `flush` is on
            // the trait at all rather than defaulted away.
            Box::pin(async move {
                JsFuture::from(self.writer.close())
                    .await
                    .map(|_| ())
                    .map_err(|e| Error::Io(format!("cannot finish the dump: {}", describe(&e))))
            })
        }
    }

    /// Ask for an image.
    ///
    /// `rfd`'s `file-handle-inner` feature lets both builds use one picking API.
    /// The picked handle carries a `web_sys::File`. A `File` is a `Blob`, so it can
    /// be sliced and streamed.
    pub async fn ask_for_image() -> Result<Option<PickedImage>> {
        let Some(handle) = rfd::AsyncFileDialog::new().pick_file().await else {
            return Ok(None);
        };
        let file = handle.inner();

        Ok(Some(PickedImage {
            name: handle.file_name(),
            bytes: file.size() as u64,
            blob: file.clone().into(),
        }))
    }

    /// Ask where to put a dump, and open the stream to it.
    ///
    /// This uses the File System Access API, because `rfd` has no save dialog on
    /// the web. That API gives a page a file it can *stream* to. A download would
    /// have to be assembled in memory first, and a whole-eMMC dump does not fit in
    /// a tab.
    pub async fn ask_for_sink(suggested: String) -> Result<Option<PickedSink>> {
        let window = web_sys::window()
            .ok_or_else(|| Error::Io("there is no window to ask on".to_string()))?;

        let options = SaveFilePickerOptions::new();
        options.set_suggested_name(Some(&suggested));

        let asked = window
            .show_save_file_picker_with_options(&options)
            .map_err(|e| Error::Io(format!("cannot ask where to save: {}", describe(&e))))?;

        let handle = match JsFuture::from(asked).await {
            Ok(handle) => handle,
            // A dialog somebody closed rejects, exactly as the device chooser
            // does, and it is a finding rather than a failure.
            Err(_) => return Ok(None),
        };

        let stream: FileSystemWritableFileStream = JsFuture::from(handle.create_writable())
            .await
            .map_err(|e| Error::Io(format!("cannot open {suggested}: {}", describe(&e))))?
            .unchecked_into();

        let writer = stream
            .get_writer()
            .map_err(|e| Error::Io(format!("cannot write to {suggested}: {}", describe(&e))))?;

        Ok(Some(PickedSink {
            name: suggested,
            writer: Box::new(StreamWriter { writer }),
        }))
    }

    /// What a JavaScript exception says, in as much detail as it will give.
    fn describe(error: &JsValue) -> String {
        error
            .dyn_ref::<js_sys::Error>()
            .map(|error| String::from(error.to_string()))
            .or_else(|| error.as_string())
            .unwrap_or_else(|| format!("{error:?}"))
    }
}

#[cfg(target_arch = "wasm32")]
pub use web::{
    PickedImage, PickedLoader, PickedSink, ask_for_blob, ask_for_board, ask_for_image,
    ask_for_loader, ask_for_port, ask_for_sink, handle_of, list_permitted, open, open_bootrom,
    open_maskrom, open_serial_at, port_name, same_board, same_port,
};

#[cfg(test)]
mod tests {
    //! Tests of the obligation to answer. The frame loop reads an empty slot as
    //! *still running*. These tests check that no way a task can end, by
    //! delivering or by dying, leaves the slot empty.

    use super::{Answer, Error, Result, Shared, lock, share};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// An answer obligation over `slot`, whose wake bumps `woke`.
    fn obligation<T>(slot: &Shared<Option<Result<T>>>, woke: &Arc<AtomicUsize>) -> Answer<T> {
        let woke = woke.clone();
        Answer {
            what: "testing",
            slot: slot.clone(),
            wake: Box::new(move || {
                woke.fetch_add(1, Ordering::SeqCst);
            }),
            delivered: false,
        }
    }

    /// The normal path: the answer lands, the frame loop is woken, and the
    /// drop that follows does not overwrite it with a fault.
    #[test]
    fn a_delivered_answer_is_the_answer() {
        let slot: Shared<Option<Result<u32>>> = share(None);
        let woke = Arc::new(AtomicUsize::new(0));

        obligation(&slot, &woke).deliver(Ok(7));

        assert!(matches!(lock(&slot).take(), Some(Ok(7))));
        assert_eq!(
            woke.load(Ordering::SeqCst),
            1,
            "woken once, by the delivery"
        );
    }

    /// An obligation dropped undelivered answers with the internal fault, naming
    /// the work. Unwinding out of a panicking task drops it this way.
    #[test]
    fn a_dropped_obligation_answers_with_the_fault() {
        let slot: Shared<Option<Result<u32>>> = share(None);
        let woke = Arc::new(AtomicUsize::new(0));

        drop(obligation(&slot, &woke));

        match lock(&slot).take() {
            Some(Err(Error::Internal(message))) => {
                assert!(message.contains("testing"), "the fault names the work");
            }
            other => panic!("expected the internal fault, got {other:?}"),
        }
        assert_eq!(woke.load(Ordering::SeqCst), 1, "woken once, by the fault");
    }

    /// End to end: a task that panics mid-work still answers. The unwind drops
    /// the future, the future's captures include the obligation, and the
    /// obligation's drop delivers the answer.
    #[test]
    fn a_panicking_task_still_answers() {
        let slot: Shared<Option<Result<u32>>> = share(None);
        let woke = Arc::new(AtomicUsize::new(0));
        let answer = obligation(&slot, &woke);

        let died = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _held = answer;
            panic!("this panic is the test's own: the task dying mid-work");
        }));

        assert!(died.is_err(), "the task died");
        assert!(
            matches!(lock(&slot).take(), Some(Err(Error::Internal(_)))),
            "and answered anyway"
        );
        assert_eq!(woke.load(Ordering::SeqCst), 1);
    }
}
