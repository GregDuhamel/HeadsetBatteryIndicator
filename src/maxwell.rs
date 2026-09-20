//! A native battery reader for the Audeze Maxwell, talking to its dongle
//! directly over hidraw.
//!
//! HeadsetControl supports the Maxwell, but reads its battery unreliably: it
//! replays a twenty-packet sequence and expects the battery answer to sit in
//! the buffer of a *different* request, one frame later. When the dongle is a
//! few milliseconds late the answer is missed. It also scans for `d6 0c 00 00`
//! anywhere in the dongle's report, leftovers included - hence stray `0%` and
//! `44%` readings, and a level reported for a headset that is switched off.
//!
//! # The report
//!
//! The dongle's input report is a stream of small messages, oldest first, and
//! its second byte counts the bytes written since the report was last fetched;
//! whatever lies beyond is left over from earlier exchanges.
//!
//! ```text
//! 05 <type> <len> 00 <payload; len bytes>
//!
//! 05 5b 03 00  d6 0c 00                   acknowledgement of request d6 0c
//! 05 5d 05 00  d6 0c 00 00 5b             answer to d6 0c: 0x5b = 91 %
//! 05 5d 0e 00  b1 2c 00 02 01 01 <addr>…  link event: 01 = headset linked
//! 05 5d 0e 00  b1 2c 00 02 00 01 <addr>…  link event: 00 = headset gone
//! ```
//!
//! The answers are only available through a `GET_REPORT` control transfer
//! (`HIDIOCGINPUT`); the dongle never pushes them on the interrupt endpoint.
//!
//! # Listen, do not ask
//!
//! The dongle announces the headset's link state on its own, and volunteers the
//! battery level when the headset connects. So the reader is passive: it
//! fetches the report about once a second - a control transfer to the dongle,
//! nothing goes over the air - and only *asks* for the level while the headset
//! is known to be linked, plus a handful of times per opened node - the first after
//! listening for a few seconds - to learn where things stand.
//!
//! That restraint is not politeness. A daemon that kept sending a request every
//! ten seconds to a headset that was switched off left the dongle's command
//! channel dead after an hour or so: audio still worked, but it answered no
//! request at all, not even those addressed to the dongle itself, and only a
//! power cycle brought it back. The likeliest explanation is that requests for
//! an absent headset pile up in the dongle; whatever the cause, not sending
//! them is the cure. As a second line of defence the reader stops asking when
//! a headset that is supposed to be linked stops answering.
//!
//! The dongle also re-enumerates on USB a second or two after every link
//! change, so its hidraw node vanishes and comes back; the reader reopens it.
//!
//! # Charging
//!
//! Nothing the dongle answers changes when the headset is charging: every
//! register it exposes to a read was compared plugged and unplugged, and only
//! the level moved. What does change is the USB bus. Plugged into the computer,
//! the headset enumerates as a device of its own (`3329:4b1e`) next to the
//! dongle. Its presence is what this module reports as charging; a headset on a
//! wall charger is invisible, and keeps reading as discharging.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread::sleep;
use std::time::{Duration, Instant};

use log::{debug, info, trace, warn};

use crate::headset::{BatteryState, Headset};

/// Audeze's USB vendor ID.
pub const VENDOR_ID: u16 = 0x3329;

/// Product IDs this module knows how to talk to.
pub const PRODUCT_IDS: [u16; 2] = [
    0x4b19, // Maxwell dongle
    0x4b18, // Maxwell Xbox dongle
];

/// Where the kernel lists hidraw nodes.
const HIDRAW_CLASS: &str = "/sys/class/hidraw";

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

/// "What is the battery level?", on output report `0x06`, relayed to the
/// headset (the `0x80` in third position; `0x00` addresses the dongle itself).
const BATTERY_REQUEST: [u8; 9] = [0x06, 0x07, 0x80, 0x05, 0x5a, 0x03, 0x00, 0xd6, 0x0c];

/// Offset of the count of fresh bytes in an input report.
const FRESH_LEN_OFFSET: usize = 1;

/// Offset of the first message in an input report.
const PAYLOAD_OFFSET: usize = 3;

/// Header of the battery answer: message type `0x5d`, five payload bytes,
/// echoing the request (`d6 0c`) with a zero status. The level follows.
const BATTERY_ANSWER: [u8; 7] = [0x5d, 0x05, 0x00, 0xd6, 0x0c, 0x00, 0x00];

/// Header of a link event. The byte that follows is `01` when the headset is
/// linked and `00` when it is gone; then come a `01` and the headset's address.
const LINK_EVENT: [u8; 7] = [0x5d, 0x0e, 0x00, 0xb1, 0x2c, 0x00, 0x02];

/// How often, and how many times, a one-shot reading looks for its answer.
const ANSWER_DELAY: Duration = Duration::from_millis(60);
const ANSWER_POLLS: u32 = 10;

/// Requests a linked headset may leave unanswered before the reader stops
/// asking. See the module documentation for why it must stop.
const MAX_UNANSWERED: u32 = 3;

/// After opening a dongle, listen this long before asking anything: if the
/// headset is there the dongle usually says so on its own, and if it just left
/// there is nobody to ask.
const LISTEN_FIRST: Duration = Duration::from_secs(3);

/// How long to wait before asking again a session that has heard nothing, one
/// entry per extra question. A single request does get lost now and then, and
/// a lone question would leave a headset that is on unnoticed until it is
/// switched off and on again. Three questions per session is the ceiling: the
/// dongle re-enumerates whenever the headset comes or goes, which opens a new
/// session, so a session still silent after these has nobody behind it.
const ASK_AGAIN_AFTER: [Duration; 2] = [Duration::from_secs(10), Duration::from_secs(30)];

/// How long an access error has to last before it is worth a warning. Right
/// after the dongle re-enumerates its new node exists for a moment without the
/// permissions udev is about to give it, and that happens at every link change.
const ERROR_SETTLE: Duration = Duration::from_secs(5);

/// The access error being watched: its text, since when, and whether it has
/// been reported yet - once it has, it is not reported again.
static LAST_ERROR: Mutex<Option<(String, Instant, bool)>> = Mutex::new(None);

/// Reports a failure to reach a dongle that sysfs says is there, once it has
/// lasted.
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

    let (since, reported) = match last.as_ref() {
        Some((previous, since, reported)) if *previous == message => (*since, *reported),
        _ => (Instant::now(), false),
    };
    if reported || since.elapsed() < ERROR_SETTLE {
        debug!("cannot reach {message}");
        *last = Some((message, since, reported));
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
    *last = Some((message, since, true));
}

fn report_recovery() {
    let mut last = LAST_ERROR
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Only worth a line if the failure was.
    if let Some((_, _, true)) = last.take() {
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

/// What the dongle last said about the headset's radio link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Link {
    /// Nothing heard yet, either way.
    Unknown,
    /// The headset is linked.
    Up,
    /// The headset is gone: switched off, or out of range.
    Down,
}

/// Something the dongle's report stream said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Message {
    /// The headset's link came up or went down.
    Link(bool),
    /// The battery level, in percent.
    Battery(u8),
}

/// One open dongle, and what is known about the headset behind it.
#[derive(Debug)]
struct Session {
    file: File,
    product: String,
    product_id: u16,
    link: Link,
    level: Option<u8>,
    opened_at: Instant,
    /// When a battery request was last sent, and how many were in this session.
    asked_at: Option<Instant>,
    asks: u32,
    /// Whether that request is still waiting for its answer.
    pending: bool,
    /// How many requests in a row went unanswered.
    unanswered: u32,
}

impl Session {
    /// Opens a dongle. `link` is what the previous session on this dongle knew:
    /// the dongle re-enumerates a second or two after every link change, and
    /// forgetting what it had just announced would turn an instant "the headset
    /// is gone" back into a guess.
    fn open(dongle: &Dongle, link: Link) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&dongle.node)?;
        Ok(Self {
            file,
            product: dongle.product.clone(),
            product_id: dongle.product_id,
            link,
            level: None,
            opened_at: Instant::now(),
            asked_at: None,
            asks: 0,
            pending: false,
            unanswered: 0,
        })
    }

    /// Fetches the report and takes in whatever it says.
    fn listen(&mut self) -> io::Result<()> {
        let frame = get_input_report(&self.file)?;
        for message in messages(&frame) {
            match message {
                Message::Link(true) => {
                    if self.link != Link::Up {
                        debug!("the dongle reports the headset linked");
                    }
                    self.link = Link::Up;
                    self.unanswered = 0;
                }
                Message::Link(false) => {
                    if self.link != Link::Down {
                        debug!("the dongle reports the headset gone");
                    }
                    self.link = Link::Down;
                    self.level = None;
                    self.pending = false;
                    self.unanswered = 0;
                }
                Message::Battery(level) => {
                    trace!("battery: {level}%");
                    self.link = Link::Up;
                    self.level = Some(level);
                    self.pending = false;
                    self.unanswered = 0;
                }
            }
        }
        Ok(())
    }

    /// Whether a battery request is due. Never while the headset is known to be
    /// gone, which is the whole point.
    fn should_ask(&self, now: Instant, interval: Duration, listen_first: Duration) -> bool {
        match self.link {
            Link::Up => self
                .asked_at
                .is_none_or(|at| now.duration_since(at) >= interval),
            // A few times per session at most, the first after having listened
            // for a while. Requests sent to nobody are what this reader exists to
            // avoid, so there is a hard ceiling rather than a slow retry.
            Link::Unknown => match (self.asked_at, self.asks) {
                (None, _) => now.duration_since(self.opened_at) >= listen_first,
                (Some(at), asks) => usize::try_from(asks - 1)
                    .ok()
                    .and_then(|index| ASK_AGAIN_AFTER.get(index))
                    .is_some_and(|delay| now.duration_since(at) >= *delay),
            },
            // The dongle said the headset is gone - possibly to the previous
            // session, this belief being inherited. One question, in case the
            // event announcing its return was lost with the re-enumeration.
            Link::Down => {
                self.asked_at.is_none() && now.duration_since(self.opened_at) >= listen_first
            }
        }
    }

    fn ask(&mut self, now: Instant) -> io::Result<()> {
        if self.pending {
            // Asking again with the previous request still open: it was lost.
            self.unanswered += 1;
        }
        if self.link == Link::Up && self.unanswered >= MAX_UNANSWERED {
            warn!(
                "the headset is reported linked but left {MAX_UNANSWERED} battery requests \
                 unanswered; not asking again until the dongle says something. If this \
                 persists while the headset works, power-cycle the dongle"
            );
            self.link = Link::Unknown;
            self.level = None;
            self.pending = false;
            self.unanswered = 0;
            self.asked_at = Some(now);
            return Ok(());
        }

        let mut request = [0u8; MSG_SIZE];
        request[..BATTERY_REQUEST.len()].copy_from_slice(&BATTERY_REQUEST);
        self.file.write_all(&request)?;
        self.asked_at = Some(now);
        self.asks += 1;
        self.pending = true;
        Ok(())
    }

    fn battery(&self, wired: bool) -> BatteryState {
        match (self.link, self.level) {
            (Link::Down, _) => BatteryState::Disconnected,
            (_, Some(level)) if wired => BatteryState::Charging(Some(level)),
            (_, Some(level)) => BatteryState::Discharging(level),
            (_, None) => BatteryState::Unavailable,
        }
    }
}

/// A native reader that keeps its dongles open between polls.
#[derive(Debug, Default)]
pub struct Reader {
    sessions: BTreeMap<PathBuf, Session>,
    /// What the last session on each dongle (by product ID) knew of the link,
    /// handed to the session that replaces it.
    last_link: BTreeMap<u16, Link>,
}

impl Reader {
    /// A reader with no dongle open yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Listens to every Maxwell dongle plugged in, asking for the level only
    /// when it is due (every `interval` while the headset is linked).
    ///
    /// Meant to be called about once a second. A dongle that is present but
    /// cannot be opened is still listed, with an unavailable battery.
    pub fn poll(&mut self, interval: Duration) -> Vec<Headset> {
        self.poll_with(interval, LISTEN_FIRST)
    }

    fn poll_with(&mut self, interval: Duration, listen_first: Duration) -> Vec<Headset> {
        let wired = headset_is_wired(Path::new(USB_DEVICES));
        let dongles = discover(Path::new(HIDRAW_CLASS));
        let now = Instant::now();

        // A node that is gone takes its session with it: the dongle
        // re-enumerates after every link change.
        let last_link = &mut self.last_link;
        self.sessions.retain(|node, session| {
            let present = dongles.iter().any(|dongle| &dongle.node == node);
            if !present {
                last_link.insert(session.product_id, session.link);
            }
            present
        });

        dongles
            .iter()
            .map(|dongle| {
                let battery = match self.listen_to(dongle, now, interval, listen_first, wired) {
                    Ok(battery) => {
                        report_recovery();
                        battery
                    }
                    Err(err) => {
                        // Most often the node vanishing mid-exchange, or not
                        // having its permissions yet right after coming back.
                        if let Some(session) = self.sessions.remove(&dongle.node) {
                            self.last_link.insert(session.product_id, session.link);
                        }
                        report_error(&dongle.node, &err);
                        BatteryState::Unavailable
                    }
                };
                Headset {
                    name: "Audeze Maxwell".to_owned(),
                    product: dongle.product.clone(),
                    vendor_id: VENDOR_ID,
                    product_id: dongle.product_id,
                    supports_battery: true,
                    battery,
                }
            })
            .collect()
    }

    /// A one-shot reading, for the `status` command: open, ask once, and give
    /// the answer time to arrive. Usually there within 60 ms, it can take a few
    /// hundred right after the dongle re-enumerated.
    pub fn read_once(&mut self) -> Vec<Headset> {
        let mut headsets = self.poll_with(Duration::ZERO, Duration::ZERO);
        for _ in 0..ANSWER_POLLS {
            if headsets
                .iter()
                .all(|headset| headset.battery != BatteryState::Unavailable)
            {
                break;
            }
            sleep(ANSWER_DELAY);
            // `Duration::MAX`: listen only, the one request is already out.
            headsets = self.poll_with(Duration::MAX, Duration::ZERO);
        }
        headsets
    }

    fn listen_to(
        &mut self,
        dongle: &Dongle,
        now: Instant,
        interval: Duration,
        listen_first: Duration,
        wired: bool,
    ) -> io::Result<BatteryState> {
        if !self.sessions.contains_key(&dongle.node) {
            let link = self
                .last_link
                .get(&dongle.product_id)
                .copied()
                .unwrap_or(Link::Unknown);
            let session = Session::open(dongle, link)?;
            debug!("opened {} ({})", dongle.node.display(), session.product);
            self.sessions.insert(dongle.node.clone(), session);
        }
        let session = self
            .sessions
            .get_mut(&dongle.node)
            .expect("the session was just inserted");

        session.listen()?;
        if session.should_ask(now, interval, listen_first) {
            session.ask(now)?;
        }
        Ok(session.battery(wired))
    }
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

/// The part of an input report written since it was last fetched.
fn fresh(frame: &[u8]) -> &[u8] {
    let len = frame.get(FRESH_LEN_OFFSET).copied().map_or(0, usize::from);
    let end = (PAYLOAD_OFFSET + len).min(frame.len());
    frame.get(PAYLOAD_OFFSET..end).unwrap_or_default()
}

/// Everything the fresh part of an input report says, oldest first.
///
/// Matching on the full message headers, rather than on the command bytes
/// alone, is what keeps an acknowledgement (which echoes `d6 0c` too) from
/// being read as a level. A message cut off by the end of the buffer is
/// skipped, and so is a level above 100.
fn messages(frame: &[u8]) -> Vec<Message> {
    let data = fresh(frame);
    (0..data.len())
        .filter_map(|at| {
            let rest = &data[at..];
            if let Some(value) = rest
                .strip_prefix(&BATTERY_ANSWER[..])
                .and_then(<[u8]>::first)
            {
                (*value <= 100).then_some(Message::Battery(*value))
            } else {
                rest.strip_prefix(&LINK_EVENT[..])
                    .and_then(<[u8]>::first)
                    .map(|state| Message::Link(*state != 0))
            }
        })
        .collect()
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

    /// A frame holding `stream` as its only fresh content.
    fn frame_with(stream: &[u8]) -> [u8; MSG_SIZE] {
        let mut frame = [0u8; MSG_SIZE];
        frame[0] = REPLY_REPORT_ID;
        frame[FRESH_LEN_OFFSET] = u8::try_from(stream.len()).unwrap();
        frame[PAYLOAD_OFFSET..PAYLOAD_OFFSET + stream.len()].copy_from_slice(stream);
        frame
    }

    fn hex(text: &str) -> Vec<u8> {
        text.split_whitespace()
            .map(|byte| u8::from_str_radix(byte, 16).unwrap())
            .collect()
    }

    /// What the dongle said, unprompted, when the headset was switched off.
    const POWER_OFF: &str =
        "05 5d 0e 00 b1 2c 00 02 00 01 8d 90 9b 67 89 c2 ff 00 05 5c 03 00 80 2c 01";

    /// And when it was switched back on: the link event, then the level.
    const POWER_ON: &str = "05 5c 03 00 80 2c 01 05 5d 0e 00 b1 2c 00 02 01 01 8d 90 9b 67 89 \
                            c2 80 01 05 5c 03 00 80 2c 03 05 5b 03 00 d6 0c 00 05 5d 05 00 d6 \
                            0c 00 00 63";

    #[test]
    fn reads_the_level_out_of_a_real_frame() {
        assert_eq!(messages(&FRESH_FRAME), [Message::Battery(92)]);
    }

    #[test]
    fn leftovers_from_earlier_exchanges_are_not_an_answer() {
        // Without the fresh-byte count this frame reads 92% for ever, which is
        // what HeadsetControl shows for a headset that is switched off.
        assert_eq!(messages(&STALE_FRAME), []);
    }

    #[test]
    fn the_dongle_announces_the_headset_going_and_coming() {
        assert_eq!(
            messages(&frame_with(&hex(POWER_OFF))),
            [Message::Link(false)]
        );
        assert_eq!(
            messages(&frame_with(&hex(POWER_ON))),
            [Message::Link(true), Message::Battery(99)]
        );
    }

    #[test]
    fn an_acknowledgement_is_not_mistaken_for_an_answer() {
        // The acknowledgement (type 5b) echoes `d6 0c` too. Followed by
        // padding it reads `d6 0c 00 00 00`, which a looser match turns into a
        // bogus 0% - the glitch HeadsetControl produces.
        let frame = frame_with(&hex("05 5b 03 00 d6 0c 00 00 00 00"));
        assert_eq!(messages(&frame), []);
    }

    #[test]
    fn nonsense_is_ignored() {
        assert_eq!(messages(&[0u8; MSG_SIZE]), []);
        assert_eq!(messages(&[]), []);
        assert_eq!(messages(&[REPLY_REPORT_ID]), []);
        // A level above 100.
        assert_eq!(
            messages(&frame_with(&hex("05 5d 05 00 d6 0c 00 00 ff"))),
            []
        );
        assert_eq!(
            messages(&frame_with(&hex("05 5d 05 00 d6 0c 00 00 64"))),
            [Message::Battery(100)]
        );
        // An answer cut off before its level byte.
        assert_eq!(messages(&frame_with(&hex("05 5d 05 00 d6 0c 00 00"))), []);
    }

    #[test]
    fn a_fresh_count_larger_than_the_frame_does_not_panic() {
        let mut frame = FRESH_FRAME;
        frame[FRESH_LEN_OFFSET] = 0xff;
        // Clamped to the buffer: everything then counts, leftovers included.
        assert_eq!(messages(&frame).last(), Some(&Message::Battery(91)));
    }

    fn session(link: Link, level: Option<u8>, asked: Option<Instant>) -> Session {
        Session {
            file: File::open("/dev/null").unwrap(),
            product: "test".to_owned(),
            product_id: 0x4b18,
            link,
            level,
            opened_at: Instant::now(),
            asked_at: asked,
            asks: u32::from(asked.is_some()),
            pending: false,
            unanswered: 0,
        }
    }

    #[test]
    fn a_headset_that_is_gone_is_asked_once_at_most() {
        // The rule that keeps the dongle alive: once a session has asked, no
        // other request goes out while the headset is known to be off, however
        // long that lasts and however short the interval.
        let long_ago = Instant::now().checked_sub(Duration::from_secs(86_400));
        let now = Instant::now();
        let minute = Duration::from_secs(60);

        assert!(!session(Link::Down, None, long_ago).should_ask(now, minute, Duration::ZERO));
        assert!(!session(Link::Down, None, long_ago).should_ask(
            now,
            Duration::ZERO,
            Duration::ZERO
        ));
        // The one question a session is allowed waits until it has listened.
        assert!(!session(Link::Down, None, None).should_ask(now, minute, LISTEN_FIRST));
        assert!(session(Link::Down, None, None).should_ask(now, minute, Duration::ZERO));
    }

    #[test]
    fn a_linked_headset_is_asked_once_per_interval() {
        let now = Instant::now();
        let minute = Duration::from_secs(60);
        let asked = |seconds| now.checked_sub(Duration::from_secs(seconds));

        assert!(session(Link::Up, Some(90), None).should_ask(now, minute, Duration::ZERO));
        assert!(!session(Link::Up, Some(90), asked(30)).should_ask(now, minute, Duration::ZERO));
        assert!(session(Link::Up, Some(90), asked(61)).should_ask(now, minute, Duration::ZERO));
    }

    #[test]
    fn a_silent_dongle_is_asked_a_few_times_and_never_again() {
        let now = Instant::now();
        let minute = Duration::from_secs(60);
        let ago = |seconds| now.checked_sub(Duration::from_secs(seconds));

        // The first question waits until the session has listened...
        assert!(!session(Link::Unknown, None, None).should_ask(now, minute, LISTEN_FIRST));
        assert!(session(Link::Unknown, None, None).should_ask(now, minute, Duration::ZERO));

        // ...a lost request is made up for, since one does get lost at times...
        let mut asked_once = session(Link::Unknown, None, ago(5));
        assert!(!asked_once.should_ask(now, minute, Duration::ZERO));
        asked_once.asked_at = ago(11);
        assert!(asked_once.should_ask(now, minute, Duration::ZERO));

        let mut asked_twice = session(Link::Unknown, None, ago(31));
        asked_twice.asks = 2;
        assert!(asked_twice.should_ask(now, minute, Duration::ZERO));

        // ...and then it stops, however long the silence lasts: a request every
        // ten seconds to a headset that was off is what wedged the dongle.
        let mut asked_out = session(Link::Unknown, None, ago(86_400));
        asked_out.asks = 3;
        assert!(!asked_out.should_ask(now, minute, Duration::ZERO));
        assert!(!asked_out.should_ask(now, Duration::ZERO, Duration::ZERO));
    }

    #[test]
    fn a_linked_headset_that_stops_answering_is_left_alone() {
        // Second line of defence: requests must not pile up behind a headset
        // that is announced but mute. /dev/null swallows the writes.
        let mut session = session(Link::Up, Some(90), None);
        session.file = OpenOptions::new().write(true).open("/dev/null").unwrap();
        let now = Instant::now();

        for _ in 0..=MAX_UNANSWERED {
            assert_eq!(session.link, Link::Up);
            session.ask(now).unwrap();
        }
        // It gave up: no more asking until the dongle speaks again.
        assert_eq!(session.link, Link::Unknown);
        assert!(!session.pending);
        assert!(!session.should_ask(now, Duration::from_secs(60), Duration::ZERO));
    }

    #[test]
    fn the_battery_state_follows_the_link() {
        assert_eq!(
            session(Link::Down, Some(80), None).battery(false),
            BatteryState::Disconnected
        );
        assert_eq!(
            session(Link::Up, Some(80), None).battery(false),
            BatteryState::Discharging(80)
        );
        assert_eq!(
            session(Link::Up, Some(80), None).battery(true),
            BatteryState::Charging(Some(80))
        );
        assert_eq!(
            session(Link::Up, None, None).battery(false),
            BatteryState::Unavailable
        );
        assert_eq!(
            session(Link::Unknown, None, None).battery(false),
            BatteryState::Unavailable
        );
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
