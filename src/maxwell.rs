//! A native battery reader for the Audeze Maxwell, talking to its dongle
//! directly over hidraw.
//!
//! HeadsetControl supports the Maxwell, but reads its battery unreliably: it
//! replays a twenty-packet sequence and expects the battery answer to sit in
//! the buffer of a *different* request, one frame later. When the dongle is a
//! few milliseconds late the answer is missed and the headset is reported as
//! unavailable while music is playing on it. It also scans for `d6 0c 00 00`
//! anywhere in the frame, which matches the dongle's acknowledgement as well as
//! its answer - hence the stray `0%` and `44%` readings (`0x2c`, 44, is all
//! over those frames).
//!
//! The dongle's input report is in fact a stream of small messages:
//!
//! ```text
//! 05 <type> <len> 00 <payload; len bytes>
//!
//! 05 5b 03 00  d6 0c 00          acknowledgement of request d6 0c
//! 05 5d 05 00  d6 0c 00 00 5b    answer to request d6 0c: 0x5b = 91 %
//! ```
//!
//! The report is a buffer the dongle fills from the start, and its second byte
//! counts the bytes written since the report was last fetched; whatever lies
//! beyond that is left over from earlier exchanges. Ignoring the count means
//! reading an old answer back - for ever, once the headset is switched off.
//!
//! ```text
//! 07 10 80 | 05 5b 03 00 d6 0c 00 | 05 5d 05 00 d6 0c 00 00 5c | <stale bytes>
//!    ^^ 16 fresh bytes: the acknowledgement (7) and the answer (9)
//! ```
//!
//! # Charging
//!
//! Nothing the dongle answers changes when the headset is charging: every
//! register it exposes to a read (`01 09 00`-`3f`, `83 2c 00`-`0f`, `d6 0c`,
//! `07 1c`) was compared plugged and unplugged, and only the level moved.
//!
//! What does change is the USB bus. Plugged into the computer, the headset
//! enumerates as a device of its own (`3329:4b1e`, "Audeze Maxwell XBOX
//! Headset") next to the dongle, and disappears when the cable is pulled. Its
//! presence is therefore what this module reports as charging. The limit is
//! obvious: a headset charging from a wall adapter is invisible, and keeps
//! reading as discharging.
//!
//! So this module sends the one request that matters, polls the input report
//! until the *answer* message shows up, and range-checks the level. Measured on
//! a Maxwell Xbox dongle: every read succeeds, in about 70 ms instead of 2.7 s.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread::sleep;
use std::time::Duration;

use log::{debug, info, trace, warn};

use crate::headset::{BatteryState, Headset};

/// Audeze's USB vendor ID.
pub const VENDOR_ID: u16 = 0x3329;

/// Product IDs this module knows how to talk to.
pub const PRODUCT_IDS: [u16; 2] = [
    0x4b19, // Maxwell dongle
    0x4b18, // Maxwell Xbox dongle
];

/// Where the kernel lists USB devices.
const USB_DEVICES: &str = "/sys/bus/usb/devices";

/// `BUS_USB`. The virtual battery this daemon creates carries the dongle's
/// vendor and product IDs too, on `BUS_VIRTUAL`, and has a hidraw node of its
/// own: without this check the reader would mistake it for a second dongle and
/// start sending it battery requests.
const BUS_USB: u16 = 0x0003;

/// Every report the dongle exchanges is this long, report ID included.
const MSG_SIZE: usize = 62;

/// Report ID of the dongle's answers.
const REPLY_REPORT_ID: u8 = 0x07;

/// "What is the battery level?", on output report `0x06`.
const BATTERY_REQUEST: [u8; 9] = [0x06, 0x07, 0x80, 0x05, 0x5a, 0x03, 0x00, 0xd6, 0x0c];

/// Offset of the count of fresh bytes in an input report.
const FRESH_LEN_OFFSET: usize = 1;

/// Offset of the first message in an input report.
const PAYLOAD_OFFSET: usize = 3;

/// Header of the answer: message type `0x5d`, five payload bytes, echoing the
/// request (`d6 0c`) with a zero status. The level is the byte that follows.
const BATTERY_ANSWER: [u8; 7] = [0x5d, 0x05, 0x00, 0xd6, 0x0c, 0x00, 0x00];

/// The dongle needs a moment between a request and its answer; Audeze's own
/// software paces itself at about this rate.
const READ_DELAY: Duration = Duration::from_millis(60);

/// How many times the input report is polled for the answer. The answer has
/// always been there on the first read in testing; the rest is slack for a
/// dongle that is busy relaying audio.
const READ_ATTEMPTS: u32 = 8;

/// The last access error reported, so a broken setup is announced once rather
/// than on every poll.
static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);

/// Reports a failure to reach a dongle that sysfs says is there.
///
/// This is loud on purpose. A dongle that is listed but cannot be opened looks,
/// from the outside, exactly like a headset that is switched off - and the
/// usual cause is a udev rule that sorts after `73-seat-late.rules`, which
/// leaves the node showing `crw-rw----` while its ACL says `group::---`.
fn report_error(node: &Path, err: &io::Error) {
    let message = format!("{}: {err}", node.display());
    let mut last = LAST_ERROR
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if last.as_deref() == Some(message.as_str()) {
        debug!("still failing: {message}");
        return;
    }

    if err.kind() == io::ErrorKind::PermissionDenied {
        warn!(
            "cannot open {message}. The udev rule granting access must sort before \
             73-seat-late.rules (check with `getfacl {}`: the group entry, not the mask, \
             has to read rw-)",
            node.display()
        );
    } else {
        warn!("cannot query {message}");
    }
    *last = Some(message);
}

fn report_recovery() {
    let mut last = LAST_ERROR
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if last.take().is_some() {
        info!("the dongle is reachable again");
    }
}

/// Whether this module handles the given USB device.
#[must_use]
pub fn supports(vendor_id: u16, product_id: u16) -> bool {
    vendor_id == VENDOR_ID && PRODUCT_IDS.contains(&product_id)
}

/// Whether a Maxwell headset is plugged into this computer with a cable, which
/// is the only evidence of charging there is (see the module documentation).
///
/// Any Audeze device that is not one of the dongles counts: the Xbox headset
/// is `4b1e`, and the other variants have IDs of their own that are not worth
/// guessing at.
fn headset_is_wired(usb_devices: &Path) -> bool {
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

/// Reads the battery of every Maxwell dongle plugged in.
///
/// A dongle that is present but cannot be opened or queried is still listed,
/// with an unavailable battery: being detected is what keeps the desktop entry
/// alive while the headset is parked.
#[must_use]
pub fn probe() -> Vec<Headset> {
    let wired = headset_is_wired(Path::new(USB_DEVICES));
    discover(Path::new("/sys/class/hidraw"))
        .into_iter()
        .map(|dongle| {
            let battery = match read_battery(&dongle.node) {
                Ok(Some(level)) => {
                    report_recovery();
                    if wired {
                        BatteryState::Charging(Some(level))
                    } else {
                        BatteryState::Discharging(level)
                    }
                }
                Ok(None) => {
                    report_recovery();
                    trace!("{}: the dongle did not answer", dongle.node.display());
                    BatteryState::Unavailable
                }
                Err(err) => {
                    report_error(&dongle.node, &err);
                    BatteryState::Unavailable
                }
            };
            Headset {
                name: "Audeze Maxwell".to_owned(),
                product: dongle.product,
                vendor_id: VENDOR_ID,
                product_id: dongle.product_id,
                supports_battery: true,
                battery,
            }
        })
        .collect()
}

/// A Maxwell dongle found in sysfs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Dongle {
    node: PathBuf,
    product: String,
    product_id: u16,
}

/// Lists the hidraw nodes that belong to a supported dongle.
fn discover(class_dir: &Path) -> Vec<Dongle> {
    let Ok(entries) = fs::read_dir(class_dir) else {
        return Vec::new();
    };

    let mut dongles: Vec<Dongle> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let uevent = fs::read_to_string(entry.path().join("device/uevent")).ok()?;
            let (bus, vendor_id, product_id, product) = parse_uevent(&uevent)?;
            (bus == BUS_USB && supports(vendor_id, product_id)).then(|| Dongle {
                node: Path::new("/dev").join(entry.file_name()),
                product,
                product_id,
            })
        })
        .collect();
    dongles.sort_by(|a, b| a.node.cmp(&b.node));
    dongles
}

/// Extracts the bus, the USB IDs and the product name from a HID device's
/// `uevent`.
///
/// The relevant lines look like `HID_ID=0003:00003329:00004B18` and
/// `HID_NAME=Audeze LLC Audeze Maxwell XBOX Dongle`.
fn parse_uevent(uevent: &str) -> Option<(u16, u16, u16, String)> {
    let field = |key: &str| {
        uevent
            .lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
    };

    let mut ids = field("HID_ID")?.split(':');
    let bus = u16::from_str_radix(ids.next()?, 16).ok()?;
    let vendor = u32::from_str_radix(ids.next()?, 16).ok()?;
    let product = u32::from_str_radix(ids.next()?, 16).ok()?;
    Some((
        bus,
        u16::try_from(vendor).ok()?,
        u16::try_from(product).ok()?,
        field("HID_NAME").unwrap_or("Audeze Maxwell").to_owned(),
    ))
}

/// Asks one dongle for the battery level.
///
/// Returns `Ok(None)` when the dongle is reachable but has no answer, which is
/// what happens when the headset is off or its radio is parked.
fn read_battery(node: &Path) -> io::Result<Option<u8>> {
    let mut device = OpenOptions::new().read(true).write(true).open(node)?;

    let mut request = [0u8; MSG_SIZE];
    request[..BATTERY_REQUEST.len()].copy_from_slice(&BATTERY_REQUEST);
    device.write_all(&request)?;

    for attempt in 1..=READ_ATTEMPTS {
        sleep(READ_DELAY);
        let frame = get_input_report(&device)?;
        if let Some(level) = parse_battery(&frame) {
            trace!("battery answer on read {attempt}: {level}%");
            return Ok(Some(level));
        }
    }
    Ok(None)
}

/// The part of an input report written since it was last fetched.
fn fresh(frame: &[u8]) -> &[u8] {
    let len = frame.get(FRESH_LEN_OFFSET).copied().map_or(0, usize::from);
    let end = (PAYLOAD_OFFSET + len).min(frame.len());
    frame.get(PAYLOAD_OFFSET..end).unwrap_or_default()
}

/// Finds the battery answer in the fresh part of an input report and
/// range-checks the level.
fn parse_battery(frame: &[u8]) -> Option<u8> {
    fresh(frame)
        .windows(BATTERY_ANSWER.len() + 1)
        .find(|window| window.starts_with(&BATTERY_ANSWER))
        .map(|window| window[BATTERY_ANSWER.len()])
        .filter(|level| *level <= 100)
}

/// `HIDIOCGINPUT`: fetches the current value of an input report.
///
/// The dongle never pushes its answers on the interrupt endpoint - a plain
/// `read()` on the hidraw node sees nothing - so they have to be fetched with a
/// `GET_REPORT` control transfer, which hidraw only exposes through this ioctl.
fn get_input_report(device: &impl AsFd) -> io::Result<[u8; MSG_SIZE]> {
    use rustix::ioctl::{Updater, ioctl, opcode};

    const HIDIOCGINPUT: rustix::ioctl::Opcode = opcode::read_write::<[u8; MSG_SIZE]>(b'H', 0x0a);

    // The first byte tells the kernel which report we want.
    let mut frame = [0u8; MSG_SIZE];
    frame[0] = REPLY_REPORT_ID;

    // SAFETY: `HIDIOCGINPUT(len)` reads the report ID from, and writes at most
    // `len` bytes into, the buffer it is given. The opcode encodes
    // `len = MSG_SIZE`, which is exactly the size of `frame`, and `frame`
    // outlives the call.
    #[allow(unsafe_code)]
    unsafe {
        ioctl(
            device,
            Updater::<HIDIOCGINPUT, [u8; MSG_SIZE]>::new(&mut frame),
        )?;
    }
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from a Maxwell Xbox dongle right after a battery request, with
    /// the headset at 92%. Sixteen fresh bytes - the acknowledgement and the
    /// answer - followed by what earlier exchanges left behind, including an
    /// older answer that says 91%.
    const FRESH_FRAME: [u8; MSG_SIZE] = [
        0x07, 0x10, 0x80, 0x05, 0x5b, 0x03, 0x00, 0xd6, 0x0c, 0x00, 0x05, 0x5d, 0x05, 0x00, 0xd6,
        0x0c, 0x00, 0x00, 0x5c, 0x07, 0x31, 0x2e, 0x30, 0x2e, 0x31, 0x2e, 0x37, 0x34, 0x05, 0x00,
        0xd6, 0x0c, 0x00, 0x00, 0x5b, 0x05, 0x5b, 0x03, 0x00, 0xd6, 0x0c, 0x00, 0x05, 0x5d, 0x05,
        0x00, 0xd6, 0x0c, 0x00, 0x00, 0x5b, 0x00, 0x0c, 0x00, 0x00, 0x5b, 0x05, 0x5b, 0x03, 0x00,
        0xd6, 0x0c,
    ];

    /// The same report fetched a second time: nothing new, so the count is
    /// zero, yet the buffer still holds every old answer.
    const STALE_FRAME: [u8; MSG_SIZE] = [
        0x07, 0x00, 0x80, 0x05, 0x5b, 0x03, 0x00, 0xd6, 0x0c, 0x00, 0x05, 0x5d, 0x05, 0x00, 0xd6,
        0x0c, 0x00, 0x00, 0x5c, 0x07, 0x31, 0x2e, 0x30, 0x2e, 0x31, 0x2e, 0x37, 0x34, 0x05, 0x00,
        0xd6, 0x0c, 0x00, 0x00, 0x5b, 0x05, 0x5b, 0x03, 0x00, 0xd6, 0x0c, 0x00, 0x05, 0x5d, 0x05,
        0x00, 0xd6, 0x0c, 0x00, 0x00, 0x5b, 0x00, 0x0c, 0x00, 0x00, 0x5b, 0x05, 0x5b, 0x03, 0x00,
        0xd6, 0x0c,
    ];

    /// A frame holding `message` as its only fresh content.
    fn frame_with(message: &[u8]) -> [u8; MSG_SIZE] {
        let mut frame = [0u8; MSG_SIZE];
        frame[0] = REPLY_REPORT_ID;
        frame[FRESH_LEN_OFFSET] = u8::try_from(message.len()).unwrap();
        frame[PAYLOAD_OFFSET..PAYLOAD_OFFSET + message.len()].copy_from_slice(message);
        frame
    }

    #[test]
    fn reads_the_level_out_of_a_real_frame() {
        assert_eq!(parse_battery(&FRESH_FRAME), Some(92));
    }

    #[test]
    fn leftovers_from_earlier_exchanges_are_not_an_answer() {
        // Without the fresh-byte count this frame reads 92% for ever, which is
        // what a switched-off headset would look like.
        assert_eq!(parse_battery(&STALE_FRAME), None);
    }

    #[test]
    fn an_acknowledgement_is_not_mistaken_for_an_answer() {
        // The acknowledgement (type 5b) echoes `d6 0c` too. Followed by
        // padding it reads `d6 0c 00 00 00`, which a looser match turns into a
        // bogus 0% - the glitch HeadsetControl produces.
        let frame = frame_with(&[0x05, 0x5b, 0x03, 0x00, 0xd6, 0x0c, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(parse_battery(&frame), None);
    }

    #[test]
    fn an_empty_frame_means_no_answer() {
        assert_eq!(parse_battery(&[0u8; MSG_SIZE]), None);
        assert_eq!(parse_battery(&[]), None);
        assert_eq!(parse_battery(&[REPLY_REPORT_ID]), None);
    }

    #[test]
    fn a_level_above_one_hundred_is_rejected() {
        let mut message = [0u8; 9];
        message[0] = 0x05;
        message[1..8].copy_from_slice(&BATTERY_ANSWER);
        message[8] = 0xff;
        assert_eq!(parse_battery(&frame_with(&message)), None);
        message[8] = 100;
        assert_eq!(parse_battery(&frame_with(&message)), Some(100));
    }

    #[test]
    fn an_answer_cut_off_by_the_fresh_count_is_ignored() {
        // The header is there but the count stops short of the level byte.
        let mut message = [0u8; 9];
        message[0] = 0x05;
        message[1..8].copy_from_slice(&BATTERY_ANSWER);
        message[8] = 50;
        let mut frame = frame_with(&message);
        frame[FRESH_LEN_OFFSET] -= 1;
        assert_eq!(parse_battery(&frame), None);
    }

    #[test]
    fn a_fresh_count_larger_than_the_frame_does_not_panic() {
        let mut frame = FRESH_FRAME;
        frame[FRESH_LEN_OFFSET] = 0xff;
        assert_eq!(parse_battery(&frame), Some(92));
    }

    #[test]
    fn parses_the_ids_out_of_a_uevent() {
        let uevent = "DRIVER=hid-generic\nHID_ID=0003:00003329:00004B18\n\
                      HID_NAME=Audeze LLC Audeze Maxwell XBOX Dongle\nHID_PHYS=usb-1/input0\n";
        assert_eq!(
            parse_uevent(uevent),
            Some((
                BUS_USB,
                0x3329,
                0x4b18,
                "Audeze LLC Audeze Maxwell XBOX Dongle".to_owned()
            ))
        );
        assert_eq!(parse_uevent("DRIVER=hid-generic\n"), None);
        assert_eq!(parse_uevent("HID_ID=garbage\n"), None);
    }

    #[test]
    fn our_own_virtual_battery_is_not_mistaken_for_a_dongle() {
        let dir = std::env::temp_dir().join(format!("hbi-hidraw-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
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
            // Somebody else's device.
            (
                "hidraw2",
                "HID_ID=0003:00001532:000000A4\nHID_NAME=Razer Dock\n",
            ),
        ] {
            fs::create_dir_all(dir.join(node).join("device")).unwrap();
            fs::write(dir.join(node).join("device/uevent"), uevent).unwrap();
        }

        let found = discover(&dir);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].node, Path::new("/dev/hidraw10"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_headset_on_a_cable_is_what_charging_looks_like() {
        let dir = std::env::temp_dir().join(format!("hbi-usb-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
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
        assert!(!headset_is_wired(&dir));

        // The headset itself shows up once the cable is in.
        plug("5-2", "3329", "4b1e");
        assert!(headset_is_wired(&dir));

        fs::remove_dir_all(dir.join("5-2")).unwrap();
        assert!(!headset_is_wired(&dir));
        assert!(!headset_is_wired(Path::new("/nonexistent/usb")));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_maxwell_dongles_are_claimed() {
        assert!(supports(0x3329, 0x4b18));
        assert!(supports(0x3329, 0x4b19));
        assert!(!supports(0x3329, 0x0001));
        assert!(!supports(0x1038, 0x4b18));
    }

    #[test]
    fn discovery_survives_a_missing_sysfs() {
        assert!(discover(Path::new("/nonexistent/hidraw")).is_empty());
    }
}
