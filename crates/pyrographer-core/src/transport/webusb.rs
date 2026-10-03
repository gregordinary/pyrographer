//! WebUSB: the transport in a browser tab.
//!
//! This module is the browser's implementation of the [`Transport`] seam, and the
//! reason the seam exists. The code that consumes it is the same code that runs
//! against nusb on a desktop, from the codecs to the verbs and the gate. None of
//! that code knows which transport it drives.
//!
//! # Differences from the native transport
//!
//! ## Discovery
//!
//! A page cannot enumerate a bus. The browser gives it a device through
//! [`request_device`], which opens a chooser the page did not draw and cannot
//! script. The page cannot pre-select a row in the chooser, and cannot open it
//! without a real user gesture. Discovery therefore differs in the seam, not only
//! in its implementation. [`crate::discovery`] holds the classification rule both
//! builds share.
//!
//! ## Deadlines
//!
//! The host sets its own deadlines. WebUSB gives a transfer no timeout, so a
//! transfer to a wedged device is a promise that never settles. A job waiting on
//! that promise would wait forever, and its cancel button could not stop it. The
//! cancellation token is checked at window boundaries, and a transfer in progress
//! is not at one. Every transfer therefore races a timer, and whichever settles
//! first is the result, as [`raced`] shows.
//!
//! # Browser support
//!
//! WebUSB needs three things:
//!
//! - A Chromium browser: Chrome, Edge or Chromium
//! - A secure context: HTTPS, or `localhost`
//! - On Windows, a WinUSB driver, as the native libusb tools need
//!
//! WebUSB refuses to claim an interface whose class is Mass Storage, and rockusb's
//! interface is not of that class. rockusb uses mass-storage framing, but
//! enumerates as vendor-specific (`0xFF`) under VID `0x2207`, so the browser hands
//! it over. **\[DOC\]**
//!
//! The scripted-transport tests pin every byte layout this transport carries, which
//! it shares with the native path. This file itself is **\[UNVERIFIED\]** against
//! a browser and a board.

use std::time::Duration;

use js_sys::{Array, Promise, Uint8Array};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    UsbControlTransferParameters, UsbDevice, UsbDeviceFilter, UsbDeviceRequestOptions,
    UsbDirection, UsbEndpointType, UsbInTransferResult, UsbOutTransferResult, UsbRecipient,
    UsbRequestType, UsbTransferStatus,
};

use super::web::{after, await_js, describe, timed_out};
use super::{Control, DfuOpen, Transport};
use crate::codec::dfu;
use crate::codec::dfu_alt::{self, AltSetting};
use crate::{Error, Result};

/// How long the host waits for one control transfer.
///
/// It is ten seconds, matching the native transport. Four paths share it: the
/// maskrom download-boot, the Ingenic boot ROM's `VR_*` requests, DFU's requests,
/// and the `GET_DESCRIPTOR` in [`open_dfu`](WebUsbTransport::open_dfu). The
/// download-boot sets the length.
///
/// Each download-boot transfer writes a 4 KiB chunk into the chip's SRAM. The
/// first such chunk to a real RK3576 maskrom took longer than a second. The native
/// constant is ten seconds rather than one for that reason. A shorter deadline here
/// would abort the browser "Upload loader" flow at byte 0 with a spurious timeout.
const CONTROL_DEADLINE: Duration = Duration::from_secs(10);

/// How long the host waits for one bulk transfer.
///
/// It is ten seconds, as in the native transport. A bulk transfer moves up to
/// 16 KiB, and no working device takes close to this long to move it. The number
/// is not a budget. It marks the point at which a device has stopped answering.
const BULK_DEADLINE: Duration = Duration::from_secs(10);

/// The DFU interface class (application-specific), from the DFU 1.1 spec.
const DFU_INTERFACE_CLASS: u8 = 0xFE;

/// The DFU interface subclass, from the DFU 1.1 spec.
const DFU_INTERFACE_SUBCLASS: u8 = 0x01;

/// The standard `GET_DESCRIPTOR` request.
const GET_DESCRIPTOR: u8 = 0x06;

/// The configuration descriptor type, the high byte of `GET_DESCRIPTOR`'s
/// `wValue`.
const DESCRIPTOR_TYPE_CONFIGURATION: u8 = 0x02;

/// The length of a configuration descriptor's header, which carries the
/// descriptor's total length.
const CONFIGURATION_DESCRIPTOR_HEADER_LEN: u16 = 9;

/// The `bmRequestType` type field: bits 5 and 6.
const REQUEST_TYPE_MASK: u8 = 0b0110_0000;
/// The `bmRequestType` recipient field: bits 0 to 4.
const RECIPIENT_MASK: u8 = 0b0001_1111;

/// A link to a USB device the browser handed to the page.
pub struct WebUsbTransport {
    device: UsbDevice,
    /// The endpoint number of the bulk IN pipe, or `None` for a device without
    /// one. A maskrom board opened for the download-boot uses control transfers
    /// alone and has none.
    bulk_in: Option<u8>,
    /// The endpoint number of the bulk OUT pipe, or `None` for a device without
    /// one.
    bulk_out: Option<u8>,
}

/// `navigator.usb`, or a refusal that says the browser has no WebUSB.
///
/// The property is checked rather than assumed. `navigator.usb` is absent outside
/// Chromium and in an insecure context, and reaching through it there throws a
/// JavaScript exception. In a wasm build, that exception is a panic that takes the
/// whole instance down, not an error a caller can report. [`list_permitted`] runs
/// at page load, so that panic would be the first thing a Firefox or Safari
/// visitor met. The property is therefore tested first, and its absence is an
/// [`Error`] with a reason a person can act on.
fn usb() -> Result<web_sys::Usb> {
    let navigator = web_sys::window()
        .ok_or_else(|| Error::Transport("there is no window to reach USB through".to_string()))?
        .navigator();

    let found = js_sys::Reflect::get(navigator.as_ref(), &JsValue::from_str("usb"))
        .map_err(|e| Error::Transport(format!("cannot reach navigator.usb: {}", describe(&e))))?;

    if found.is_undefined() || found.is_null() {
        return Err(Error::Transport(
            "this browser does not offer WebUSB. pyrographer's web flasher needs a \
             Chromium-based browser (Chrome, Edge, or Chromium) on a secure origin: HTTPS or \
             localhost."
                .to_string(),
        ));
    }

    Ok(navigator.usb())
}

/// Ask the person to pick a device, and return the device they picked.
///
/// This is the acquisition seam. The browser does not enumerate a bus for a page,
/// and `requestDevice` is the only way to obtain a device. It opens a dialog the
/// page did not draw and cannot script. It fails unless it is called from a real
/// user gesture, and the web build's write confirmation depends on that property.
///
/// `Ok(None)` means the person closed the chooser without picking a device. That
/// is a result, not a failure.
///
/// The filter is by vendor ID, never by product ID. A product ID names an SoC
/// family, and a table of them would go stale with every new part.
/// [`crate::discovery::ROCKCHIP_VID`] explains the rule.
///
/// **One chooser carries every vendor's filter**, because a browser opens one
/// chooser per user gesture. A person with an Ingenic board picks it without first
/// naming its vendor. The native scan differs here. It makes a VID-filtered pass
/// per vendor, so it knows a device's vendor by the pass that found it. In a
/// browser the device arrives with no such record, and
/// [`Vendor::from_vid`](crate::discovery::Vendor::from_vid) reads the vendor from
/// the device itself.
///
/// Pass [`USB_VENDOR_IDS`](crate::discovery::USB_VENDOR_IDS) to offer every board
/// pyrographer has a backend for.
pub async fn request_device(vendor_ids: &[u16]) -> Result<Option<UsbDevice>> {
    let usb = usb()?;

    let filters: Vec<UsbDeviceFilter> = vendor_ids
        .iter()
        .map(|vendor_id| {
            let filter = UsbDeviceFilter::new();
            filter.set_vendor_id(*vendor_id);
            filter
        })
        .collect();

    let options = UsbDeviceRequestOptions::new(&filters);
    let picked = JsFuture::from(usb.request_device(&options)).await;

    match picked {
        Ok(device) => Ok(Some(device)),
        // The chooser rejects when it is dismissed, and it rejects when the page
        // has no permission to open one. They are not the same thing, and only
        // the first is a person deciding not to.
        Err(error) => {
            if is_not_found(&error) {
                Ok(None)
            } else {
                Err(Error::Transport(format!(
                    "the browser would not open a device chooser: {}",
                    describe(&error)
                )))
            }
        }
    }
}

/// List the devices this origin has already been given permission for.
///
/// **This is not a bus scan.** `getDevices` returns only the devices a person has
/// already granted this origin through the chooser.
///
/// A board that was never picked is absent from the list, even while it is plugged
/// in and enumerated. A board granted once reappears on the next page load without
/// another trip through the chooser. [`request_device`] cannot do that, because it
/// needs a user gesture every time.
///
/// The permission is per origin and outlives the tab, so the list survives a
/// reload. It lasts until the person revokes the grant in the browser's own site
/// settings, the only place a grant can be revoked.
///
/// The list is filtered by vendor ID, the rule [`request_device`] asks the chooser
/// for and the native scan uses. [`crate::discovery::ROCKCHIP_VID`] explains why
/// the filter never uses a product ID. The function takes the same set of IDs the
/// chooser was opened with, so remembered devices and pickable devices pass the
/// same filter.
///
/// **\[UNVERIFIED\]** against a browser. The specification says a grant persists
/// across a reload, and that has not been observed here.
pub async fn list_permitted(vendor_ids: &[u16]) -> Result<Vec<UsbDevice>> {
    let usb = usb()?;

    let devices = JsFuture::from(usb.get_devices()).await.map_err(|e| {
        Error::Transport(format!(
            "the browser would not list the devices it has permission for: {}",
            describe(&e)
        ))
    })?;

    Ok(devices
        .into_iter()
        .filter(|device: &UsbDevice| vendor_ids.contains(&device.vendor_id()))
        .collect())
}

/// Whether a rejected `requestDevice` is a chooser somebody closed.
///
/// A dialog dismissed with nothing selected rejects with a `NotFoundError`. Every
/// other rejection is a failure, such as a page with no user gesture behind it or
/// a context that is not secure. Telling the two apart keeps a canceled dialog from
/// being reported as a broken one.
fn is_not_found(error: &JsValue) -> bool {
    describe(error).contains("NotFoundError")
}

impl WebUsbTransport {
    /// Open a device the browser handed over, and claim the interface that
    /// carries its boot protocol.
    ///
    /// A device in a boot state exposes exactly one bulk pipe pair, so the
    /// transport finds its endpoints here and owns them. The native transport has
    /// the same shape, and neither takes an endpoint address on every call.
    pub async fn open(device: UsbDevice) -> Result<Self> {
        await_js(device.open(), "opening the device").await?;

        // A device that has just enumerated may have no configuration selected.
        // The boot protocols all live on the first one.
        if device.configuration().is_none() {
            await_js(device.select_configuration(1), "selecting configuration 1").await?;
        }

        let pair = bulk_pair(&device).ok_or_else(|| {
            Error::Protocol(
                "the device has no interface with a bulk IN and a bulk OUT endpoint, so it does \
                 not use a boot protocol pyrographer supports"
                    .to_string(),
            )
        })?;

        await_js(
            device.claim_interface(pair.interface),
            "claiming the interface",
        )
        .await
        .map_err(claim_error)?;

        Ok(Self {
            device,
            bulk_in: Some(pair.bulk_in),
            bulk_out: Some(pair.bulk_out),
        })
    }

    /// Open a maskrom device for the download-boot, over control transfers alone.
    ///
    /// It is the browser's counterpart to `UsbTransport::open_maskrom`. A maskrom
    /// board takes a loader over vendor control transfers, not a bulk pair. This
    /// function therefore claims interface 0 for the control path and leaves the
    /// bulk endpoints empty. The bulk methods refuse on the transport it returns.
    ///
    /// Whether the browser claims interface 0 and passes endpoint-0 control
    /// transfers on a real maskrom board is **\[UNVERIFIED\]**. This path has not
    /// run in a browser or against a board.
    pub async fn open_maskrom(device: UsbDevice) -> Result<Self> {
        await_js(device.open(), "opening the device").await?;

        if device.configuration().is_none() {
            await_js(device.select_configuration(1), "selecting configuration 1").await?;
        }

        await_js(device.claim_interface(0), "claiming the interface")
            .await
            .map_err(claim_error)?;

        Ok(Self {
            device,
            bulk_in: None,
            bulk_out: None,
        })
    }

    /// Open an Ingenic DFU gadget for the flash path.
    ///
    /// It is the browser's counterpart to `UsbTransport::open_dfu`. It reaches the
    /// same three facts by a different route. The DFU interface and its
    /// alt-settings are browser objects. `USBAlternateInterface` carries the
    /// class and subclass that identify the interface. It also carries
    /// `interfaceName`, the string descriptor nusb must fetch separately, so the
    /// partition names need no extra request.
    ///
    /// The browser does not expose the functional descriptor. WebUSB exposes no
    /// class-specific descriptor, and `wTransferSize` is only in that descriptor.
    /// The configuration is therefore fetched as bytes with a standard
    /// `GET_DESCRIPTOR`, and parsed by [`dfu::functional_in_configuration`]. A
    /// flashing tool must not guess a transfer size, so this open refuses without
    /// one, as the native open does.
    ///
    /// DFU uses endpoint 0, so the bulk pipes stay empty, and the bulk methods
    /// refuse, as they do for a maskrom board.
    ///
    /// It is **\[UNVERIFIED\]** in three places:
    ///
    /// - That Chromium passes a standard `GET_DESCRIPTOR` through at all
    /// - What an Ingenic U-Boot's DFU descriptors say
    /// - That `interfaceName` is populated before an interface is claimed
    pub async fn open_dfu(device: UsbDevice) -> Result<DfuOpen<Self>> {
        await_js(device.open(), "opening the device").await?;

        if device.configuration().is_none() {
            await_js(device.select_configuration(1), "selecting configuration 1").await?;
        }

        let configuration = device.configuration().ok_or_else(|| {
            Error::Protocol(
                "the device offers no configuration to read its interfaces from".to_string(),
            )
        })?;

        // The DFU interface: application-specific class, DFU subclass. An
        // interface declares those on each of its alt-settings, so the search is
        // over all of them and the first match names the interface.
        let mut interface_number = None;
        let mut alternates = Vec::new();
        for interface in configuration.interfaces().iter() {
            let mut matched = Vec::new();
            for alternate in interface.alternates().iter() {
                if alternate.interface_class() == DFU_INTERFACE_CLASS
                    && alternate.interface_subclass() == DFU_INTERFACE_SUBCLASS
                {
                    matched.push(alternate);
                }
            }
            if !matched.is_empty() {
                interface_number = Some(interface.interface_number());
                alternates = matched;
                break;
            }
        }
        let Some(interface_number) = interface_number else {
            return Err(Error::Protocol(
                "the device declares no DFU interface (class 0xFE, subclass 0x01), so it is not \
                 the DFU gadget pyrographer expects a bootstrapped board to present"
                    .to_string(),
            ));
        };

        // Each alt-setting of the DFU interface is a partition, named by its
        // interface string. A name the browser did not populate leaves the
        // partition named by its index, so the board is still usable with a
        // label missing -- the same fallback the native open makes for a string
        // descriptor that will not read.
        let mut alts: Vec<AltSetting> = alternates
            .iter()
            .map(|alternate| {
                let number = alternate.alternate_setting();
                alternate
                    .interface_name()
                    .as_deref()
                    .and_then(|string| dfu_alt::parse_alt(number, string).ok())
                    .unwrap_or_else(|| AltSetting {
                        index: number,
                        name: format!("alt{number}"),
                        size: None,
                    })
            })
            .collect();
        // Table order is alt-setting order, however the descriptors were laid out.
        alts.sort_by_key(|alt| alt.index);

        // The transfer size that caps a block, from the one descriptor the
        // browser will not hand over as an object.
        let mut transport = Self {
            device,
            bulk_in: None,
            bulk_out: None,
        };
        let raw = transport.configuration_descriptor().await?;
        let functional = dfu::functional_in_configuration(&raw, interface_number)?;
        if !functional.can_upload() {
            return Err(Error::Protocol(
                "the DFU interface does not advertise UPLOAD, so its flash cannot be read back"
                    .to_string(),
            ));
        }

        await_js(
            transport.device.claim_interface(interface_number),
            "claiming the interface",
        )
        .await
        .map_err(claim_error)?;

        Ok(DfuOpen {
            transport,
            interface: u16::from(interface_number),
            functional,
            alts,
        })
    }

    /// Fetch the active configuration descriptor as raw bytes.
    ///
    /// This is the one place this transport reads a descriptor from the wire.
    /// WebUSB exposes interfaces and endpoints as objects and nothing more. A
    /// class-specific descriptor, such as the DFU functional descriptor and its
    /// transfer size, has no property to read it from. A standard `GET_DESCRIPTOR`
    /// reads the bytes, and changes nothing on the device. That Chromium passes it
    /// through is **\[UNVERIFIED\]**, as [`open_dfu`](Self::open_dfu) records.
    ///
    /// The fetch takes two transfers, because the length is in the answer. The
    /// nine-byte header carries `wTotalLength`, and the second transfer asks for
    /// exactly that many bytes. A round number large enough to cover most
    /// descriptors would be a guess about the device's descriptor layout.
    ///
    /// The descriptor index is the active configuration's, one less than its
    /// `configurationValue`. `GET_DESCRIPTOR` addresses configurations by
    /// zero-based index, and a configuration names itself by value. On a
    /// single-configuration device, which every board here is, those are 0 and 1.
    async fn configuration_descriptor(&mut self) -> Result<Vec<u8>> {
        let value = self
            .device
            .configuration()
            .map(|configuration| configuration.configuration_value())
            .unwrap_or(1);
        let index = u16::from(value.saturating_sub(1));

        let header = self
            .control(Control::In {
                request_type: 0x80,
                request: GET_DESCRIPTOR,
                value: (u16::from(DESCRIPTOR_TYPE_CONFIGURATION) << 8) | index,
                index: 0,
                length: CONFIGURATION_DESCRIPTOR_HEADER_LEN,
            })
            .await?;

        if header.len() < usize::from(CONFIGURATION_DESCRIPTOR_HEADER_LEN) {
            return Err(Error::Protocol(format!(
                "the device answered a configuration descriptor request with {} bytes, which is \
                 shorter than its header",
                header.len()
            )));
        }
        let total = u16::from_le_bytes([header[2], header[3]]);
        if usize::from(total) < usize::from(CONFIGURATION_DESCRIPTOR_HEADER_LEN) {
            return Err(Error::Protocol(format!(
                "the device claims a configuration descriptor of {total} bytes, which is shorter \
                 than its own header"
            )));
        }

        self.control(Control::In {
            request_type: 0x80,
            request: GET_DESCRIPTOR,
            value: (u16::from(DESCRIPTOR_TYPE_CONFIGURATION) << 8) | index,
            index: 0,
            length: total,
        })
        .await
    }

    /// The `UsbDevice` this transport drives, so a caller can tell one board from
    /// another.
    ///
    /// The web build's write confirmation depends on this method. A person picks
    /// the destination again from the chooser. The re-pick is a confirmation
    /// because the picked board is compared with the board the plan was made for.
    /// That comparison uses `Object.is` over the `USBDevice`, and is
    /// **\[UNVERIFIED\]**.
    pub fn device(&self) -> &UsbDevice {
        &self.device
    }
}

/// The error a bulk call gives on a transport opened for control transfers only.
fn no_bulk_pipe() -> Error {
    Error::Protocol(
        "this transport opened a device for control transfers only (a maskrom board), so it has \
         no bulk pipe to read or write"
            .to_string(),
    )
}

/// A bulk pipe pair, and the interface it is on.
struct BulkPair {
    interface: u8,
    bulk_in: u8,
    bulk_out: u8,
}

/// Find the interface with a bulk IN and a bulk OUT endpoint.
///
/// It makes the same search the native transport makes over nusb's descriptors,
/// over the browser's objects instead. Mass Storage is not skipped here. The
/// browser refuses to claim that class, and [`crate::discovery`] has already
/// classified a device that presents it as out of pyrographer's reach.
fn bulk_pair(device: &UsbDevice) -> Option<BulkPair> {
    let configuration = device.configuration()?;

    for interface in configuration.interfaces().iter() {
        let alternate = interface.alternate();

        let mut bulk_in = None;
        let mut bulk_out = None;

        for endpoint in alternate.endpoints().iter() {
            if endpoint.type_() != UsbEndpointType::Bulk {
                continue;
            }
            match endpoint.direction() {
                UsbDirection::In => bulk_in.get_or_insert(endpoint.endpoint_number()),
                UsbDirection::Out => bulk_out.get_or_insert(endpoint.endpoint_number()),
                _ => continue,
            };
        }

        if let (Some(bulk_in), Some(bulk_out)) = (bulk_in, bulk_out) {
            return Some(BulkPair {
                interface: interface.interface_number(),
                bulk_in,
                bulk_out,
            });
        }
    }
    None
}

impl Transport for WebUsbTransport {
    async fn control(&mut self, req: Control<'_>) -> Result<Vec<u8>> {
        match req {
            Control::In {
                request_type,
                request,
                value,
                index,
                length,
            } => {
                let setup = setup(request_type, request, value, index)?;
                let result: UsbInTransferResult = raced(
                    self.device.control_transfer_in(&setup, length),
                    CONTROL_DEADLINE,
                    "control IN",
                )
                .await?;

                check(result.status(), "control IN")?;
                Ok(bytes_of(result.data().as_ref()))
            }

            Control::Out {
                request_type,
                request,
                value,
                index,
                data,
            } => {
                let setup = setup(request_type, request, value, index)?;

                let result: UsbOutTransferResult = raced(
                    self.device
                        .control_transfer_out_with_u8_slice(&setup, data)
                        .map_err(|e| {
                            Error::Transport(format!(
                                "the browser refused a control OUT: {}",
                                describe(&e)
                            ))
                        })?,
                    CONTROL_DEADLINE,
                    "control OUT",
                )
                .await?;

                check(result.status(), "control OUT")?;
                Ok(Vec::new())
            }
        }
    }

    async fn write_bulk(&mut self, data: &[u8]) -> Result<()> {
        let bulk_out = self.bulk_out.ok_or_else(no_bulk_pipe)?;
        let result: UsbOutTransferResult = raced(
            self.device
                .transfer_out_with_u8_slice(bulk_out, data)
                .map_err(|e| {
                    Error::Transport(format!("the browser refused a bulk OUT: {}", describe(&e)))
                })?,
            BULK_DEADLINE,
            "bulk OUT",
        )
        .await?;

        // Clear a halted endpoint before reporting the stall, exactly as the
        // native transport does. WebUSB does not auto-clear one, so without this
        // every command after a stall fails until physical replug.
        let status = result.status();
        if matches!(status, UsbTransferStatus::Stall) {
            clear_halt(&self.device, UsbDirection::Out, bulk_out).await;
        }
        check(status, "bulk OUT")?;

        // A short write is the host failing to put the bytes on the wire, which
        // is not a thing a device did and not a thing to read a status into.
        let written = result.bytes_written() as usize;
        if written != data.len() {
            return Err(Error::Transport(format!(
                "bulk OUT sent {written} of {} bytes",
                data.len()
            )));
        }
        Ok(())
    }

    async fn read_bulk(&mut self, len: usize) -> Result<Vec<u8>> {
        let bulk_in = self.bulk_in.ok_or_else(no_bulk_pipe)?;
        let wanted = u32::try_from(len).map_err(|_| {
            Error::InvalidRequest(format!(
                "a bulk IN of {len} bytes exceeds the 32-bit length a WebUSB transfer takes"
            ))
        })?;

        let result: UsbInTransferResult = raced(
            self.device.transfer_in(bulk_in, wanted),
            BULK_DEADLINE,
            "bulk IN",
        )
        .await?;

        // Clear a halted endpoint before reporting the stall, as native does.
        let status = result.status();
        if matches!(status, UsbTransferStatus::Stall) {
            clear_halt(&self.device, UsbDirection::In, bulk_in).await;
        }
        check(status, "bulk IN")?;
        let bytes = bytes_of(result.data().as_ref());

        // A device that puts more on the wire than was asked for has left the
        // host counting bytes it cannot account for. The surplus is the error --
        // trimming it would hide it, and it would be misread as the next
        // command's reply. The native transport holds the same line; the agent
        // above both of them is built on it.
        if bytes.len() > len {
            return Err(Error::Protocol(format!(
                "bulk IN answered a {len}-byte request with {} bytes. The device and the host are \
                 no longer synchronized",
                bytes.len()
            )));
        }
        Ok(bytes)
    }

    /// Select an alt-setting with `selectAlternateInterface()`, in place of the
    /// seam's default `SET_INTERFACE` control transfer.
    ///
    /// The browser keeps its own record of which alt-setting an interface is on,
    /// and offers `selectAlternateInterface()` for changing it. A standard
    /// `SET_INTERFACE` sent directly would leave that record disagreeing with the
    /// device. The request therefore goes through the API the browser provides,
    /// and that is why [`Transport::select_alt_setting`] is a seam method.
    ///
    /// The interface number is the DFU interface the agent drives. It arrives as
    /// a `u16`, the width of a `wIndex`, and WebUSB takes it as the byte it is.
    async fn select_alt_setting(&mut self, interface: u16, alt: u8) -> Result<()> {
        let interface = u8::try_from(interface).map_err(|_| {
            Error::InvalidRequest(format!(
                "interface {interface} is not an interface number a device can have"
            ))
        })?;

        await_js(
            self.device.select_alternate_interface(interface, alt),
            "selecting an alt-setting",
        )
        .await
    }
}

/// The SETUP packet, from the raw `bmRequestType` byte the seam carries.
///
/// The seam keeps that byte raw so it stays vendor-neutral, because a vendor's
/// protocol reference gives the byte. It is decoded here, because WebUSB takes the
/// type and the recipient as enums rather than as bits.
fn setup(
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
) -> Result<UsbControlTransferParameters> {
    let kind = match request_type & REQUEST_TYPE_MASK {
        0b0000_0000 => UsbRequestType::Standard,
        0b0010_0000 => UsbRequestType::Class,
        0b0100_0000 => UsbRequestType::Vendor,
        other => {
            return Err(Error::InvalidRequest(format!(
                "{other:#04x} is not a control request type the USB specification defines"
            )));
        }
    };

    let recipient = match request_type & RECIPIENT_MASK {
        0 => UsbRecipient::Device,
        1 => UsbRecipient::Interface,
        2 => UsbRecipient::Endpoint,
        3 => UsbRecipient::Other,
        other => {
            return Err(Error::InvalidRequest(format!(
                "{other:#04x} is not a control request recipient the USB specification defines"
            )));
        }
    };

    Ok(UsbControlTransferParameters::new(
        index, recipient, request, kind, value,
    ))
}

/// Race a transfer against the clock.
///
/// WebUSB gives a transfer no timeout of its own, and a promise that never
/// settles is a job that never ends. The cancellation token is checked at window
/// boundaries, and a transfer in progress is not at one. A cancel button
/// therefore cannot reach a host stuck here. The transfer races a `setTimeout`,
/// and whichever settles first is the result.
///
/// The losing transfer is not canceled, because WebUSB has no way to cancel it.
/// After a timeout, the device can still deliver bytes that nothing is waiting
/// for. An agent that consumes this transport already refuses every later command
/// in that case, because [`Error::Timeout`] leaves it
/// [`Desynchronized`](Error::Desynchronized). That is why desynchronization is a
/// state of the agent and not only a failure.
///
/// The settled value says which of the two won. The timer resolves with a marker
/// no transfer result can be, and `timed_out` reads it.
async fn raced<T>(transfer: Promise<T>, deadline: Duration, what: &'static str) -> Result<T>
where
    T: JsCast + wasm_bindgen::convert::FromWasmAbi + 'static,
{
    let timer = after(deadline)?;
    let race = Promise::race(Array::of2(transfer.as_ref(), timer.as_ref()).as_ref());

    let settled = JsFuture::from(race)
        .await
        .map_err(|e| Error::Transport(format!("{what} failed: {}", describe(&e))))?;

    if timed_out(&settled) {
        return Err(Error::Timeout {
            what,
            waited: deadline,
        });
    }

    settled.dyn_into::<T>().map_err(|_| {
        Error::Protocol(format!(
            "{what} settled with something that is not a USB transfer result"
        ))
    })
}

/// Clear a stalled bulk endpoint, mirroring the native transport's `clear_halt`.
///
/// WebUSB does not clear a halt automatically. The user agent leaves the endpoint
/// halted until the page calls `clearHalt`, and reopening the device does not
/// clear it. Without this call, every command after a stall fails until the device
/// is physically replugged. The call's own result is discarded. The stall itself
/// is still returned as the error, and clearing only lets the next command
/// through, as on the native side.
async fn clear_halt(device: &UsbDevice, direction: UsbDirection, endpoint: u8) {
    let _ = await_js(
        device.clear_halt(direction, endpoint),
        "clearing a stalled endpoint",
    )
    .await;
}

/// Turn a transfer status into an error, or `Ok(())` for a successful transfer.
fn check(status: UsbTransferStatus, what: &'static str) -> Result<()> {
    match status {
        UsbTransferStatus::Ok => Ok(()),
        // The device refused the command outright. It is not a timeout and not a
        // broken pipe: it is an answer, and it is "no".
        UsbTransferStatus::Stall => Err(Error::Protocol(format!(
            "the device stalled {what}: it refused the command outright"
        ))),
        UsbTransferStatus::Babble => Err(Error::Protocol(format!(
            "the device sent more on {what} than the endpoint can carry"
        ))),
        _ => Err(Error::Transport(format!("{what} failed"))),
    }
}

/// The bytes behind a `DataView`, copied out.
///
/// A data-in phase arrives as a view over the browser's own buffer, and the
/// caller gets a `Vec`. The native transport returns the same type, so no code
/// that consumes the seam needs to know a `DataView` exists.
fn bytes_of(data: Option<&js_sys::DataView>) -> Vec<u8> {
    let Some(view) = data else {
        return Vec::new();
    };
    Uint8Array::new_with_byte_offset_and_length(
        &view.buffer(),
        view.byte_offset() as u32,
        view.byte_length() as u32,
    )
    .to_vec()
}

/// Turn a failed interface claim into an error that names the remedy.
///
/// The browser refuses to claim an interface another driver holds, and it refuses
/// one whose class is Mass Storage. Both arrive here as the same rejection, and
/// both have the same remedy for a person: something else has the device.
fn claim_error(error: Error) -> Error {
    Error::Busy(format!(
        "{error}. The browser does not claim an interface that another driver holds, and it \
         never claims a Mass Storage interface. On Windows, the device needs a WinUSB driver."
    ))
}
