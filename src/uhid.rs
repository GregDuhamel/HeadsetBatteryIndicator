//! A minimal, dependency-light binding to the kernel's `/dev/uhid` character
//! device.
//!
//! `uhid` lets a userspace process create a virtual HID device. The kernel then
//! parses the report descriptor we hand it exactly as if the device were
//! physically plugged in. That is the whole trick behind this crate: a report
//! descriptor that declares nothing but a battery makes `hid-input` register a
//! `power_supply` object in sysfs, which UPower picks up on its own.
//!
//! The ABI is a single `struct uhid_event` written to (and read from) the
//! character device. It is `__packed`, has a fixed size, and has been stable
//! since Linux 3.18, so the offsets are hard-coded here instead of pulling in a
//! bindgen dependency.

use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};

use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use rustix::io::Errno;

/// Default path of the uhid character device.
pub const DEV_UHID: &str = "/dev/uhid";

/// `BUS_VIRTUAL` from `linux/input.h`.
///
/// Using the virtual bus (rather than `BUS_USB`) keeps device-specific kernel
/// HID drivers away from our fake device: they all match on a physical bus, so
/// only `hid-generic` binds to it.
const BUS_VIRTUAL: u16 = 0x06;

// Event types, from `linux/uhid.h`.
const UHID_DESTROY: u32 = 1;
const UHID_START: u32 = 2;
const UHID_STOP: u32 = 3;
const UHID_OPEN: u32 = 4;
const UHID_CLOSE: u32 = 5;
const UHID_OUTPUT: u32 = 6;
const UHID_GET_REPORT: u32 = 9;
const UHID_GET_REPORT_REPLY: u32 = 10;
const UHID_CREATE2: u32 = 11;
const UHID_INPUT2: u32 = 12;
const UHID_SET_REPORT: u32 = 13;
const UHID_SET_REPORT_REPLY: u32 = 14;

// Field offsets inside `struct uhid_event`. The 4-byte `type` comes first, then
// the union of request structures.
const OFF_TYPE: usize = 0;
const OFF_CREATE_NAME: usize = 4;
const LEN_NAME: usize = 128;
const OFF_CREATE_PHYS: usize = OFF_CREATE_NAME + LEN_NAME;
const LEN_PHYS: usize = 64;
const OFF_CREATE_UNIQ: usize = OFF_CREATE_PHYS + LEN_PHYS;
const LEN_UNIQ: usize = 64;
const OFF_CREATE_RD_SIZE: usize = OFF_CREATE_UNIQ + LEN_UNIQ;
const OFF_CREATE_BUS: usize = OFF_CREATE_RD_SIZE + 2;
const OFF_CREATE_VENDOR: usize = OFF_CREATE_BUS + 2;
const OFF_CREATE_PRODUCT: usize = OFF_CREATE_VENDOR + 4;
const OFF_CREATE_VERSION: usize = OFF_CREATE_PRODUCT + 4;
const OFF_CREATE_COUNTRY: usize = OFF_CREATE_VERSION + 4;
const OFF_CREATE_RD_DATA: usize = OFF_CREATE_COUNTRY + 4;

/// `HID_MAX_DESCRIPTOR_SIZE`: the largest report descriptor the kernel accepts.
const RD_DATA_MAX: usize = 4096;
/// `UHID_DATA_MAX`: the largest report payload.
const DATA_MAX: usize = 4096;

/// Size of `struct uhid_event`: the type tag plus its largest union member
/// (`struct uhid_create2_req`).
const EVENT_SIZE: usize = OFF_CREATE_RD_DATA + RD_DATA_MAX;

// `struct uhid_input2_req`: size, then data.
const OFF_INPUT_SIZE: usize = 4;
const OFF_INPUT_DATA: usize = 6;

// `struct uhid_get_report_req`: id, rnum, rtype.
const OFF_GET_REPORT_ID: usize = 4;
const OFF_GET_REPORT_RNUM: usize = 8;

// `struct uhid_get_report_reply_req`: id, err, size, data.
const OFF_GET_REPLY_ID: usize = 4;
const OFF_GET_REPLY_ERR: usize = 8;
const OFF_GET_REPLY_SIZE: usize = 10;
const OFF_GET_REPLY_DATA: usize = 12;

// `struct uhid_set_report_req` shares its first fields with the get variant.
const OFF_SET_REPORT_ID: usize = 4;
// `struct uhid_set_report_reply_req`: id, err.
const OFF_SET_REPLY_ID: usize = 4;
const OFF_SET_REPLY_ERR: usize = 8;

/// Report ID carried by the battery input report.
pub const REPORT_ID: u8 = 0x01;

/// Number of bytes in a battery input report, report ID included.
pub const REPORT_LEN: usize = 3;

/// Report descriptor of the virtual battery device.
///
/// Three things matter here, all checked against `drivers/hid/hid-input.c`:
///
/// * The top-level collection has to be an *input application*
///   (`IS_INPUT_APPLICATION`: generic desktop, digitizer, consumer control...).
///   `hidinput_connect()` returns before looking at a single field otherwise,
///   and the device ends up with a hidraw node and nothing else. Consumer
///   Control is the honest choice for a headset - it is what the real dongle
///   declares too.
/// * `Battery Strength` (Generic Device Controls, usage `0x20`) is what makes
///   `hidinput_setup_battery()` register the `power_supply`, and `Charging`
///   (Battery System page, usage `0x44`) is what flips it between
///   `Charging` and `Discharging`.
/// * `hidinput_connect()` tears the battery back down when a device ends up
///   with no input capability at all ("No inputs registered, leaving"). The
///   trailing vendor-defined one-bit field exists solely to avoid that: an
///   unknown usage that is one bit wide is mapped to `BTN_MISC`, which is
///   enough to keep the input device alive. `BTN_MISC` is outside every range
///   systemd's `input_id` builtin looks at, so the node stays untagged — udev
///   does not advertise it as a keyboard or a pointer, and UPower keeps
///   reporting the device as a plain battery. We never set that bit.
///
/// The vendor page is `0xff21`, picked because the kernel special-cases
/// `0xff00`, `0xff01`, `0xff09`, `0xff31`, `0xff43`, `0xff7f`, `0xffa0`,
/// `0xffbc` and `0xffd1`, and ignores their usages instead of mapping them.
pub const REPORT_DESCRIPTOR: &[u8] = &[
    0x05, 0x0c, //       Usage Page (Consumer)
    0x09, 0x01, //       Usage (Consumer Control)
    0xa1, 0x01, //       Collection (Application)
    0x85, REPORT_ID, //    Report ID (1)
    0x05, 0x06, //         Usage Page (Generic Device Controls)
    0x09, 0x20, //         Usage (Battery Strength)
    0x15, 0x00, //         Logical Minimum (0)
    0x26, 0x64, 0x00, //   Logical Maximum (100)
    0x75, 0x08, //         Report Size (8)
    0x95, 0x01, //         Report Count (1)
    0x81, 0x02, //         Input (Data, Variable, Absolute)
    0x05, 0x85, //         Usage Page (Battery System)
    0x09, 0x44, //         Usage (Charging)
    0x25, 0x01, //         Logical Maximum (1)
    0x75, 0x01, //         Report Size (1)
    0x81, 0x02, //         Input (Data, Variable, Absolute)
    0x06, 0x21, 0xff, //   Usage Page (Vendor Defined 0xff21)
    0x09, 0x02, //         Usage (0x02) -> BTN_MISC, never reported
    0x81, 0x02, //         Input (Data, Variable, Absolute)
    0x75, 0x06, //         Report Size (6)
    0x81, 0x03, //         Input (Constant) - padding
    0xc0, //             End Collection
];

/// Everything the kernel needs to instantiate the virtual device.
#[derive(Debug, Clone)]
pub struct CreateParams {
    /// Device name. It reaches UPower as `POWER_SUPPLY_MODEL_NAME`, which is
    /// the label KDE shows, so this should be the headset's marketing name.
    pub name: String,
    /// Physical path, free-form.
    pub phys: String,
    /// Unique identifier. The kernel names the sysfs power supply
    /// `hid-<uniq>-battery`, so it must be unique and filesystem-safe.
    pub uniq: String,
    /// Vendor ID, mirrored from the real headset.
    pub vendor: u32,
    /// Product ID, mirrored from the real headset.
    pub product: u32,
}

/// An event received from the kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// The kernel finished setting up the device.
    Start,
    /// The device is being torn down.
    Stop,
    /// A userspace process opened the device node.
    Open,
    /// The last reader closed the device node.
    Close,
    /// An output report was sent to us; we have nothing to do with it.
    Output,
    /// The kernel wants the current value of a report.
    GetReport {
        /// Request identifier to echo back in the reply.
        id: u32,
        /// Report number being requested.
        rnum: u8,
    },
    /// The kernel wants to set a report; we always acknowledge.
    SetReport {
        /// Request identifier to echo back in the reply.
        id: u32,
    },
    /// An event this crate does not handle.
    Other(u32),
}

/// An open handle on `/dev/uhid`, optionally backing one virtual device.
#[derive(Debug)]
pub struct Uhid {
    fd: OwnedFd,
}

impl AsFd for Uhid {
    /// Borrows the underlying descriptor, so the handle can be polled.
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl Uhid {
    /// Opens the uhid character device.
    ///
    /// # Errors
    ///
    /// Fails if the node cannot be opened read/write, which usually means the
    /// process is neither root nor holding a descriptor handed over by the
    /// service manager.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Self::from_fd(OwnedFd::from(file))
    }

    /// Adopts an already open descriptor, for example one passed by systemd.
    ///
    /// # Errors
    ///
    /// Fails if the descriptor cannot be switched to non-blocking mode.
    pub fn from_fd(fd: OwnedFd) -> io::Result<Self> {
        let flags = fcntl_getfl(&fd)?;
        fcntl_setfl(&fd, flags | OFlags::NONBLOCK)?;
        Ok(Self { fd })
    }

    /// Creates the virtual HID device.
    ///
    /// # Errors
    ///
    /// Fails if the descriptor is too large or the write to `/dev/uhid` fails,
    /// for instance because a device already exists on this handle.
    pub fn create(&self, params: &CreateParams, descriptor: &[u8]) -> io::Result<()> {
        if descriptor.len() > RD_DATA_MAX {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "report descriptor exceeds HID_MAX_DESCRIPTOR_SIZE",
            ));
        }

        let mut event = [0u8; EVENT_SIZE];
        put_u32(&mut event, OFF_TYPE, UHID_CREATE2);
        put_str(&mut event, OFF_CREATE_NAME, LEN_NAME, &params.name);
        put_str(&mut event, OFF_CREATE_PHYS, LEN_PHYS, &params.phys);
        put_str(&mut event, OFF_CREATE_UNIQ, LEN_UNIQ, &params.uniq);
        put_u16(
            &mut event,
            OFF_CREATE_RD_SIZE,
            u16::try_from(descriptor.len()).unwrap_or(u16::MAX),
        );
        put_u16(&mut event, OFF_CREATE_BUS, BUS_VIRTUAL);
        put_u32(&mut event, OFF_CREATE_VENDOR, params.vendor);
        put_u32(&mut event, OFF_CREATE_PRODUCT, params.product);
        put_u32(&mut event, OFF_CREATE_VERSION, 0);
        put_u32(&mut event, OFF_CREATE_COUNTRY, 0);
        event[OFF_CREATE_RD_DATA..OFF_CREATE_RD_DATA + descriptor.len()]
            .copy_from_slice(descriptor);

        self.write_event(&event)
    }

    /// Destroys the virtual device, leaving the handle reusable.
    ///
    /// # Errors
    ///
    /// Fails if the write to `/dev/uhid` fails.
    pub fn destroy(&self) -> io::Result<()> {
        let mut event = [0u8; EVENT_SIZE];
        put_u32(&mut event, OFF_TYPE, UHID_DESTROY);
        self.write_event(&event)
    }

    /// Feeds an input report to the kernel.
    ///
    /// # Errors
    ///
    /// Fails if the payload is too large or the write fails.
    pub fn send_input(&self, data: &[u8]) -> io::Result<()> {
        if data.len() > DATA_MAX {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "input report exceeds UHID_DATA_MAX",
            ));
        }

        let mut event = [0u8; EVENT_SIZE];
        put_u32(&mut event, OFF_TYPE, UHID_INPUT2);
        put_u16(
            &mut event,
            OFF_INPUT_SIZE,
            u16::try_from(data.len()).unwrap_or(u16::MAX),
        );
        event[OFF_INPUT_DATA..OFF_INPUT_DATA + data.len()].copy_from_slice(data);
        self.write_event(&event)
    }

    /// Answers a [`Event::GetReport`] request.
    ///
    /// The kernel expects the report ID as the first byte of `data`, the same
    /// convention `hid_hw_raw_request()` uses.
    ///
    /// # Errors
    ///
    /// Fails if the payload is too large or the write fails.
    pub fn reply_get_report(&self, id: u32, data: &[u8]) -> io::Result<()> {
        if data.len() > DATA_MAX {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "report reply exceeds UHID_DATA_MAX",
            ));
        }

        let mut event = [0u8; EVENT_SIZE];
        put_u32(&mut event, OFF_TYPE, UHID_GET_REPORT_REPLY);
        put_u32(&mut event, OFF_GET_REPLY_ID, id);
        put_u16(&mut event, OFF_GET_REPLY_ERR, 0);
        put_u16(
            &mut event,
            OFF_GET_REPLY_SIZE,
            u16::try_from(data.len()).unwrap_or(u16::MAX),
        );
        event[OFF_GET_REPLY_DATA..OFF_GET_REPLY_DATA + data.len()].copy_from_slice(data);
        self.write_event(&event)
    }

    /// Acknowledges a [`Event::SetReport`] request.
    ///
    /// # Errors
    ///
    /// Fails if the write fails.
    pub fn reply_set_report(&self, id: u32) -> io::Result<()> {
        let mut event = [0u8; EVENT_SIZE];
        put_u32(&mut event, OFF_TYPE, UHID_SET_REPORT_REPLY);
        put_u32(&mut event, OFF_SET_REPLY_ID, id);
        put_u16(&mut event, OFF_SET_REPLY_ERR, 0);
        self.write_event(&event)
    }

    /// Reads the next pending event, or `None` when the queue is empty.
    ///
    /// # Errors
    ///
    /// Fails on any read error other than `EAGAIN` and `EINTR`.
    pub fn read_event(&self) -> io::Result<Option<Event>> {
        let mut buf = [0u8; EVENT_SIZE];
        match rustix::io::read(&self.fd, &mut buf) {
            // A short read means the kernel had nothing queued; EINTR means a
            // signal landed mid-wait. Both are "come back later".
            Ok(0) | Err(Errno::AGAIN | Errno::INTR) => Ok(None),
            Ok(_) => Ok(Some(decode_event(&buf))),
            Err(err) => Err(err.into()),
        }
    }

    fn write_event(&self, event: &[u8]) -> io::Result<()> {
        let written = rustix::io::write(&self.fd, event)?;
        if written == event.len() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::WriteZero,
                format!("short write to uhid: {written} of {} bytes", event.len()),
            ))
        }
    }
}

/// Locates the power supply the kernel registered for the device named `uniq`.
///
/// The name is not stable across kernels: it was `hid-<uniq>-battery` for
/// years, and recent kernels append the report ID (`hid-<uniq>-battery-1`)
/// now that a HID device may carry several batteries. Match on the prefix
/// instead of guessing.
#[must_use]
pub fn find_power_supply(uniq: &str) -> Option<PathBuf> {
    find_power_supply_in(Path::new("/sys/class/power_supply"), uniq)
}

fn find_power_supply_in(class_dir: &Path, uniq: &str) -> Option<PathBuf> {
    let prefix = format!("hid-{uniq}-battery");
    std::fs::read_dir(class_dir)
        .ok()?
        .filter_map(Result::ok)
        .find(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.strip_prefix(&prefix)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('-'))
        })
        .map(|entry| entry.path())
}

/// Builds the payload of a battery input report.
#[must_use]
pub fn battery_report(percent: u8, charging: bool) -> [u8; REPORT_LEN] {
    [REPORT_ID, percent.min(100), u8::from(charging)]
}

fn decode_event(buf: &[u8; EVENT_SIZE]) -> Event {
    match get_u32(buf, OFF_TYPE) {
        UHID_START => Event::Start,
        UHID_STOP => Event::Stop,
        UHID_OPEN => Event::Open,
        UHID_CLOSE => Event::Close,
        UHID_OUTPUT => Event::Output,
        UHID_GET_REPORT => Event::GetReport {
            id: get_u32(buf, OFF_GET_REPORT_ID),
            rnum: buf[OFF_GET_REPORT_RNUM],
        },
        UHID_SET_REPORT => Event::SetReport {
            id: get_u32(buf, OFF_SET_REPORT_ID),
        },
        other => Event::Other(other),
    }
}

/// Copies `value` into a fixed-size, NUL-padded field, truncating on a `char`
/// boundary so the kernel never sees a partial UTF-8 sequence.
fn put_str(buf: &mut [u8], offset: usize, len: usize, value: &str) {
    let mut end = value.len().min(len - 1);
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    buf[offset..offset + end].copy_from_slice(&value.as_bytes()[..end]);
}

fn put_u16(buf: &mut [u8], offset: usize, value: u16) {
    buf[offset..offset + 2].copy_from_slice(&value.to_ne_bytes());
}

fn put_u32(buf: &mut [u8], offset: usize, value: u32) {
    buf[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
}

fn get_u32(buf: &[u8], offset: usize) -> u32 {
    let mut bytes = [0u8; 4];
    bytes.copy_from_slice(&buf[offset..offset + 4]);
    u32::from_ne_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_size_matches_the_kernel_abi() {
        // 4 (type) + 128 (name) + 64 (phys) + 64 (uniq) + 2 + 2 + 4 * 4 + 4096.
        assert_eq!(EVENT_SIZE, 4376);
        assert_eq!(OFF_CREATE_RD_DATA, 280);
    }

    #[test]
    fn descriptor_declares_battery_charging_and_one_input_bit() {
        // Battery Strength on the Generic Device Controls page.
        assert!(
            REPORT_DESCRIPTOR
                .windows(4)
                .any(|w| w == [0x05, 0x06, 0x09, 0x20])
        );
        // Charging on the Battery System page.
        assert!(
            REPORT_DESCRIPTOR
                .windows(4)
                .any(|w| w == [0x05, 0x85, 0x09, 0x44])
        );
        // A vendor page the kernel does not special-case, so its one-bit field
        // is mapped to BTN_MISC and the input device survives.
        assert!(
            REPORT_DESCRIPTOR
                .windows(3)
                .any(|w| w == [0x06, 0x21, 0xff])
        );
        // An input application at the top, or hidinput_connect() walks away
        // before registering anything - battery included.
        assert_eq!(
            &REPORT_DESCRIPTOR[..6],
            [0x05, 0x0c, 0x09, 0x01, 0xa1, 0x01]
        );
        assert!(REPORT_DESCRIPTOR.len() <= RD_DATA_MAX);
        // 8 battery bits + 1 charging bit + 1 vendor bit + 6 padding bits.
        assert_eq!(REPORT_LEN, 3);
    }

    #[test]
    fn create_event_is_laid_out_where_the_kernel_expects() {
        let mut event = [0u8; EVENT_SIZE];
        put_u32(&mut event, OFF_TYPE, UHID_CREATE2);
        put_str(&mut event, OFF_CREATE_NAME, LEN_NAME, "Audeze Maxwell");
        put_u16(&mut event, OFF_CREATE_BUS, BUS_VIRTUAL);
        put_u32(&mut event, OFF_CREATE_VENDOR, 0x3329);

        assert_eq!(get_u32(&event, OFF_TYPE), UHID_CREATE2);
        assert_eq!(
            &event[OFF_CREATE_NAME..OFF_CREATE_NAME + 14],
            b"Audeze Maxwell"
        );
        assert_eq!(event[OFF_CREATE_NAME + 14], 0, "name must stay NUL padded");
        assert_eq!(get_u32(&event, OFF_CREATE_VENDOR), 0x3329);
    }

    #[test]
    fn long_names_are_truncated_on_a_char_boundary() {
        let mut buf = [0u8; 8];
        put_str(&mut buf, 0, 8, "ééééééééé");
        // Seven bytes are available; the fourth 'é' would straddle the cut.
        assert_eq!(&buf, b"\xc3\xa9\xc3\xa9\xc3\xa9\0\0");
        assert!(std::str::from_utf8(&buf[..6]).is_ok());
    }

    #[test]
    fn the_power_supply_is_found_under_both_kernel_naming_schemes() {
        let dir = std::env::temp_dir().join(format!("hbi-psy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("hid-headset-3329-4b18-battery-1")).unwrap();
        std::fs::create_dir_all(dir.join("hid-other-battery")).unwrap();
        // A different device whose uniq merely starts the same way.
        std::fs::create_dir_all(dir.join("hid-headset-3329-4b18-batteryx")).unwrap();

        assert_eq!(
            find_power_supply_in(&dir, "headset-3329-4b18"),
            Some(dir.join("hid-headset-3329-4b18-battery-1"))
        );
        assert_eq!(
            find_power_supply_in(&dir, "other"),
            Some(dir.join("hid-other-battery"))
        );
        assert_eq!(find_power_supply_in(&dir, "absent"), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn battery_report_clamps_and_flags() {
        assert_eq!(battery_report(42, false), [REPORT_ID, 42, 0]);
        assert_eq!(battery_report(200, true), [REPORT_ID, 100, 1]);
    }

    #[test]
    fn get_report_requests_are_decoded() {
        let mut buf = [0u8; EVENT_SIZE];
        put_u32(&mut buf, OFF_TYPE, UHID_GET_REPORT);
        put_u32(&mut buf, OFF_GET_REPORT_ID, 7);
        buf[OFF_GET_REPORT_RNUM] = REPORT_ID;

        assert_eq!(
            decode_event(&buf),
            Event::GetReport {
                id: 7,
                rnum: REPORT_ID
            }
        );
    }
}
