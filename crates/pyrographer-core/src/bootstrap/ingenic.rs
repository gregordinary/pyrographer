//! The bootstrap that brings an Ingenic XBurst board from its USB boot ROM to a
//! DFU-capable U-Boot.
//!
//! An Ingenic SoC in USB boot runs its boot ROM and nothing else. DRAM is
//! uninitialized, and the boot ROM has no flash primitive. This module uploads a
//! loader in two stages over the boot ROM's `VR_*` vendor protocol. A DRAM-init
//! stage goes into SRAM, and then a DFU-capable U-Boot goes into the DRAM that
//! stage brought up. The board then jumps into U-Boot, tears down its boot ROM USB
//! device, and re-enumerates as a DFU gadget, which speaks the protocol in
//! [`dfu`](crate::codec::dfu). This module is the Ingenic counterpart of the
//! Rockchip [`download_boot`](super::download_boot).
//!
//! The byte layouts are sans-I/O codecs in [`ingenic_boot`]. That module builds the
//! `VR_*` requests as transport-neutral SETUP packets, and parses the 8-byte
//! identifying magic. This module is the I/O half. It runs the control-and-bulk
//! sequence that carries a loader to the device. Progress and cancellation work as
//! they do in the flash verbs.
//!
//! # The sequence
//!
//! First, `VR_GET_CPU_INFO` identifies the SoC and returns its 8-byte magic. This
//! is the **only** point in the flow where the magic is available, because a
//! running U-Boot answers DFU, which has no such request. The magic has the role
//! the Rockchip `chipver` reply has: pinnable bytes that arm the write gate.
//!
//! Then each stage runs these steps:
//!
//! 1. `VR_SET_DATA_ADDRESS`, where the stage loads
//! 2. `VR_SET_DATA_LENGTH`, how long it is
//! 3. The payload, over the bulk OUT pipe
//! 4. `VR_PROGRAM_START1` for stage1 (run from SRAM), or `VR_PROGRAM_START2` for
//!    stage2 (run from DRAM)
//!
//! The stage2 jump tears the boot ROM's USB device down.
//!
//! # Hardware verification
//!
//! No Ingenic hardware has run this driver. Every hardware-facing choice here is
//! **\[UNVERIFIED\]** until a bench session pins it. A driver built only against a
//! scripted transport can still be wrong on hardware: the Rockchip download-boot
//! needed corrections on a real RK3576. The open choices are these:
//!
//! - The exact ordering: whether `VR_SET_DATA_LENGTH` is sent, and before or after
//!   the address
//! - The per-stage load and entry addresses, which are per-SoC and supplied by the
//!   caller rather than guessed here
//! - Whether the board stays on the same transport between stage1 and stage2, or
//!   re-enumerates
//! - The DRAM settle time after stage1, ~2 s **\[COMMUNITY\]**
//! - How the stage2 jump appears to the host
//!
//! This driver accepts the device leaving the bus ([`Error::Disconnected`]) as it
//! acknowledges `VR_PROGRAM_START2`, as a rockusb reset does. A real board can
//! signal the teardown differently.
//!
//! The tests pin the wire format against a scripted transport: the exact SETUP of
//! every `VR_*` transfer, and the exact bytes of every bulk chunk.

use crate::codec::ingenic_boot::{self, CpuInfo, Setup};
use crate::progress::{Cancel, Progress, ProgressSink};
use crate::transport::{Control, Transport};
use crate::{Error, Result};

/// The largest bulk window one write puts on the wire.
///
/// The boot ROM knows the total from `VR_SET_DATA_LENGTH`, so the payload can be
/// split across any number of bulk writes. They concatenate into one stream on the
/// wire. Chunking only advances progress and gives cancellation a boundary to stop
/// at, as the Rockchip download-boot's chunked control transfers do. The size
/// carries no protocol meaning.
const BULK_CHUNK: usize = 64 * 1024;

/// The load-and-entry address of stage1, the DRAM-init SPL, in the USB bootstrap.
///
/// It is uniform across the XBurst and XBurst2 family. Every SoC in thingino-dfu's
/// `ddr_config_database` shares this `spl_addr`: T10, T20/21, T30/31, T40, T41, A1
/// and their variants. A caller can therefore offer it as a default without knowing
/// the exact part.
///
/// The value is **\[COMMUNITY\]**, from thingino-dfu. It is unpinned until a board
/// on the bench boots from it, and a build that links its SPL elsewhere overrides
/// it.
pub const SPL_LOAD_ADDRESS: u32 = 0x8000_1800;

/// The load-and-entry address of stage2, the DFU-capable U-Boot, in the DRAM stage1
/// brought up.
///
/// It is uniform across the family, as [`SPL_LOAD_ADDRESS`] is. It is also the
/// classic Ingenic U-Boot link address, DRAM base `0x8000_0000` plus 1 MiB. The value
/// is **\[COMMUNITY\]**, from thingino-dfu.
pub const UBOOT_LOAD_ADDRESS: u32 = 0x8010_0000;

/// The default DRAM-init settle after stage1, in milliseconds: the time the memory
/// controller needs to bring DRAM up before stage2 can be loaded into it.
///
/// thingino-dfu's T31 profile uses 2000 ms (`ddr_init_wait_ms`). Other SoCs can
/// differ, so a caller offers this value as an editable starting point, not a
/// settled one. **\[COMMUNITY\]**
pub const DEFAULT_SETTLE_MS: u32 = 2000;

/// One stage of an Ingenic bootstrap: a named blob, where it loads, and where it
/// runs.
///
/// The load and entry addresses are per-SoC facts the caller supplies. The Ingenic
/// boot ROM takes an explicit address, where the Rockchip download-boot selects a
/// region with a fixed `wIndex`. The addresses are **\[UNVERIFIED\]** until a
/// board's own loader build pins them.
#[derive(Debug, Clone)]
pub struct Stage {
    /// The stage's name, for progress and for an error that names the stage the
    /// boot ROM refused. The name separates a bad DRAM init from a U-Boot too large
    /// for the memory it was told to load into.
    pub name: String,
    /// The blob bytes, uploaded over the bulk pipe exactly as given.
    pub data: Vec<u8>,
    /// The address `VR_SET_DATA_ADDRESS` points the upload at.
    pub load_address: u32,
    /// The address `VR_PROGRAM_START1`/`VR_PROGRAM_START2` jumps to. It is usually
    /// the same as [`load_address`](Stage::load_address). It is a separate field
    /// because the boot ROM takes the two as separate values, and a loader can use
    /// different ones.
    pub entry_address: u32,
    /// Milliseconds to settle after this stage is started. For stage1, it is the
    /// time the memory controller needs to bring DRAM up before stage2 is loaded
    /// into it, ~2 s **\[COMMUNITY\]**. If the delay is not honored, stage2 can land
    /// in memory that is not yet ready.
    pub settle_ms: u32,
}

impl Stage {
    /// Wrap a transport failure with which stage it hit and what the driver was
    /// doing, keeping the cause's own words.
    fn failed(&self, doing: &str, cause: Error) -> Error {
        Error::Protocol(format!(
            "bootstrapping the Ingenic board failed while {doing} for stage '{}': {}",
            self.name,
            detail(&cause)
        ))
    }

    /// Wrap a bulk-upload failure with the offset in the stage where it stopped. A
    /// stage too large for its target memory then reports how many bytes the boot
    /// ROM accepted, not a bare "the write failed".
    fn failed_at(&self, done_in_stage: usize, cause: Error) -> Error {
        Error::Protocol(format!(
            "uploading the Ingenic stage '{}' failed at byte {} of {}: {}",
            self.name,
            done_in_stage,
            self.data.len(),
            detail(&cause)
        ))
    }
}

/// The cause's own message, without re-nesting a `Protocol` error's Display
/// prefix. The Rockchip driver unwraps the same way, so a wrapped stall reads as
/// one sentence.
fn detail(cause: &Error) -> String {
    match cause {
        Error::Protocol(message) => message.clone(),
        other => other.to_string(),
    }
}

/// A two-stage Ingenic loader: a DRAM-init blob, then an optional DFU-capable
/// U-Boot.
///
/// Stage1 runs from SRAM with `VR_PROGRAM_START1`. An optional stage2 runs from the
/// DRAM stage1 brought up, with `VR_PROGRAM_START2`. A loader with no stage2
/// initializes DRAM and stops. That partial bootstrap is useful only for probing,
/// because nothing then re-enumerates.
#[derive(Debug, Clone)]
pub struct IngenicLoader {
    /// The DRAM-init stage (the SPL), run from SRAM.
    pub stage1: Stage,
    /// The DFU-capable U-Boot, run from DRAM. With `None`, the bootstrap stops
    /// after DRAM init.
    pub stage2: Option<Stage>,
}

/// One entry in a bootstrap's upload plan: a stage, its `VR_*` start request, and
/// whether the start tears down the boot ROM USB device.
///
/// Only the stage2 jump into U-Boot tears the device down, so only that step
/// accepts the board leaving the bus as it acknowledges the start.
struct PlanStep<'a> {
    stage: &'a Stage,
    program_start: fn(u32) -> Setup,
    tears_down_usb: bool,
}

/// Upload a loader to an Ingenic boot-ROM board, and return the SoC magic it
/// reported.
///
/// The board is identified first, with `VR_GET_CPU_INFO`, and then each stage is
/// uploaded and started in turn. Progress is reported in bytes of stage payload.
///
/// [`cancel`](Cancel) is checked before identification, before each bulk chunk, and
/// again before each stage's start request. A canceled bootstrap therefore stops
/// between transfers, never mid-transfer, and never starts a stage it has not fully
/// uploaded. No check follows a start request, so a started stage's settle delay
/// always runs in full. A cancel after stage1 starts takes effect once stage2's
/// address and length requests have gone out. A cancel after the last start
/// request has no effect.
///
/// The caller records the returned [`CpuInfo`] as the board's identity, and later
/// arms the write gate with it. This is the only place it can be read, as the
/// module documentation explains. On success, the board re-enumerates as a DFU
/// gadget, a different device on the bus. The caller must discover and open it
/// again, as after the Rockchip loader re-enumerates.
pub async fn download_boot<T: Transport>(
    transport: &mut T,
    loader: &IngenicLoader,
    progress: ProgressSink<'_>,
    cancel: &Cancel,
) -> Result<CpuInfo> {
    if loader.stage1.data.is_empty() {
        return Err(Error::InvalidRequest(
            "the Ingenic loader has an empty stage1, so there is no DRAM-init code to upload"
                .to_string(),
        ));
    }
    if cancel.is_canceled() {
        return Err(Error::Canceled);
    }

    // Identify before uploading anything: the magic is only available now, and a
    // ROM that does not answer here is one there is no point uploading to.
    let cpu = identify(transport).await?;

    let total =
        loader.stage1.data.len() as u64 + loader.stage2.as_ref().map_or(0, |s| s.data.len() as u64);
    progress(Progress::Started { total_bytes: total });

    // stage1 runs from SRAM and returns to the ROM, so it does not tear the USB
    // device down; stage2 jumps into U-Boot, which does. The start request and
    // that one distinction are all the two stages differ by.
    let mut plan = vec![PlanStep {
        stage: &loader.stage1,
        program_start: ingenic_boot::program_start1,
        tears_down_usb: false,
    }];
    if let Some(stage2) = &loader.stage2 {
        plan.push(PlanStep {
            stage: stage2,
            program_start: ingenic_boot::program_start2,
            tears_down_usb: true,
        });
    }

    let mut done = 0u64;
    for step in plan {
        let stage = step.stage;
        // Tell the ROM where the payload loads and how long it is.
        control_out(
            transport,
            ingenic_boot::set_data_address(stage.load_address),
        )
        .await
        .map_err(|cause| stage.failed("setting the load address", cause))?;
        let len = u32::try_from(stage.data.len()).map_err(|_| {
            Error::InvalidRequest(format!(
                "the Ingenic stage '{}' is {} bytes, larger than the 32-bit length the ROM takes",
                stage.name,
                stage.data.len()
            ))
        })?;
        control_out(transport, ingenic_boot::set_data_length(len))
            .await
            .map_err(|cause| stage.failed("setting the payload length", cause))?;

        // The payload over the bulk pipe, a window at a time.
        let mut done_in_stage = 0usize;
        for chunk in stage.data.chunks(BULK_CHUNK) {
            if cancel.is_canceled() {
                return Err(Error::Canceled);
            }
            transport
                .write_bulk(chunk)
                .await
                .map_err(|cause| stage.failed_at(done_in_stage, cause))?;
            done_in_stage += chunk.len();
            done += chunk.len() as u64;
            progress(Progress::Advanced {
                done_bytes: done,
                total_bytes: total,
            });
        }

        // Jump to it. The stage2 jump tears the ROM USB device down, so the
        // device often leaves the bus before acknowledging -- that is the jump
        // working, the same way a rockusb reset does, and the only ending that
        // is not a failure here.
        if cancel.is_canceled() {
            return Err(Error::Canceled);
        }
        match control_out(transport, (step.program_start)(stage.entry_address)).await {
            Ok(()) => {}
            Err(Error::Disconnected) if step.tears_down_usb => {}
            Err(cause) => return Err(stage.failed("starting the uploaded stage", cause)),
        }

        crate::progress::settle(stage.settle_ms).await;
    }

    progress(Progress::Finished { done_bytes: done });
    Ok(cpu)
}

/// Ask the boot ROM which SoC it is, and parse its 8-byte reply.
async fn identify<T: Transport>(transport: &mut T) -> Result<CpuInfo> {
    let setup = ingenic_boot::get_cpu_info();
    let reply = transport
        .control(Control::In {
            request_type: setup.request_type,
            request: setup.request,
            value: setup.value,
            index: setup.index,
            length: setup.length,
        })
        .await?;
    CpuInfo::parse(&reply)
}

/// Issue one `VR_*` host-to-device request. Every one of them carries its
/// operands in `wValue`/`wIndex`, so the data stage is empty: a zero-length
/// control OUT.
async fn control_out<T: Transport>(transport: &mut T, setup: Setup) -> Result<()> {
    transport
        .control(Control::Out {
            request_type: setup.request_type,
            request: setup.request,
            value: setup.value,
            index: setup.index,
            data: &[],
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::testing::{ScriptedTransport, Step};

    /// The 8-byte magic a scripted boot ROM answers `VR_GET_CPU_INFO` with. The
    /// bytes are arbitrary, because the driver returns them uninterpreted.
    const MAGIC: [u8; 8] = *b"T31\0\0\0\0\0";

    /// The `ExpectControlIn` for the identify step, answering with [`MAGIC`].
    fn expect_identify() -> Step {
        let setup = ingenic_boot::get_cpu_info();
        Step::ExpectControlIn {
            request_type: setup.request_type,
            request: setup.request,
            value: setup.value,
            index: setup.index,
            length: setup.length,
            reply: MAGIC.to_vec(),
        }
    }

    /// The `ExpectControlOut` for a no-data `VR_*` request, built from its `Setup`.
    fn expect_control(setup: Setup) -> Step {
        Step::ExpectControlOut {
            request_type: setup.request_type,
            request: setup.request,
            value: setup.value,
            index: setup.index,
            data: Vec::new(),
        }
    }

    /// The steps a faithful upload of one stage makes: address, length, the bulk
    /// payload a window at a time, then the jump.
    fn expect_stage(stage: &Stage, program_start: fn(u32) -> Setup) -> Vec<Step> {
        let mut steps = vec![
            expect_control(ingenic_boot::set_data_address(stage.load_address)),
            expect_control(ingenic_boot::set_data_length(stage.data.len() as u32)),
        ];
        steps.extend(
            stage
                .data
                .chunks(BULK_CHUNK)
                .map(|chunk| Step::ExpectWrite(chunk.to_vec())),
        );
        steps.push(expect_control(program_start(stage.entry_address)));
        steps
    }

    fn stage(name: &str, data: Vec<u8>, load: u32, entry: u32) -> Stage {
        Stage {
            name: name.to_string(),
            data,
            load_address: load,
            entry_address: entry,
            settle_ms: 0, // no real sleep in a test
        }
    }

    #[test]
    fn a_two_stage_bootstrap_identifies_then_uploads_each_stage_to_its_address() {
        // A stage1 spanning several bulk windows, and a smaller stage2.
        let data1: Vec<u8> = (0..BULK_CHUNK + 500).map(|i| i as u8).collect();
        let data2: Vec<u8> = vec![0xa5; 4096];
        let stage1 = stage("spl", data1.clone(), 0x8000_0000, 0x8000_0000);
        let stage2 = stage("u-boot", data2.clone(), 0x8010_0000, 0x8010_0000);
        let loader = IngenicLoader {
            stage1: stage1.clone(),
            stage2: Some(stage2.clone()),
        };

        let mut script = vec![expect_identify()];
        script.extend(expect_stage(&stage1, ingenic_boot::program_start1));
        script.extend(expect_stage(&stage2, ingenic_boot::program_start2));

        let mut transport = ScriptedTransport::new(script);
        let mut events = Vec::new();
        let cpu = pollster::block_on(download_boot(
            &mut transport,
            &loader,
            &mut |p| events.push(p),
            &Cancel::new(),
        ))
        .expect("the bootstrap follows the script");
        transport.assert_drained();

        // The identity is captured and returned uninterpreted.
        assert_eq!(cpu.magic, MAGIC);

        let total = (data1.len() + data2.len()) as u64;
        assert_eq!(
            events.first(),
            Some(&Progress::Started { total_bytes: total })
        );
        assert_eq!(
            events.last(),
            Some(&Progress::Finished { done_bytes: total })
        );
    }

    /// A stage1-only loader initializes DRAM and stops. It starts with
    /// `VR_PROGRAM_START1`, which does not tear the USB device down, and there is
    /// no second stage or teardown to accept.
    #[test]
    fn a_stage1_only_loader_starts_via_program_start1_and_stops() {
        let data1 = vec![0x11; 200];
        let stage1 = stage("spl", data1.clone(), 0x8000_0000, 0x8000_0000);
        let loader = IngenicLoader {
            stage1: stage1.clone(),
            stage2: None,
        };

        let mut script = vec![expect_identify()];
        script.extend(expect_stage(&stage1, ingenic_boot::program_start1));

        let mut transport = ScriptedTransport::new(script);
        let cpu = pollster::block_on(download_boot(
            &mut transport,
            &loader,
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect("a stage1-only bootstrap follows the script");
        transport.assert_drained();
        assert_eq!(cpu.magic, MAGIC);
    }

    /// The stage2 jump tears the boot ROM USB device down, so the board leaves the
    /// bus as it acknowledges `VR_PROGRAM_START2`. The driver treats that
    /// disconnect on the final jump as success, as a rockusb reset does.
    #[test]
    fn the_device_leaving_the_bus_on_the_final_jump_is_success() {
        let data1 = vec![0x11; 64];
        let data2 = vec![0x22; 64];
        let stage1 = stage("spl", data1, 0x8000_0000, 0x8000_0000);
        let stage2 = stage("u-boot", data2, 0x8010_0000, 0x8010_0000);
        let loader = IngenicLoader {
            stage1: stage1.clone(),
            stage2: Some(stage2.clone()),
        };

        // The whole conversation up to the final PROGRAM_START2, which the device
        // answers by dropping off the bus instead of acknowledging.
        let mut script = vec![expect_identify()];
        script.extend(expect_stage(&stage1, ingenic_boot::program_start1));
        script.push(expect_control(ingenic_boot::set_data_address(
            stage2.load_address,
        )));
        script.push(expect_control(ingenic_boot::set_data_length(
            stage2.data.len() as u32,
        )));
        script.push(Step::ExpectWrite(stage2.data.clone()));
        script.push(Step::Disconnect); // the START2 jump tears USB down

        let mut transport = ScriptedTransport::new(script);
        let cpu = pollster::block_on(download_boot(
            &mut transport,
            &loader,
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect("the device leaving on the final jump is the jump working");
        transport.assert_drained();
        assert_eq!(cpu.magic, MAGIC);
    }

    /// A boot ROM whose `VR_GET_CPU_INFO` reply fails to parse stops the bootstrap
    /// before any stage byte goes out. Nothing is uploaded to a board that cannot
    /// identify itself. Here the identify step answers with fewer than the eight
    /// bytes of the magic. No stage steps follow in the script, so an upload that
    /// reached the transport would panic.
    #[test]
    fn a_rom_whose_identify_reply_is_malformed_stops_before_uploading() {
        let stage1 = stage("spl", vec![0x11; 64], 0x8000_0000, 0x8000_0000);
        let loader = IngenicLoader {
            stage1,
            stage2: None,
        };

        let setup = ingenic_boot::get_cpu_info();
        let mut transport = ScriptedTransport::new(vec![Step::ExpectControlIn {
            request_type: setup.request_type,
            request: setup.request,
            value: setup.value,
            index: setup.index,
            length: setup.length,
            reply: vec![b'T', b'3', b'1'], // three bytes, not the eight the magic is
        }]);

        let err = pollster::block_on(download_boot(
            &mut transport,
            &loader,
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect_err("a malformed identify reply stops the bootstrap");
        assert!(matches!(err, Error::Protocol(_)), "{err:?}");
        transport.assert_drained();
    }

    /// An empty stage1 is refused before the transport is touched, because there
    /// is no DRAM-init code to upload. The script is empty, so any transfer would
    /// panic.
    #[test]
    fn an_empty_stage1_is_refused() {
        let loader = IngenicLoader {
            stage1: stage("spl", Vec::new(), 0, 0),
            stage2: None,
        };
        let mut transport = ScriptedTransport::new(vec![]);
        let err = pollster::block_on(download_boot(
            &mut transport,
            &loader,
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect_err("an empty stage1 has nothing to upload");
        assert!(matches!(err, Error::InvalidRequest(_)), "{err:?}");
        transport.assert_drained();
    }

    #[test]
    fn a_canceled_bootstrap_stops_before_it_identifies() {
        let loader = IngenicLoader {
            stage1: stage("spl", vec![0u8; 32], 0x8000_0000, 0x8000_0000),
            stage2: None,
        };
        // Pre-canceled: nothing should reach the transport, so an empty script
        // that would panic on any transfer proves it.
        let mut transport = ScriptedTransport::new(vec![]);
        let cancel = Cancel::new();
        cancel.cancel();

        let result =
            pollster::block_on(download_boot(&mut transport, &loader, &mut |_| {}, &cancel));
        assert!(matches!(result, Err(Error::Canceled)), "{result:?}");
        transport.assert_drained();
    }
}
