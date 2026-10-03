//! The native USB transport, backed by nusb.
//!
//! nusb calls the operating system's USB stack, so this module is compiled on
//! native targets only. The browser's implementation of the same [`Transport`]
//! seam is `WebUsbTransport`, in the sibling `webusb` module.
//!
//! **This transport blocks the thread it is driven on.** Every transfer has a
//! wall-clock deadline, and nusb enforces it by blocking until the transfer
//! completes or the deadline passes. The methods are `async` because the seam is
//! async, and the browser's implementation suspends. This one blocks instead.
//!
//! The blocking costs nothing under `pollster`, whose `block_on` parks the same
//! thread for as long. The GUI runs each verb on a thread of its own. A multi-task
//! executor that drove two devices on one thread would pay for the blocking.
//! Supporting one means changing this module.
//!
//! nusb returns its blocking syscalls as a [`MaybeFuture`]. That covers listing,
//! opening and claiming a device. It also covers the control transfers and the
//! halt-clears, whose deadlines usbfs enforces inside the ioctl itself.
//!
//! Awaited bare, a `MaybeFuture` needs a `smol` or `tokio` runtime and panics
//! without one. Its panic message says to enable a runtime, which is the wrong fix
//! here. pyrographer runs no async runtime on purpose, so every `MaybeFuture` in
//! this module is taken with [`MaybeFuture::wait`]. The syscall then runs on this
//! thread and blocks it, as every transfer in this module does.

use nusb::descriptors::TransferType;
use nusb::transfer::{
    Buffer, Bulk, ControlIn, ControlOut, ControlType, In, Out, Recipient, TransferError,
};
use nusb::{Endpoint, Interface, MaybeFuture};
use std::time::Duration;

use super::{Control, DfuOpen, Transport};
use crate::codec::dfu;
use crate::codec::dfu_alt::{self, AltSetting};
use crate::discovery::DeviceInfo;
use crate::{Error, Result};

/// How long a control transfer can take before it is abandoned.
///
/// The deadline is generous on purpose. A 1-second deadline is too short: the
/// first download-boot chunk sent to a real RK3576 in maskrom did not complete
/// inside it. Whether that BootROM is slow to arm its first data stage, or was
/// never going to answer, is unsettled. A usbmon capture taken alongside the
/// reference tool can settle it. The deadline costs time only on a device that
/// has already stopped answering, so it errs long.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);

/// How long one bulk transfer can take before it is abandoned.
///
/// This bounds a single transfer, not an operation. A dump is thousands of
/// transfers, and each gets the whole deadline. A 16 KiB chunk crosses a USB-2
/// gadget in milliseconds, but the CSW that ends a write waits for the flash to
/// finish programming. The deadline is therefore generous on purpose. It is high
/// enough that no healthy device reaches it, and low enough that a wedged device
/// fails within seconds. It is not measured against hardware. **\[WEAK\]**
const BULK_TIMEOUT: Duration = Duration::from_secs(10);

/// The `bmRequestType` type field: bits 5 and 6.
///
/// The fourth value is reserved and has no encoding. A request that names it is
/// refused, rather than coerced into a SETUP packet that says something else.
fn control_type(request_type: u8) -> Result<ControlType> {
    match (request_type >> 5) & 0b11 {
        0 => Ok(ControlType::Standard),
        1 => Ok(ControlType::Class),
        2 => Ok(ControlType::Vendor),
        _ => Err(Error::Protocol(format!(
            "bmRequestType {request_type:#04x} names a reserved request type"
        ))),
    }
}

/// The `bmRequestType` recipient field: bits 0 through 4.
///
/// Values above `Other` are reserved, and refused for the same reason.
fn recipient(request_type: u8) -> Result<Recipient> {
    match request_type & 0b1_1111 {
        0 => Ok(Recipient::Device),
        1 => Ok(Recipient::Interface),
        2 => Ok(Recipient::Endpoint),
        3 => Ok(Recipient::Other),
        _ => Err(Error::Protocol(format!(
            "bmRequestType {request_type:#04x} names a reserved recipient"
        ))),
    }
}

/// The native USB transport, backed by nusb.
pub struct UsbTransport {
    /// Held for the interface claim, and for control transfers.
    interface: Interface,
    /// The bulk pipe pair, or `None` for a transport opened without one. A
    /// loader-mode board always has one. A maskrom board opened by
    /// [`open_maskrom`](UsbTransport::open_maskrom) speaks over control transfers
    /// alone and has none. The bulk methods then refuse, rather than reach for a
    /// pipe that was never claimed.
    bulk: Option<BulkPipes>,
}

/// The two bulk endpoints a boot-protocol device uses.
struct BulkPipes {
    bulk_in: Endpoint<Bulk, In>,
    bulk_out: Endpoint<Bulk, Out>,
}

impl UsbTransport {
    /// Open a device found by [`discovery`](crate::discovery) and claim its
    /// bulk interface.
    ///
    /// Listing and classifying a device reads only what the operating system
    /// cached at enumeration, so it needs no permission. This is the first call
    /// that opens the device, so a missing udev rule surfaces here as
    /// [`Error::AccessDenied`]. The error carries a hint that says how to fix it.
    pub async fn open(info: &DeviceInfo) -> Result<Self> {
        let device_info = nusb::list_devices()
            .wait()
            .map_err(|e| Error::Transport(format!("cannot list USB devices: {e}")))?
            .find(|d| d.bus_id() == info.bus_id && d.device_address() == info.device_address)
            .ok_or(Error::DeviceNotFound)?;

        let device = device_info
            .open()
            .wait()
            .map_err(|e| open_error(e, info.vendor_id, info.product_id))?;

        let config = device
            .active_configuration()
            .map_err(|e| Error::Transport(format!("cannot read the active configuration: {e}")))?;

        let pair = bulk_interface(&config).ok_or_else(|| {
            Error::Protocol(
                "the device has no interface with a bulk IN and bulk OUT endpoint, so it does \
                 not use a boot protocol pyrographer supports"
                    .to_string(),
            )
        })?;

        // Detaching first covers a device that re-enumerated as mass storage,
        // where usb-storage has already bound the interface. It is a no-op
        // where no driver is attached, and off Linux.
        let interface = device
            .detach_and_claim_interface(pair.interface)
            .wait()
            .map_err(|e| open_error(e, info.vendor_id, info.product_id))?;

        // A claim leaves the interface on alternate setting 0, so a pair declared
        // on any other setting has to be selected or the endpoints below open
        // against a setting that does not carry them. It happens here, before
        // they are opened: nusb requires no endpoint to be open on the interface
        // when the setting changes.
        if pair.alt_setting != 0 {
            interface
                .set_alt_setting(pair.alt_setting)
                .wait()
                .map_err(|e| {
                    Error::Transport(format!(
                        "cannot select alternate setting {} on interface {}: {e}",
                        pair.alt_setting, pair.interface
                    ))
                })?;
        }

        let bulk_in = interface.endpoint::<Bulk, In>(pair.bulk_in).map_err(|e| {
            Error::Transport(format!("cannot open bulk IN {:#04x}: {e}", pair.bulk_in))
        })?;
        let bulk_out = interface
            .endpoint::<Bulk, Out>(pair.bulk_out)
            .map_err(|e| {
                Error::Transport(format!("cannot open bulk OUT {:#04x}: {e}", pair.bulk_out))
            })?;

        Ok(Self {
            interface,
            bulk: Some(BulkPipes { bulk_in, bulk_out }),
        })
    }

    /// Open a maskrom device for the download-boot, over control transfers alone.
    ///
    /// A maskrom board's BootROM takes a loader over vendor control transfers on
    /// endpoint 0, not over a bulk pair. The RK3576 BootROM presents the same bulk
    /// pair a loader does, with nothing serving it. This function claims interface
    /// 0, where the Rockchip BootROM presents itself, for endpoint-0 control. It
    /// leaves the bulk pipe pair empty, so the bulk methods refuse on the transport
    /// it returns.
    ///
    /// Interface 0 is verified against a real RK3576 (2026-07-17) as the interface
    /// to claim for endpoint-0 control. On that board, the interface claim and
    /// endpoint-0 control uploaded the loader end to end at ~455 KiB/s. The
    /// `0x0472` jump then re-enumerated the board in loader mode.
    pub async fn open_maskrom(info: &DeviceInfo) -> Result<Self> {
        let device_info = nusb::list_devices()
            .wait()
            .map_err(|e| Error::Transport(format!("cannot list USB devices: {e}")))?
            .find(|d| d.bus_id() == info.bus_id && d.device_address() == info.device_address)
            .ok_or(Error::DeviceNotFound)?;

        let device = device_info
            .open()
            .wait()
            .map_err(|e| open_error(e, info.vendor_id, info.product_id))?;

        let interface = device
            .detach_and_claim_interface(0)
            .wait()
            .map_err(|e| open_error(e, info.vendor_id, info.product_id))?;

        Ok(Self {
            interface,
            bulk: None,
        })
    }

    /// Open a DFU gadget for the flash path, and read what the agent needs from its
    /// descriptors.
    ///
    /// It is the counterpart of [`open`](UsbTransport::open) for a bootstrapped
    /// Ingenic board. DFU uses control transfers to an interface, not a bulk pair.
    /// This function therefore claims the DFU interface for endpoint-0 control and
    /// leaves the bulk pipes empty, as [`open_maskrom`](UsbTransport::open_maskrom)
    /// does.
    ///
    /// Unlike the maskrom open, it also reads the DFU interface's shape. The
    /// [`functional`](DfuOpen::functional) descriptor's transfer size caps a block,
    /// and the [`alts`](DfuOpen::alts) are the board's partitions. A DFU agent
    /// addresses named alt-settings, and needs to know them before it reads.
    ///
    /// The DFU interface is the one that declares the DFU class:
    /// application-specific `0xFE`, subclass `0x01`. Its functional descriptor gives
    /// the transfer size, and each alt-setting's interface string names a
    /// partition. A partition whose name cannot be read is named by its index, so a
    /// missing string descriptor costs a label and the open still succeeds.
    ///
    /// **\[UNVERIFIED\]** until an Ingenic DFU board is on the bench. Such a board
    /// settles what an Ingenic U-Boot's descriptors say: the class it declares, and
    /// the strings it sets.
    pub async fn open_dfu(info: &DeviceInfo) -> Result<DfuOpen<Self>> {
        let device_info = nusb::list_devices()
            .wait()
            .map_err(|e| Error::Transport(format!("cannot list USB devices: {e}")))?
            .find(|d| d.bus_id() == info.bus_id && d.device_address() == info.device_address)
            .ok_or(Error::DeviceNotFound)?;

        let device = device_info
            .open()
            .wait()
            .map_err(|e| open_error(e, info.vendor_id, info.product_id))?;

        let config = device
            .active_configuration()
            .map_err(|e| Error::Transport(format!("cannot read the active configuration: {e}")))?;

        // The DFU interface: application-specific class, DFU subclass.
        let interface_number = config
            .interface_alt_settings()
            .find(|alt| {
                alt.class() == DFU_INTERFACE_CLASS && alt.subclass() == DFU_INTERFACE_SUBCLASS
            })
            .map(|alt| alt.interface_number())
            .ok_or_else(|| {
                Error::Protocol(
                    "the device declares no DFU interface (class 0xFE, subclass 0x01), so it is \
                     not the DFU gadget pyrographer expects a bootstrapped board to present"
                        .to_string(),
                )
            })?;

        // The functional descriptor gives the transfer size that caps a block, and
        // says whether the interface can be read back at all.
        let functional = dfu_functional(&config, interface_number)?;
        if !functional.can_upload() {
            return Err(Error::Protocol(
                "the DFU interface does not advertise UPLOAD, so its flash cannot be read back"
                    .to_string(),
            ));
        }

        // The language string names are asked in: the device's first supported one,
        // or US English, which is what nearly every device answers to.
        let language = device
            .get_string_descriptor_supported_languages(CONTROL_TIMEOUT)
            .wait()
            .ok()
            .and_then(|mut languages| languages.next())
            .unwrap_or(nusb::descriptors::language_id::US_ENGLISH);

        // Each alt-setting of the DFU interface is a partition, named by its
        // interface string. A string that will not read leaves the partition named
        // by its index, so the board is still usable with a label missing.
        let mut alts: Vec<AltSetting> = config
            .interface_alt_settings()
            .filter(|alt| alt.interface_number() == interface_number)
            .map(|alt| {
                let number = alt.alternate_setting();
                let name = alt.string_index().and_then(|index| {
                    device
                        .get_string_descriptor(index, language, CONTROL_TIMEOUT)
                        .wait()
                        .ok()
                });
                name.as_deref()
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

        // Claim the DFU interface for endpoint-0 control. DFU rides no bulk pair, so
        // the pipes stay empty and the bulk methods refuse, as they do for maskrom.
        let interface = device
            .detach_and_claim_interface(interface_number)
            .wait()
            .map_err(|e| open_error(e, info.vendor_id, info.product_id))?;

        Ok(DfuOpen {
            transport: Self {
                interface,
                bulk: None,
            },
            interface: u16::from(interface_number),
            functional,
            alts,
        })
    }
}

/// The DFU interface class (application-specific), from the DFU 1.1 spec.
const DFU_INTERFACE_CLASS: u8 = 0xFE;

/// The DFU interface subclass, from the DFU 1.1 spec.
const DFU_INTERFACE_SUBCLASS: u8 = 0x01;

/// Find and parse the DFU functional descriptor carried on the DFU interface.
///
/// The functional descriptor (type `0x21`) follows the interface descriptor in the
/// configuration, so it is looked for among the DFU interface's own descriptors.
/// Its absence is an [`Error::Protocol`]. A DFU interface without one gives the
/// host no transfer size, and a flashing tool must not guess one.
fn dfu_functional(
    config: &nusb::descriptors::ConfigurationDescriptor,
    interface_number: u8,
) -> Result<dfu::Functional> {
    for alt in config.interface_alt_settings() {
        if alt.interface_number() != interface_number {
            continue;
        }
        for descriptor in alt.descriptors() {
            if descriptor.descriptor_type() == dfu::FUNCTIONAL_DESCRIPTOR_TYPE {
                return dfu::Functional::parse(&descriptor[..]);
            }
        }
    }
    Err(Error::Protocol(
        "the DFU interface carries no functional descriptor (type 0x21), so its transfer size is \
         unknown"
            .to_string(),
    ))
}

/// The error a bulk call gives on a transport opened for control transfers only.
fn no_bulk_pipe() -> Error {
    Error::Protocol(
        "this transport opened a device for control transfers only (a maskrom board), so it has \
         no bulk pipe to read or write"
            .to_string(),
    )
}

/// A bulk pipe pair: where it is declared, and the two endpoint addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BulkPair {
    /// The interface carrying the pair.
    interface: u8,
    /// The alternate setting the pair is declared on. A claim leaves the
    /// interface on setting 0, so any other setting must be selected.
    alt_setting: u8,
    /// The bulk IN endpoint's address.
    bulk_in: u8,
    /// The bulk OUT endpoint's address.
    bulk_out: u8,
}

/// Find the interface alternate setting that carries a bulk pipe pair.
///
/// Rockchip's loader declares its pair on interface 0, alternate setting 0.
/// Reading the pair from the descriptors rather than hard-coding it lets the same
/// transport serve the other vendors. That includes a vendor that declares its
/// pair on a non-zero alternate setting, which the caller then selects.
fn bulk_interface(config: &nusb::descriptors::ConfigurationDescriptor) -> Option<BulkPair> {
    for alt in config.interface_alt_settings() {
        let mut bulk_in = None;
        let mut bulk_out = None;
        for endpoint in alt.endpoints() {
            if endpoint.transfer_type() != TransferType::Bulk {
                continue;
            }
            match endpoint.direction() {
                nusb::transfer::Direction::In => bulk_in.get_or_insert(endpoint.address()),
                nusb::transfer::Direction::Out => bulk_out.get_or_insert(endpoint.address()),
            };
        }
        if let (Some(bulk_in), Some(bulk_out)) = (bulk_in, bulk_out) {
            return Some(BulkPair {
                interface: alt.interface_number(),
                alt_setting: alt.alternate_setting(),
                bulk_in,
                bulk_out,
            });
        }
    }
    None
}

/// Turn an nusb open or claim failure into an error a person can act on.
fn open_error(err: nusb::Error, vendor_id: u16, product_id: u16) -> Error {
    match err.kind() {
        nusb::ErrorKind::PermissionDenied => Error::AccessDenied {
            vendor_id,
            product_id,
        },
        nusb::ErrorKind::Busy => Error::Busy(err.to_string()),
        nusb::ErrorKind::Disconnected => Error::Disconnected,
        _ => Error::Transport(err.to_string()),
    }
}

/// Turn an nusb transfer failure into the crate's [`Error`].
///
/// A stall is a device's refusal of the request. The transfer methods clear the
/// halt before they call this function, so the pipe is usable again. The next
/// command then starts clean, instead of failing for a reason unrelated to it.
///
/// A canceled transfer is a timed-out one. Nothing here cancels a transfer in
/// flight, because the [`Cancel`](crate::progress::Cancel) token is checked
/// between windows, never inside one. Only an expired deadline takes a transfer
/// back. Reporting that as "the caller canceled" would blame the person for a
/// device that stopped answering.
fn transfer_error(err: TransferError, what: &'static str, waited: Duration) -> Error {
    match err {
        TransferError::Disconnected => Error::Disconnected,
        TransferError::Cancelled => Error::Timeout { what, waited },
        TransferError::Stall => Error::Protocol(format!(
            "the device stalled {what}: it refused the command outright"
        )),
        other => Error::Transport(format!("{what}: {other}")),
    }
}

impl Transport for UsbTransport {
    async fn control(&mut self, req: Control<'_>) -> Result<Vec<u8>> {
        match req {
            Control::In {
                request_type,
                request,
                value,
                index,
                length,
            } => self
                .interface
                .control_in(
                    ControlIn {
                        control_type: control_type(request_type)?,
                        recipient: recipient(request_type)?,
                        request,
                        value,
                        index,
                        length,
                    },
                    CONTROL_TIMEOUT,
                )
                .wait()
                .map_err(|e| transfer_error(e, "control IN", CONTROL_TIMEOUT)),
            Control::Out {
                request_type,
                request,
                value,
                index,
                data,
            } => {
                self.interface
                    .control_out(
                        ControlOut {
                            control_type: control_type(request_type)?,
                            recipient: recipient(request_type)?,
                            request,
                            value,
                            index,
                            data,
                        },
                        CONTROL_TIMEOUT,
                    )
                    .wait()
                    .map_err(|e| transfer_error(e, "control OUT", CONTROL_TIMEOUT))?;
                Ok(Vec::new())
            }
        }
    }

    async fn write_bulk(&mut self, data: &[u8]) -> Result<()> {
        let bulk = self.bulk.as_mut().ok_or_else(no_bulk_pipe)?;

        // Submits, waits out the deadline, and takes the transfer back if the
        // deadline passes -- so a device that never answers leaves nothing
        // pending behind it, and a timed-out transfer cannot land in the lap of
        // the command that follows.
        let completion = bulk
            .bulk_out
            .transfer_blocking(Buffer::from(data), BULK_TIMEOUT);

        if let Err(err) = completion.status {
            if err == TransferError::Stall {
                let _ = bulk.bulk_out.clear_halt().wait();
            }
            return Err(transfer_error(err, "bulk OUT", BULK_TIMEOUT));
        }

        if completion.actual_len != data.len() {
            return Err(Error::Transport(format!(
                "bulk OUT moved {} of {} bytes",
                completion.actual_len,
                data.len()
            )));
        }
        Ok(())
    }

    async fn read_bulk(&mut self, len: usize) -> Result<Vec<u8>> {
        if len == 0 {
            return Ok(Vec::new());
        }

        let bulk = self.bulk.as_mut().ok_or_else(no_bulk_pipe)?;

        // nusb requires an IN transfer's requested length to be a nonzero
        // multiple of the endpoint's maximum packet size. A CSW is 13 bytes, so
        // asking for exactly what we want would be rejected outright. Round the
        // request up instead: the device ends the transfer with a short packet,
        // and we keep only what arrived.
        let packet = bulk.bulk_in.max_packet_size();
        let requested = len.div_ceil(packet) * packet;

        let completion = bulk
            .bulk_in
            .transfer_blocking(Buffer::new(requested), BULK_TIMEOUT);

        if let Err(err) = completion.status {
            if err == TransferError::Stall {
                let _ = bulk.bulk_in.clear_halt().wait();
            }
            return Err(transfer_error(err, "bulk IN", BULK_TIMEOUT));
        }

        // Rounding the request up is what lets a device put more on the wire than
        // was asked for. The surplus is evidence that the two ends have fallen
        // out of step, so it is reported rather than trimmed away: trimming would
        // leave those bytes to be read as the next reply, and the confusion would
        // surface later, blamed on a command that did nothing wrong. It is the
        // stance `parse_csw` already takes.
        if completion.actual_len > len {
            return Err(Error::Protocol(format!(
                "bulk IN answered a {len}-byte request with {} bytes. The device and the host \
                 are no longer synchronized",
                completion.actual_len
            )));
        }

        // For an IN transfer nusb sets the buffer's length to what arrived, so a
        // short packet -- the way a device ends a reply early, which is normal --
        // lands here as a short vector.
        Ok(completion.buffer.into_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_type_decodes_into_a_type_and_a_recipient() {
        // 0x40: host-to-device, vendor, device. Ingenic's VR_* requests use it.
        assert_eq!(control_type(0x40).unwrap(), ControlType::Vendor);
        assert_eq!(recipient(0x40).unwrap(), Recipient::Device);

        // 0xc0: device-to-host, vendor, device.
        assert_eq!(control_type(0xc0).unwrap(), ControlType::Vendor);
        assert_eq!(recipient(0xc0).unwrap(), Recipient::Device);

        // 0x81: device-to-host, standard, interface.
        assert_eq!(control_type(0x81).unwrap(), ControlType::Standard);
        assert_eq!(recipient(0x81).unwrap(), Recipient::Interface);

        // 0x22: host-to-device, class, endpoint.
        assert_eq!(control_type(0x22).unwrap(), ControlType::Class);
        assert_eq!(recipient(0x22).unwrap(), Recipient::Endpoint);
    }

    /// Nothing in the codebase takes back a transfer in flight, because the
    /// cancellation token is checked between windows, never inside one. Only an
    /// expired deadline cancels a transfer. Reporting that as `Canceled` tells a
    /// person they asked for a device to stop answering. It hides the fact they
    /// can act on, which is that the device stopped answering.
    #[test]
    fn a_canceled_transfer_is_a_timed_out_one_because_nothing_else_cancels_one() {
        let error = transfer_error(TransferError::Cancelled, "bulk IN", BULK_TIMEOUT);
        assert!(
            matches!(
                error,
                Error::Timeout {
                    what: "bulk IN",
                    waited: BULK_TIMEOUT
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn a_reserved_request_type_is_refused_rather_than_coerced() {
        // Type bits 0b11 and recipients above 3 are reserved: nusb has no
        // encoding for them, and picking a neighboring one would put a
        // different request on the wire than the caller asked for.
        assert!(matches!(control_type(0x60), Err(Error::Protocol(_))));
        assert!(matches!(recipient(0x04), Err(Error::Protocol(_))));
    }

    /// The open paths take nusb's blocking syscalls with `wait`, because there
    /// is no async runtime to await them on. Awaited bare, nusb panics with
    /// advice to enable one. Opening a device that cannot exist runs each path
    /// through `list_devices` as far as the find. That is far enough to catch an
    /// `.await` reintroduced in front of it. The test runs on any machine, with no
    /// board attached, under the same `pollster` the CLI and GUI drive verbs with.
    #[test]
    fn open_paths_need_no_async_runtime() {
        let ghost = DeviceInfo {
            vendor: crate::discovery::Vendor::Rockchip,
            vendor_id: 0x2207,
            product_id: 0x350e,
            bcd_usb: 0x0200,
            mode: crate::discovery::Mode::Maskrom,
            bus_id: "no-such-bus".to_string(),
            device_address: 0,
        };

        let opened = pollster::block_on(UsbTransport::open(&ghost));
        assert!(matches!(opened, Err(Error::DeviceNotFound)));

        let opened = pollster::block_on(UsbTransport::open_maskrom(&ghost));
        assert!(matches!(opened, Err(Error::DeviceNotFound)));
    }

    /// An endpoint descriptor, laid out as the USB specification does.
    /// `attributes` carries the transfer type in its low two bits: `0x02` is
    /// bulk, `0x03` interrupt. The high bit of `address` is the direction.
    const fn endpoint(address: u8, attributes: u8) -> [u8; 7] {
        [
            7,          // bLength
            0x05,       // bDescriptorType: ENDPOINT
            address,    // bEndpointAddress
            attributes, // bmAttributes
            0x00, 0x02, // wMaxPacketSize: 512, little-endian
            0,    // bInterval
        ]
    }

    /// An interface descriptor, with its endpoint descriptors after it.
    fn interface(number: u8, alt_setting: u8, endpoints: &[[u8; 7]]) -> Vec<u8> {
        let mut bytes = vec![
            9,                     // bLength
            0x04,                  // bDescriptorType: INTERFACE
            number,                // bInterfaceNumber
            alt_setting,           // bAlternateSetting
            endpoints.len() as u8, // bNumEndpoints
            0xff,                  // bInterfaceClass: vendor specific
            0,                     // bInterfaceSubClass
            0,                     // bInterfaceProtocol
            0,                     // iInterface
        ];
        for endpoint in endpoints {
            bytes.extend_from_slice(endpoint);
        }
        bytes
    }

    /// A configuration descriptor wrapping `interfaces`. The tests feed it to
    /// nusb's own parser, so they exercise the real descriptor path with no device.
    fn config(interfaces: &[Vec<u8>]) -> Vec<u8> {
        let body = interfaces.concat();
        let total = (9 + body.len()) as u16;
        let mut bytes = vec![9, 0x02];
        bytes.extend_from_slice(&total.to_le_bytes()); // wTotalLength
        bytes.extend_from_slice(&[
            1,    // bNumInterfaces
            1,    // bConfigurationValue
            0,    // iConfiguration
            0x80, // bmAttributes: bus powered
            0x32, // bMaxPower
        ]);
        bytes.extend(body);
        bytes
    }

    /// The Rockchip shape: one interface, the pair on the default setting.
    #[test]
    fn a_pair_on_the_default_alternate_setting_is_found_there() {
        let bytes = config(&[interface(
            0,
            0,
            &[endpoint(0x81, 0x02), endpoint(0x02, 0x02)],
        )]);
        let descriptor =
            nusb::descriptors::ConfigurationDescriptor::new(&bytes).expect("a valid descriptor");

        assert_eq!(
            bulk_interface(&descriptor),
            Some(BulkPair {
                interface: 0,
                alt_setting: 0,
                bulk_in: 0x81,
                bulk_out: 0x02,
            })
        );
    }

    /// A pair declared on a non-zero alternate setting comes back with the
    /// setting it was found on. Claiming the interface leaves it on setting 0, so
    /// the addresses alone are not enough. Without the setting, the caller would
    /// open endpoints against a setting that does not declare them.
    #[test]
    fn a_pair_on_another_alternate_setting_comes_back_with_that_setting() {
        let bytes = config(&[
            interface(0, 0, &[]),
            interface(0, 1, &[endpoint(0x81, 0x02), endpoint(0x02, 0x02)]),
        ]);
        let descriptor =
            nusb::descriptors::ConfigurationDescriptor::new(&bytes).expect("a valid descriptor");

        assert_eq!(
            bulk_interface(&descriptor),
            Some(BulkPair {
                interface: 0,
                alt_setting: 1,
                bulk_in: 0x81,
                bulk_out: 0x02,
            })
        );
    }

    #[test]
    fn a_device_with_no_bulk_pair_is_not_speaking_a_protocol_we_know() {
        // A lone interrupt IN endpoint: not a bulk pair, and not half of one.
        let bytes = config(&[interface(0, 0, &[endpoint(0x81, 0x03)])]);
        let descriptor =
            nusb::descriptors::ConfigurationDescriptor::new(&bytes).expect("a valid descriptor");

        assert_eq!(bulk_interface(&descriptor), None);
    }
}
