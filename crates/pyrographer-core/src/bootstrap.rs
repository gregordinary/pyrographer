//! The bootstrap that brings a maskrom board to loader mode.
//!
//! A board in maskrom runs a BootROM and nothing else. DRAM is uninitialized and
//! the flash is unreachable. This module uploads a loader into the chip over
//! vendor control transfers: the 471 DRAM-init blobs, then the 472 loader. The
//! board then re-enumerates in loader mode, and every verb that consumes
//! [`FlashAgent`](crate::agent::FlashAgent) can drive it.
//!
//! The byte layouts are sans-I/O codecs in [`codec::rkboot`](crate::codec::rkboot).
//! That module parses the loader container and builds each section's wire payload:
//! the stored bytes, the pad rule and the trailing CRC. This module is the I/O
//! half. It runs the control-transfer sequence that carries those bytes to the
//! device, one chunk at a time. Progress and cancellation work as they do in the
//! flash verbs.
//!
//! The whole path is verified on a real RK3576. The upload runs at ~455 KiB/s over
//! endpoint 0. The `0x0472` jump tears down the maskrom USB device, and a loader
//! re-enumerates at a new address somewhat over three seconds later.
//!
//! The device that reappears is a different device on the bus, and needs a fresh
//! discovery and open. [`download_boot`] therefore returns as soon as the upload
//! completes, and the caller finds the board again. A watcher needs a window
//! longer than the three seconds the re-enumeration takes.
//!
//! [`download_boot`] is the Rockchip maskrom path. The submodule [`ingenic`] does
//! the same job for an Ingenic XBurst board. Its wire protocol differs: `VR_*`
//! vendor requests on endpoint 0, a bulk payload, and a two-stage upload. Both
//! bring a board with nothing usable on flash to a state a flash agent can drive.

pub mod ingenic;

use crate::codec::rkboot::{self, CHUNK_SIZE, LoaderImage};
use crate::progress::{Cancel, Progress, ProgressSink};
use crate::transport::{Control, Transport};
use crate::{Error, Result};

/// `bmRequestType` for the download: vendor, host-to-device, device recipient.
const REQUEST_TYPE: u8 = 0x40;
/// `bRequest` for the download-boot memory write.
const REQUEST_DOWNLOAD: u8 = 0x0c;
/// `wIndex` that loads a section into SRAM and calls it: the 471 DRAM-init code.
const CODE_471: u16 = 0x0471;
/// `wIndex` that loads a section into DRAM, tears down USB, and jumps: the 472
/// loader.
const CODE_472: u16 = 0x0472;

/// One section's wire payload, ready to upload, and where it goes.
///
/// It carries the container's own name for the section, so a failure names the
/// stage the BootROM refused. The two stages fail for different reasons. A
/// rejected DRAM init points to a bad blob. A rejected loader can point to a
/// BootROM that will not take a payload this large.
struct Section {
    /// `wIndex`: [`CODE_471`] or [`CODE_472`].
    code: u16,
    /// The container's name for it (`UsbHead`, `u-boot-rockchip-usb472`).
    name: String,
    /// The prepared bytes: the stored data, the pad rule, the trailing CRC.
    payload: Vec<u8>,
    /// Milliseconds to settle after it, from the container.
    delay_ms: u32,
}

impl Section {
    /// Turn a transport failure into one that says where in this section it
    /// happened.
    ///
    /// A bare "the device stalled control OUT" cannot distinguish a BootROM that
    /// refused the first chunk from one that took 340 KiB and then stopped. On the
    /// multi-megabyte 472 of a RAM-booted mainline U-Boot, the offset shows how
    /// much the BootROM accepted before it stopped. The error keeps the
    /// transport's own message (a stall and a timeout mean different things), with
    /// the byte offset in front of it.
    fn failed_at(&self, done_in_section: usize, cause: Error) -> Error {
        // The cause's own message, without re-nesting its Display prefix: a
        // stall is `Protocol`, so its inner string is the sentence to quote.
        let detail = match &cause {
            Error::Protocol(message) => message.clone(),
            other => other.to_string(),
        };
        Error::Protocol(format!(
            "uploading CODE{:x} ({}) to the maskrom board failed at byte {} of {}: {}",
            self.code,
            self.name,
            done_in_section,
            self.payload.len(),
            detail
        ))
    }
}

/// Upload a loader to a maskrom board over the download-boot control transfers.
///
/// Every 471 section is uploaded first, each followed by the delay the container
/// asks for (DRAM settling time). The 472 loader is uploaded next.
/// [`rkboot::download_payload`] prepares each section, which is sent in
/// [`CHUNK_SIZE`]-byte control transfers. A chunk shorter than that marks the
/// section's end. A section whose length is an exact multiple of [`CHUNK_SIZE`]
/// gets a one-byte terminating transfer, the reference's "pend packet", to signal
/// its end.
///
/// Progress is reported in bytes of prepared payload. [`cancel`](Cancel) is
/// checked at every chunk boundary, so a canceled upload stops between transfers
/// and never mid-transfer. On success, the board re-enumerates in loader mode as a
/// different device. The caller must discover and open it again, as the module
/// documentation explains.
pub async fn download_boot<T: Transport>(
    transport: &mut T,
    loader: &LoaderImage,
    progress: ProgressSink<'_>,
    cancel: &Cancel,
) -> Result<()> {
    // Prepare every section's wire payload up front. A loader is under a
    // megabyte and is already wholly in memory as `loader`, so unlike a flash
    // image there is nothing here to stream and nothing to hold that was not
    // already held.
    let sections: Vec<Section> = loader
        .code_471
        .iter()
        .map(|b| (CODE_471, b))
        .chain(loader.code_472.iter().map(|b| (CODE_472, b)))
        .map(|(code, blob)| Section {
            code,
            name: blob.name.clone(),
            payload: rkboot::download_payload(&blob.data),
            delay_ms: blob.delay_ms,
        })
        .collect();

    if sections.is_empty() {
        return Err(Error::InvalidRequest(
            "loader file: it has no 471 or 472 sections, so there is nothing to bootstrap with"
                .to_string(),
        ));
    }

    let total_bytes: u64 = sections.iter().map(|s| s.payload.len() as u64).sum();
    progress(Progress::Started { total_bytes });

    let mut done: u64 = 0;
    for section in &sections {
        let mut done_in_section = 0usize;
        for chunk in section.payload.chunks(CHUNK_SIZE) {
            if cancel.is_canceled() {
                return Err(Error::Canceled);
            }
            transport
                .control(Control::Out {
                    request_type: REQUEST_TYPE,
                    request: REQUEST_DOWNLOAD,
                    value: 0,
                    index: section.code,
                    data: chunk,
                })
                .await
                .map_err(|cause| section.failed_at(done_in_section, cause))?;
            done_in_section += chunk.len();
            done += chunk.len() as u64;
            progress(Progress::Advanced {
                done_bytes: done,
                total_bytes,
            });
        }

        // An exact multiple of the chunk size ends on a full chunk, which the
        // BootROM does not read as end-of-section; one extra byte -- a single
        // zero, the reference's "pend packet" (`RKU_DeviceRequest`,
        // `bSendPendPacket`) -- supplies the short transfer that does. One byte
        // and not zero: a zero-length control OUT is a setup packet with no
        // data stage at all, which is not a short chunk. Unexercised on
        // hardware: no section of the loaders seen so far lands on the
        // boundary.
        if section.payload.len().is_multiple_of(CHUNK_SIZE) {
            transport
                .control(Control::Out {
                    request_type: REQUEST_TYPE,
                    request: REQUEST_DOWNLOAD,
                    value: 0,
                    index: section.code,
                    data: &[0],
                })
                .await
                .map_err(|cause| section.failed_at(done_in_section, cause))?;
        }

        crate::progress::settle(section.delay_ms).await;
    }

    progress(Progress::Finished { done_bytes: done });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::rkboot::CodeBlob;
    use crate::transport::testing::{ScriptedTransport, Step};

    /// A section, and the control-OUT steps a faithful upload of it would make.
    fn expect_section(code: u16, payload: &[u8]) -> Vec<Step> {
        let mut steps: Vec<Step> = payload
            .chunks(CHUNK_SIZE)
            .map(|chunk| Step::ExpectControlOut {
                request_type: REQUEST_TYPE,
                request: REQUEST_DOWNLOAD,
                value: 0,
                index: code,
                data: chunk.to_vec(),
            })
            .collect();
        if payload.len().is_multiple_of(CHUNK_SIZE) {
            steps.push(Step::ExpectControlOut {
                request_type: REQUEST_TYPE,
                request: REQUEST_DOWNLOAD,
                value: 0,
                index: code,
                data: vec![0],
            });
        }
        steps
    }

    fn blob(name: &str, data: Vec<u8>) -> CodeBlob {
        CodeBlob {
            name: name.to_string(),
            data,
            delay_ms: 0,
        }
    }

    #[test]
    fn the_upload_sends_each_section_to_its_own_code_in_chunks() {
        // A 471 section spanning several chunks, and a 472 section. RC4 off, so
        // the expected payload is the data plus its CRC and the wire bytes are
        // easy to reason about.
        let data_471: Vec<u8> = (0..CHUNK_SIZE + 100).map(|i| i as u8).collect();
        let data_472: Vec<u8> = vec![0xab; 64];
        let loader = LoaderImage {
            chip: None,
            code_471: vec![blob("UsbHead", data_471.clone())],
            code_472: vec![blob("Loader", data_472.clone())],
            rc4_disabled: true,
        };

        let payload_471 = rkboot::download_payload(&data_471);
        let payload_472 = rkboot::download_payload(&data_472);
        let mut script = expect_section(CODE_471, &payload_471);
        script.extend(expect_section(CODE_472, &payload_472));

        let mut transport = ScriptedTransport::new(script);
        let mut events = Vec::new();
        pollster::block_on(download_boot(
            &mut transport,
            &loader,
            &mut |p| events.push(p),
            &Cancel::new(),
        ))
        .expect("the upload follows the script");
        transport.assert_drained();

        let total = (payload_471.len() + payload_472.len()) as u64;
        assert_eq!(
            events.first(),
            Some(&Progress::Started { total_bytes: total })
        );
        assert_eq!(
            events.last(),
            Some(&Progress::Finished { done_bytes: total })
        );
    }

    #[test]
    fn an_exact_multiple_of_the_chunk_size_gets_a_one_byte_terminating_transfer() {
        // A payload of exactly CHUNK_SIZE would end on a full chunk; the driver
        // must add a one-byte transfer -- the pend packet -- so the section's
        // end is still signaled. download_payload appends 2 CRC bytes, so the
        // data is CHUNK_SIZE - 2.
        let data = vec![0x5a; CHUNK_SIZE - 2];
        let payload = rkboot::download_payload(&data);
        assert!(
            payload.len().is_multiple_of(CHUNK_SIZE),
            "the fixture is an exact multiple"
        );

        let loader = LoaderImage {
            chip: None,
            code_471: vec![blob("UsbHead", data)],
            code_472: vec![],
            rc4_disabled: true,
        };

        let mut transport = ScriptedTransport::new(expect_section(CODE_471, &payload));
        pollster::block_on(download_boot(
            &mut transport,
            &loader,
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect("the terminating transfer is in the script");
        transport.assert_drained();
    }

    /// A loader built from raw sections goes on the wire exactly as a parsed
    /// container's would: same codes, same chunking, same payload treatment.
    /// This is the binman-artifact path (bare usb471/usb472 files with no
    /// container). The test pins that the missing container changes nothing in the
    /// upload.
    #[test]
    fn a_raw_built_loader_uploads_like_a_parsed_one() {
        let data_471: Vec<u8> = (0..300).map(|i| i as u8).collect();
        let data_472: Vec<u8> = vec![0xcd; CHUNK_SIZE + 17];
        let loader = LoaderImage::from_raw(
            Some(("u-boot-rockchip-usb471.bin".to_string(), data_471.clone())),
            Some(("u-boot-rockchip-usb472.bin".to_string(), data_472.clone())),
        );

        let mut script = expect_section(CODE_471, &rkboot::download_payload(&data_471));
        script.extend(expect_section(
            CODE_472,
            &rkboot::download_payload(&data_472),
        ));

        let mut transport = ScriptedTransport::new(script);
        pollster::block_on(download_boot(
            &mut transport,
            &loader,
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect("the raw sections follow the same script");
        transport.assert_drained();
    }

    /// A BootROM can take part of a section and then refuse a chunk. The upload
    /// then fails with the byte offset it stopped at and the transport's own
    /// reason. That pattern is the signature of a 472 too large for the download
    /// window. The offset turns "the device stalled control OUT" into a
    /// measurement.
    #[test]
    fn a_stall_partway_through_a_section_reports_where_it_stopped() {
        // A 472 spanning two chunks: the first is accepted, the second stalls.
        let data = vec![0xcd; CHUNK_SIZE + 100];
        let loader = LoaderImage::from_raw(
            None,
            Some(("u-boot-rockchip-usb472".to_string(), data.clone())),
        );
        let payload = rkboot::download_payload(&data);

        let script = vec![
            Step::ExpectControlOut {
                request_type: REQUEST_TYPE,
                request: REQUEST_DOWNLOAD,
                value: 0,
                index: CODE_472,
                data: payload[..CHUNK_SIZE].to_vec(),
            },
            Step::StallControlOut,
        ];

        let mut transport = ScriptedTransport::new(script);
        let error = pollster::block_on(download_boot(
            &mut transport,
            &loader,
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect_err("the second chunk stalls");

        let Error::Protocol(message) = error else {
            panic!("a stalled upload is a protocol error: {error:?}");
        };
        // Which stage, where it stopped, and the transport's own reason.
        assert!(message.contains("CODE472"), "names the stage: {message}");
        assert!(
            message.contains("u-boot-rockchip-usb472"),
            "names the section: {message}"
        );
        assert!(
            message.contains(&format!("byte {CHUNK_SIZE} of {}", payload.len())),
            "reports the offset: {message}"
        );
        assert!(
            message.contains("stalled control OUT"),
            "keeps the cause: {message}"
        );
    }

    #[test]
    fn a_canceled_upload_stops_before_the_first_transfer() {
        let loader = LoaderImage {
            chip: None,
            code_471: vec![blob("UsbHead", vec![0u8; 32])],
            code_472: vec![],
            rc4_disabled: true,
        };
        // No scripted steps: a pre-canceled token must return before any control
        // transfer is issued, so none is expected.
        let mut transport = ScriptedTransport::new(vec![]);
        let cancel = Cancel::new();
        cancel.cancel();

        let result =
            pollster::block_on(download_boot(&mut transport, &loader, &mut |_| {}, &cancel));
        assert!(matches!(result, Err(Error::Canceled)), "{result:?}");
        transport.assert_drained();
    }

    #[test]
    fn a_loader_with_no_sections_is_refused() {
        let loader = LoaderImage {
            chip: None,
            code_471: vec![],
            code_472: vec![],
            rc4_disabled: true,
        };
        let mut transport = ScriptedTransport::new(vec![]);
        let result = pollster::block_on(download_boot(
            &mut transport,
            &loader,
            &mut |_| {},
            &Cancel::new(),
        ));
        assert!(
            matches!(result, Err(Error::InvalidRequest(_))),
            "{result:?}"
        );
    }
}
