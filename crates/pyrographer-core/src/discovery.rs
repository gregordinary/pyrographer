//! Device discovery: enumerate USB devices and classify their boot mode.
//!
//! Everything here reads the descriptors the operating system cached as the device
//! enumerated. Listing and classifying a board therefore needs no permission to
//! open it. A missing udev rule is reported later, by the first verb that opens
//! the device.
//!
//! [`list_rockchip`] is native-only. A browser cannot scan the bus, so the seam
//! itself differs, not only its implementation. A page receives a device from a
//! chooser that only a user gesture can open. It can also ask for the devices this
//! origin has already been granted (`transport::list_permitted`). That list holds
//! the devices a person has granted, not the devices that are plugged in.
//!
//! A page has only those two routes to a device. Either way, the device arrives as
//! a `web_sys::UsbDevice`, and `describe_webusb` builds a [`DeviceInfo`] for it. The
//! same [`Mode`] rule classifies it, and that rule is the part both builds share.

use crate::{Error, Result};
#[cfg(not(target_arch = "wasm32"))]
use nusb::MaybeFuture;

/// Rockchip's USB vendor ID.
///
/// Discovery scans by vendor ID, and never filters on product ID. The product ID
/// identifies the SoC family, not the mode. A table of product IDs would need an
/// entry for every new part. The vendor ID and the [`Mode`] convention need none.
pub const ROCKCHIP_VID: u16 = 0x2207;

/// Ingenic's USB vendor ID, used from the JZ4770 onward, covering every X-series
/// and T-series camera SoC pyrographer targets.
///
/// Ingenic encodes the mode in the product ID, where Rockchip encodes it in
/// `bcdUSB`. [`classify_ingenic`] reads it.
pub const INGENIC_VID: u16 = 0xa108;

/// Ingenic's legacy USB vendor ID (JZ4740/4750/4760).
///
/// None of pyrographer's designed-for parts use it. Discovery still scans it, so a
/// legacy board is listed and named rather than silently absent. The same
/// [`classify_ingenic`] rule classifies it.
pub const INGENIC_VID_LEGACY: u16 = 0x601a;

/// The product ID Ingenic's post-bootstrap DFU gadget always enumerates at.
///
/// The Ingenic mode convention rests on this one PID. The DFU gadget enumerates at
/// `0x4d44` on every SoC, and no boot ROM PID is `0x4d44`. The rule "this PID means
/// DFU, and any other PID on the vendor's VID means boot ROM" therefore needs no
/// per-SoC table. [`classify_ingenic`] applies it.
pub const INGENIC_DFU_PID: u16 = 0x4d44;

/// The USB Mass Storage interface class.
///
/// It is public because classifying a device requires knowing whether the device
/// declares this class. On the web, the interface list comes from WebUSB, not from
/// this module.
pub const CLASS_MASS_STORAGE: u8 = 0x08;

/// Which vendor's backend owns a detected device.
///
/// [`DeviceInfo`] carries it because a device's [`Mode`] and product ID are read
/// from the same VID-filtered scan. The verbs need to know which bootstrap and
/// which [`FlashAgent`](crate::agent) a device needs before they open it.
/// Discovery scans one vendor at a time, so this is set by construction, not
/// guessed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    /// Rockchip (maskrom / rockusb loader over USB bulk).
    Rockchip,
    /// Ingenic (XBurst USB boot ROM, then DFU after bootstrap).
    Ingenic,
}

impl Vendor {
    /// The vendor's name, for a message a person has to read.
    pub fn name(self) -> &'static str {
        match self {
            Vendor::Rockchip => "Rockchip",
            Vendor::Ingenic => "Ingenic",
        }
    }

    /// Which backend owns a vendor ID, or `None` for one no backend claims.
    ///
    /// The web build depends on this function, and the native scan does not use
    /// it. Natively, discovery runs one VID-filtered pass per vendor, so a device's
    /// vendor is the vendor of the pass that found it.
    ///
    /// A browser returns one device, from a chooser opened with every vendor's
    /// filter at once. Only the vendor ID on the device says which vendor it
    /// belongs to. This function therefore does on the web what a separate pass
    /// does natively.
    ///
    /// `None` is a device on neither vendor's ID. The chooser filters by vendor, so
    /// such a device is not expected. `describe_webusb` reports it as an error
    /// rather than defaulting to a vendor. A default of Rockchip would send rockusb
    /// commands to a device that does not implement them.
    pub fn from_vid(vendor_id: u16) -> Option<Self> {
        match vendor_id {
            ROCKCHIP_VID => Some(Vendor::Rockchip),
            INGENIC_VID | INGENIC_VID_LEGACY => Some(Vendor::Ingenic),
            _ => None,
        }
    }
}

/// Every vendor ID pyrographer's USB backends answer for.
///
/// It is the web chooser's complete filter list. A page cannot scan a bus. Where
/// the native build makes one VID-filtered pass per vendor, the browser opens one
/// chooser carrying every filter. This constant holds that set in one place, so the
/// chooser and [`Vendor::from_vid`] agree about which devices are pyrographer's.
pub const USB_VENDOR_IDS: [u16; 3] = [ROCKCHIP_VID, INGENIC_VID, INGENIC_VID_LEGACY];

/// The boot or recovery mode a detected device is in.
///
/// Each vendor has its own variants. `Maskrom` and `Loader` are Rockchip states,
/// and `BootRom` and `Dfu` are Ingenic ones. The descriptors give no basis for
/// naming one vendor's states with another vendor's names.
/// [`MassStorage`](Mode::MassStorage) is the one mode any vendor's device can
/// enter, and it means the same thing for all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Rockchip maskrom: the BootROM is running. DRAM is uninitialized and
    /// flash is unreachable until a loader is uploaded into SRAM.
    Maskrom,
    /// Rockchip loader: a loader is running and flash is reachable.
    Loader,
    /// Ingenic USB boot ROM: the on-chip ROM is running the `VR_*` vendor
    /// protocol. It has no flash primitive. A DRAM-init stage and a DFU-capable
    /// U-Boot must be uploaded before flash is reachable.
    BootRom,
    /// Ingenic DFU gadget: a DFU-capable U-Boot is running and flash is
    /// reachable as named DFU alt-settings.
    Dfu,
    /// The device is presenting a USB Mass Storage interface rather than a boot
    /// protocol. Its flash is reachable through the operating system's own block
    /// layer. On Linux, the [`block`](crate::block) backend opens it as a disk.
    MassStorage,
}

impl Mode {
    /// The mode's name, for a message a person has to read.
    pub fn name(self) -> &'static str {
        match self {
            Mode::Maskrom => "maskrom",
            Mode::Loader => "loader",
            Mode::BootRom => "boot ROM",
            Mode::Dfu => "DFU",
            Mode::MassStorage => "mass storage",
        }
    }
}

/// A USB device detected in a boot or recovery state.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    /// Which vendor's backend owns this device. Natively, it is set from the
    /// VID-filtered scan that found the device. In a browser,
    /// [`Vendor::from_vid`] reads it from the device's vendor ID.
    pub vendor: Vendor,
    /// USB vendor ID.
    pub vendor_id: u16,
    /// USB product ID. It identifies the SoC family, and [`ROCKCHIP_VID`] explains
    /// why discovery does not match on it.
    pub product_id: u16,
    /// The `bcdUSB` field, whose low bit carries the mode. See [`Mode`].
    pub bcd_usb: u16,
    /// The mode the device is in.
    pub mode: Mode,
    /// The bus the device is on. Together with
    /// [`device_address`](Self::device_address), it identifies the device well
    /// enough to open it.
    ///
    /// **Empty in a browser**, which has no bus to name. There, the `device` field
    /// identifies a board. It is web only, so a native rustdoc does not show it.
    pub bus_id: String,
    /// The device's address on its bus.
    ///
    /// **Zero in a browser**, for the same reason [`bus_id`](Self::bus_id) is
    /// empty.
    pub device_address: u8,
    /// The device object the browser handed over. **Web only.**
    ///
    /// Natively, a board is a place: [`bus_id`](Self::bus_id) and
    /// [`device_address`](Self::device_address). Two listings that name the same
    /// place name the same board. A browser gives no place. It returns an object,
    /// and that object is the permission to talk to the board. Two listings that
    /// hold the same object name the same board. The identity therefore travels
    /// with the description, and a caller compares whole devices, not coordinates.
    ///
    /// The description carries it because a listing must be openable.
    /// `transport::list_permitted` returns devices, and a row a person clicks must
    /// reach the device it names.
    #[cfg(target_arch = "wasm32")]
    pub device: web_sys::UsbDevice,
}

/// List connected Rockchip devices in a boot or recovery state.
///
/// Native only. It blocks while the operating system lists its devices. A browser
/// has no such list and cannot block, as the module documentation explains.
#[cfg(not(target_arch = "wasm32"))]
pub fn list_rockchip() -> Result<Vec<DeviceInfo>> {
    let devices = nusb::list_devices()
        .wait()
        .map_err(|e| Error::Transport(format!("cannot list USB devices: {e}")))?;

    Ok(devices
        .filter(|dev| dev.vendor_id() == ROCKCHIP_VID)
        .map(|dev| {
            let mass_storage = dev
                .interfaces()
                .any(|interface| interface.class() == CLASS_MASS_STORAGE);
            DeviceInfo {
                vendor: Vendor::Rockchip,
                vendor_id: dev.vendor_id(),
                product_id: dev.product_id(),
                bcd_usb: dev.usb_version(),
                mode: classify_rockchip(dev.usb_version(), mass_storage),
                bus_id: dev.bus_id().to_string(),
                device_address: dev.device_address(),
            }
        })
        .collect())
}

/// List connected Ingenic devices in a boot or DFU state.
///
/// Native only, for the same reason [`list_rockchip`] is: a browser cannot scan
/// the bus. It scans both the current [`INGENIC_VID`] and the legacy
/// [`INGENIC_VID_LEGACY`], so an older part is listed and named. It classifies
/// each device with [`classify_ingenic`].
#[cfg(not(target_arch = "wasm32"))]
pub fn list_ingenic() -> Result<Vec<DeviceInfo>> {
    let devices = nusb::list_devices()
        .wait()
        .map_err(|e| Error::Transport(format!("cannot list USB devices: {e}")))?;

    Ok(devices
        .filter(|dev| dev.vendor_id() == INGENIC_VID || dev.vendor_id() == INGENIC_VID_LEGACY)
        .map(|dev| {
            let mass_storage = dev
                .interfaces()
                .any(|interface| interface.class() == CLASS_MASS_STORAGE);
            DeviceInfo {
                vendor: Vendor::Ingenic,
                vendor_id: dev.vendor_id(),
                product_id: dev.product_id(),
                bcd_usb: dev.usb_version(),
                mode: classify_ingenic(dev.product_id(), mass_storage),
                bus_id: dev.bus_id().to_string(),
                device_address: dev.device_address(),
            }
        })
        .collect())
}

/// Classify a Rockchip device from its descriptors.
///
/// Rockchip encodes the mode in the low bit of `bcdUSB`. Odd advertises loader,
/// and even advertises maskrom. Treat the even case as a **claim, not a finding**.
///
/// The RK3576 SPL loader runs rockusb behind an even flag, verified on hardware.
/// In the descriptors, that loader is therefore indistinguishable from the maskrom
/// it replaced. [`Mode::Maskrom`] is a mode to probe rather than trust. A verb
/// settles an even flag by opening the device and sending a `TEST_UNIT_READY`, then
/// the probe command. Only a device that faults every command is a real BootROM.
///
/// An odd flag is trusted as loader, because no device has been seen to run maskrom
/// behind one. A decoded mode is still a starting point for the verbs, not a final
/// answer.
///
/// A device that re-enumerated as mass storage is neither. Its low bit would
/// classify it as one of two modes it is not in, so the interface class is checked
/// first.
///
/// The scan is native-only, but this rule is not. The web build classifies its
/// picked device with it, from the descriptors WebUSB reports. The rule therefore
/// takes two plain arguments, which either build can supply.
pub fn classify_rockchip(bcd_usb: u16, mass_storage: bool) -> Mode {
    if mass_storage {
        Mode::MassStorage
    } else if bcd_usb & 1 == 0 {
        Mode::Maskrom
    } else {
        Mode::Loader
    }
}

/// Classify an Ingenic device from its descriptors.
///
/// Ingenic encodes the mode in the product ID, not `bcdUSB`, which is the reverse
/// of [`classify_rockchip`]. The post-bootstrap DFU gadget always enumerates at
/// [`INGENIC_DFU_PID`] (`0x4d44`), whatever the SoC. The boot ROM's PID varies by
/// family (`c309` on T-series, `1000` on X1000, ...), but is never `0x4d44`.
///
/// The rule is therefore one known PID for DFU, and "any other PID on the vendor's
/// VID is the boot ROM". It needs no per-SoC PID table. The Rockchip scan also
/// avoids a product-ID table. There the product ID is ignored, and here one fixed
/// PID decides the mode.
///
/// Mass storage is checked first, for the same reason it is in
/// [`classify_rockchip`]. An Ingenic board running a Linux mass-storage gadget
/// belongs to the operating system's block layer, not to a boot protocol. Its PID
/// would otherwise read as a boot ROM.
///
/// Like the Rockchip rule, this takes plain arguments rather than a device. The web
/// build therefore classifies a picked device from WebUSB's descriptors with the
/// same logic.
pub fn classify_ingenic(product_id: u16, mass_storage: bool) -> Mode {
    if mass_storage {
        Mode::MassStorage
    } else if product_id == INGENIC_DFU_PID {
        Mode::Dfu
    } else {
        Mode::BootRom
    }
}

/// Reassemble `bcdUSB` from the three numbers a browser splits it into.
///
/// WebUSB reports `usbVersionMajor`, `usbVersionMinor` and `usbVersionSubminor`
/// rather than the raw field. The Rockchip mode convention is carried in that
/// field's low bit, as [`classify_rockchip`] describes. On the web, the mode of
/// every Rockchip board therefore depends on this function. An error here would
/// misclassify every board in the same direction.
///
/// The field is binary-coded decimal in the usual USB way, one nibble per number.
/// `2.0.1` is therefore `0x0201`, which is odd, and so a loader.
///
/// It is not `cfg`-gated, and it is tested on the host, because checking
/// arithmetic needs no browser.
pub fn bcd_usb(major: u8, minor: u8, subminor: u8) -> u16 {
    (u16::from(major) << 8) | (u16::from(minor) << 4) | u16::from(subminor)
}

/// Describe a device the browser returned.
///
/// It is the web implementation of the discovery seam. A page cannot scan a bus,
/// so no scan precedes this call. A page receives one device, from a chooser only a
/// user gesture can open. This function describes that single device and lists
/// nothing else. It produces the same [`DeviceInfo`] the native scan produces,
/// classified by the [`classify_rockchip`] and [`classify_ingenic`] rules both
/// builds share.
///
/// [`Vendor::from_vid`] reads the vendor, and so the rule to apply, from the
/// device. No per-vendor pass has established it, because one chooser carries every
/// vendor's filter and returns one device. A vendor ID no backend claims is an
/// [`Error`] naming it, not a default, so the function returns a [`Result`]. The
/// two vendors encode the mode in different fields. A misread vendor would give a
/// misread mode, and then a rockusb command sent to an Ingenic boot ROM.
///
/// Two fields are left empty rather than invented. WebUSB reports no bus and no
/// address, because it returns a device object, not a place.
/// [`bus_id`](DeviceInfo::bus_id) is therefore empty, and
/// [`device_address`](DeviceInfo::device_address) is zero. The web build never asks
/// a person to transcribe a coordinate, because there is nothing short and stable
/// to transcribe. Its write confirmation is a re-pick rather than a typed address.
///
/// `bcdUSB` is reconstructed, because the browser splits it into three numbers and
/// the Rockchip mode convention is carried in its low bit. `2.0.1` is `0x0201`,
/// which is odd, and so a loader.
#[cfg(target_arch = "wasm32")]
pub fn describe_webusb(device: &web_sys::UsbDevice) -> Result<DeviceInfo> {
    let vendor = Vendor::from_vid(device.vendor_id()).ok_or_else(|| {
        Error::InvalidRequest(format!(
            "the browser returned a device on vendor ID {:04x}, which is not one pyrographer \
             has a backend for",
            device.vendor_id()
        ))
    })?;

    let bcd_usb = bcd_usb(
        device.usb_version_major(),
        device.usb_version_minor(),
        device.usb_version_subminor(),
    );

    // A device that has not been opened has no configuration to read interfaces
    // from, so the class comes from the device descriptor when that is all there
    // is. Neither is authoritative on its own -- a composite device declares its
    // class per interface -- so both are consulted, and either one saying Mass
    // Storage is enough.
    let mass_storage = device.device_class() == CLASS_MASS_STORAGE
        || device.configuration().is_some_and(|configuration| {
            configuration
                .interfaces()
                .iter()
                .any(|interface| interface.alternate().interface_class() == CLASS_MASS_STORAGE)
        });

    // Each vendor's mode lives in a different field -- Rockchip's in the low bit
    // of `bcdUSB`, Ingenic's in the product ID -- so the vendor decides which
    // rule reads it, and both rules are the ones the native scan runs.
    let mode = match vendor {
        Vendor::Rockchip => classify_rockchip(bcd_usb, mass_storage),
        Vendor::Ingenic => classify_ingenic(device.product_id(), mass_storage),
    };

    Ok(DeviceInfo {
        vendor,
        vendor_id: device.vendor_id(),
        product_id: device.product_id(),
        bcd_usb,
        mode,
        bus_id: String::new(),
        device_address: 0,
        device: device.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_even_bcd_usb_is_maskrom_and_an_odd_one_is_loader() {
        // The values Rockchip parts actually advertise.
        assert_eq!(classify_rockchip(0x0200, false), Mode::Maskrom);
        assert_eq!(classify_rockchip(0x0201, false), Mode::Loader);
        // The rule is the low bit, not the value, so it holds for a part that
        // advertises something else entirely.
        assert_eq!(classify_rockchip(0x0110, false), Mode::Maskrom);
        assert_eq!(classify_rockchip(0x0301, false), Mode::Loader);
    }

    #[test]
    fn a_mass_storage_interface_outranks_the_bcd_usb_rule() {
        assert_eq!(classify_rockchip(0x0201, true), Mode::MassStorage);
        assert_eq!(classify_rockchip(0x0200, true), Mode::MassStorage);
    }

    #[test]
    fn ingenics_dfu_pid_is_dfu_and_every_other_pid_is_the_boot_rom() {
        // The one fixed PID the DFU gadget always enumerates at.
        assert_eq!(classify_ingenic(INGENIC_DFU_PID, false), Mode::Dfu);
        // The boot-ROM PIDs vary by SoC family; none of them is the DFU one, so
        // the rule reports every one of them as the boot ROM without a table.
        assert_eq!(classify_ingenic(0xc309, false), Mode::BootRom); // T-series
        assert_eq!(classify_ingenic(0x1000, false), Mode::BootRom); // X1000
        assert_eq!(classify_ingenic(0xeaef, false), Mode::BootRom); // X2000E
    }

    #[test]
    fn a_mass_storage_interface_outranks_the_ingenic_pid_rule() {
        // Even at the DFU PID, a mass-storage interface means the OS owns the
        // flash, so it is reported first.
        assert_eq!(classify_ingenic(INGENIC_DFU_PID, true), Mode::MassStorage);
        assert_eq!(classify_ingenic(0xc309, true), Mode::MassStorage);
    }

    /// The browser splits `bcdUSB` into three numbers, and the mode convention is
    /// carried in the low bit of the whole field. On the web, whether a board is
    /// running a loader depends on this reassembly alone. If it were wrong, it would
    /// misclassify every board in the same direction. That kind of error looks like
    /// a working program.
    ///
    /// It is checked on the host rather than in a browser, because checking
    /// arithmetic needs no browser.
    #[test]
    fn a_browsers_three_version_numbers_reassemble_into_the_field_the_mode_lives_in() {
        // What a Rockchip loader advertises: 2.0.1, and the low bit is what says
        // so.
        assert_eq!(bcd_usb(2, 0, 1), 0x0201);
        assert_eq!(classify_rockchip(bcd_usb(2, 0, 1), false), Mode::Loader);

        // And a maskrom: 2.0.0.
        assert_eq!(bcd_usb(2, 0, 0), 0x0200);
        assert_eq!(classify_rockchip(bcd_usb(2, 0, 0), false), Mode::Maskrom);

        // Each number lands in its own nibble, so a version that uses all three
        // still comes back whole.
        assert_eq!(bcd_usb(1, 1, 0), 0x0110);
        assert_eq!(bcd_usb(3, 2, 1), 0x0321);
    }

    /// The web build has no per-vendor pass to say which backend owns the device
    /// the chooser returned. The vendor ID alone says. It is checked on the host,
    /// because checking a lookup needs no browser.
    #[test]
    fn a_vendor_id_names_its_backend_and_an_unclaimed_one_names_none() {
        assert_eq!(Vendor::from_vid(ROCKCHIP_VID), Some(Vendor::Rockchip));
        assert_eq!(Vendor::from_vid(INGENIC_VID), Some(Vendor::Ingenic));
        // The legacy VID is a different number for the same backend: a JZ4740 is
        // seen and named rather than silently absent.
        assert_eq!(Vendor::from_vid(INGENIC_VID_LEGACY), Some(Vendor::Ingenic));

        // Somebody else's device is nobody's, and saying so is what keeps a
        // rockusb command off it.
        assert_eq!(Vendor::from_vid(0x0403), None);
        assert_eq!(Vendor::from_vid(0x0000), None);
    }

    /// The chooser's filters and the lookup that reads a picked device must agree
    /// about which devices are pyrographer's. Every ID in the filter list must
    /// resolve to a backend, and every vendor must appear in the list.
    #[test]
    fn every_id_the_web_chooser_filters_on_has_a_backend() {
        for vid in USB_VENDOR_IDS {
            assert!(
                Vendor::from_vid(vid).is_some(),
                "the chooser offers {vid:04x}, which no backend claims"
            );
        }
        assert!(USB_VENDOR_IDS.contains(&ROCKCHIP_VID));
        assert!(USB_VENDOR_IDS.contains(&INGENIC_VID));
        assert!(USB_VENDOR_IDS.contains(&INGENIC_VID_LEGACY));
    }
}
