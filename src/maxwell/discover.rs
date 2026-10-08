//! Finding the dongle in sysfs, and the wired headset next to it - which is
//! what charging looks like; the [module documentation](super) says why.

use std::fs;
use std::path::{Path, PathBuf};

use hidraw::{Bus, Filter};
use log::debug;

use super::{PRODUCT_IDS, VENDOR_ID, supports};

/// What the dongle's product string is reported as when the kernel has none
/// for it (`HID_NAME` missing from the uevent, which usbhid never leaves out).
const DEFAULT_PRODUCT: &str = "Audeze Maxwell";

/// Whether a Maxwell headset is plugged into this computer with a cable, which
/// is the only evidence of charging there is (see the module documentation).
///
/// Any Audeze device that is not one of the dongles counts: the Xbox headset
/// is `4b1e`, and the other variants have IDs of their own that are not worth
/// guessing at.
pub(super) fn headset_is_wired(usb_devices: &Path) -> bool {
    let Ok(entries) = fs::read_dir(usb_devices) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|entry| {
        let id = |name: &str| {
            let raw = fs::read_to_string(entry.path().join(name)).ok()?;
            u16::from_str_radix(raw.trim(), 16).ok()
        };
        id("idVendor") == Some(VENDOR_ID)
            && id("idProduct").is_some_and(|product| !PRODUCT_IDS.contains(&product))
    })
}

/// A Maxwell dongle found in sysfs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Dongle {
    pub(super) node: PathBuf,
    /// The kernel's name for the device (`HID_NAME`, the USB manufacturer and
    /// product strings), reported as [`Headset::product`].
    pub(super) product: String,
    pub(super) product_id: u16,
}

impl Dongle {
    /// The dongle behind a hidraw node, if the node is one. [`discover`](super::discover()) has
    /// already kept Audeze's devices on USB; the product ID is what is left to
    /// check.
    fn from_node(node: hidraw::Node) -> Option<Self> {
        supports(node.vendor, node.product).then(|| Self {
            node: node.path,
            product: if node.name.is_empty() {
                DEFAULT_PRODUCT.to_owned()
            } else {
                node.name
            },
            product_id: node.product,
        })
    }
}

/// As [`discover`](super::discover()), with the hidraw class directory and the device directory
/// given, so that a fake tree can stand in for sysfs under test.
///
/// A sysfs that cannot be listed is reported as no dongle at all, as a reader
/// that runs every second must not fail over it; the bridge says so when it
/// lasts.
pub(super) fn discover_in(sysfs_root: &Path, dev_root: &Path) -> Vec<Dongle> {
    let filter = Filter::new().bus(Bus::Usb).vendor(VENDOR_ID);
    match hidraw::discover_in(sysfs_root, dev_root, &filter) {
        Ok(nodes) => nodes.into_iter().filter_map(Dongle::from_node).collect(),
        Err(err) => {
            debug!("cannot list {}: {err}", sysfs_root.display());
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn our_own_virtual_battery_is_not_mistaken_for_a_dongle() {
        // The identity is parsed by hidraw, which has its own tests; what is
        // checked here is this module's filter: the USB bus, Audeze's vendor
        // ID, a dongle's product ID.
        let class_dir = tempfile::tempdir().unwrap();
        let dir = class_dir.path();
        for (node, uevent) in [
            // The real dongle, on USB.
            (
                "hidraw10",
                "HID_ID=0003:00003329:00004B18\nHID_NAME=Audeze Dongle\n",
            ),
            // The virtual battery: same IDs, BUS_VIRTUAL.
            (
                "hidraw17",
                "HID_ID=0006:00003329:00004B18\nHID_NAME=Audeze Maxwell\n",
            ),
            // Another Audeze device on USB - the headset on a cable, should
            // it expose a HID interface - is not a dongle.
            ("hidraw18", "HID_ID=0003:00003329:00004B1E\n"),
            // Somebody else's device.
            (
                "hidraw2",
                "HID_ID=0003:00001532:000000A4\nHID_NAME=Razer Dock\n",
            ),
            // A dongle whose uevent has no name.
            ("hidraw3", "HID_ID=0003:00003329:00004B19\n"),
        ] {
            fs::create_dir_all(dir.join(node).join("device")).unwrap();
            fs::write(dir.join(node).join("device/uevent"), uevent).unwrap();
        }

        let found = discover_in(dir, Path::new("/dev"));
        assert_eq!(
            found,
            [
                Dongle {
                    node: PathBuf::from("/dev/hidraw3"),
                    product: DEFAULT_PRODUCT.to_owned(),
                    product_id: 0x4b19,
                },
                Dongle {
                    node: PathBuf::from("/dev/hidraw10"),
                    product: "Audeze Dongle".to_owned(),
                    product_id: 0x4b18,
                },
            ]
        );
    }

    #[test]
    fn a_headset_on_a_cable_is_what_charging_looks_like() {
        let usb_devices = tempfile::tempdir().unwrap();
        let dir = usb_devices.path();
        let plug = |name: &str, vendor: &str, product: &str| {
            fs::create_dir_all(dir.join(name)).unwrap();
            fs::write(dir.join(name).join("idVendor"), format!("{vendor}\n")).unwrap();
            fs::write(dir.join(name).join("idProduct"), format!("{product}\n")).unwrap();
        };

        // The dongle alone, and somebody else's device: nothing is charging.
        plug("1-5", "3329", "4b18");
        plug("1-3", "1532", "00a4");
        // Interfaces and hubs have no idVendor file at all.
        fs::create_dir_all(dir.join("1-5:1.0")).unwrap();
        assert!(!headset_is_wired(dir));

        // The headset itself shows up once the cable is in.
        plug("5-2", "3329", "4b1e");
        assert!(headset_is_wired(dir));

        fs::remove_dir_all(dir.join("5-2")).unwrap();
        assert!(!headset_is_wired(dir));
        assert!(!headset_is_wired(Path::new("/nonexistent/usb")));
    }

    #[test]
    fn discovery_survives_a_missing_sysfs() {
        assert_eq!(
            discover_in(Path::new("/nonexistent/hidraw"), Path::new("/dev")),
            [] as [Dongle; 0]
        );
    }
}
