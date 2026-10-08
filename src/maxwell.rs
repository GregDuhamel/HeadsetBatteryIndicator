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
//! # The transport
//!
//! Everything that touches the kernel goes through the [`hidraw`] crate,
//! shared with the other daemons of this account: [`hidraw::discover`] lists
//! `/sys/class/hidraw` and tells the USB dongle from the virtual battery this
//! daemon publishes under the same IDs, [`Device::write`] sends the request as
//! an output report, [`Device::get_input`] fetches the answer, and
//! [`hidraw::is_gone`] tells an unplugged dongle from a transfer that merely
//! failed. What stays here is the dongle's side of it: its IDs, its report
//! layout, and when to ask.
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
//! a headset that is supposed to be linked stops answering. Stops, not for
//! ever: a session that has heard nothing is asked again every ten minutes,
//! which is a hundredth of the pace that wedged the dongle, so a headset that
//! came back unannounced is still found.
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
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread::sleep;
use std::time::{Duration, Instant};

use hidraw::{Bus, Device, Filter};
use log::{debug, info, trace, warn};

use crate::headset::{BatteryState, Headset};

/// Audeze's USB vendor ID.
pub const VENDOR_ID: u16 = 0x3329;

/// Product IDs this module knows how to talk to.
pub const PRODUCT_IDS: [u16; 2] = [
    0x4b19, // Maxwell dongle
    0x4b18, // Maxwell Xbox dongle
];

/// Where the kernel lists USB devices. It is read for the wired headset (see
/// [`headset_is_wired`]); the dongle's hidraw nodes come from
/// [`hidraw::discover`].
const USB_DEVICES: &str = "/sys/bus/usb/devices";

/// What the dongle's product string is reported as when the kernel has none
/// for it (`HID_NAME` missing from the uevent, which usbhid never leaves out).
const DEFAULT_PRODUCT: &str = "Audeze Maxwell";

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
/// switched off and on again. Three quick questions per session is the
/// ceiling: the dongle re-enumerates whenever the headset comes or goes, which
/// opens a new session, so a session still silent after these most likely has
/// nobody behind it - and gets the slow cadence below.
const ASK_AGAIN_AFTER: [Duration; 2] = [Duration::from_secs(10), Duration::from_secs(30)];

/// Once the quick questions are spent, how long between further ones to a
/// session that has heard nothing. Without any, a headset whose link event was
/// missed - or that was given up on as mute - stayed unlisted until the dongle
/// next said something, which can be never. Ten minutes is six requests an
/// hour, against the three hundred and more that wedged the dongle.
const ASK_IDLE_RETRY: Duration = Duration::from_secs(600);

// Giving up on a mute headset relies on the quick questions being spent by
// then, so that the slow cadence applies: see `Session::ask`.
const _: () = assert!(MAX_UNANSWERED as usize > ASK_AGAIN_AFTER.len());

/// How long an access error has to last before it is worth a warning. Right
/// after the dongle re-enumerates its new node exists for a moment without the
/// permissions udev is about to give it, and that happens at every link change.
const ERROR_SETTLE: Duration = Duration::from_secs(5);

/// The access error being watched, and whether it is worth a line yet.
///
/// What it decides is kept apart from the logging, so that it can be tested
/// with instants of the test's choosing; the one instance lives in
/// [`ACCESS`], and [`report_error`] and [`report_recovery`] do the talking.
#[derive(Debug)]
struct AccessWatch {
    /// The error's text, since when it has been seen, and whether it has
    /// been reported - once it has, it is not reported again.
    last: Option<(String, Instant, bool)>,
}

impl AccessWatch {
    const fn new() -> Self {
        Self { last: None }
    }

    /// Takes in a failure, and says whether it is worth a warning now: the
    /// same error has lasted [`ERROR_SETTLE`], and has not been reported.
    /// A different error starts a new clock.
    fn failing(&mut self, message: String, now: Instant) -> bool {
        let (since, reported) = match self.last.as_ref() {
            Some((previous, since, reported)) if *previous == message => (*since, *reported),
            _ => (now, false),
        };
        let report = !reported && now.duration_since(since) >= ERROR_SETTLE;
        self.last = Some((message, since, reported || report));
        report
    }

    /// Takes in a success, and says whether the recovery is worth a line:
    /// only if the failure was.
    fn recovered(&mut self) -> bool {
        matches!(self.last.take(), Some((_, _, true)))
    }
}

static ACCESS: Mutex<AccessWatch> = Mutex::new(AccessWatch::new());

/// Reports a failure to reach a dongle that sysfs says is there, once it has
/// lasted.
///
/// This is loud on purpose. A dongle that is listed but cannot be opened looks,
/// from the outside, exactly like a headset that is switched off - and the
/// usual cause is a udev rule that sorts after `73-seat-late.rules`, which
/// leaves the node showing `crw-rw----` while its ACL says `group::---`.
fn report_error(node: &Path, err: &io::Error) {
    let message = format!("{}: {err}", node.display());
    let report = ACCESS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .failing(message.clone(), Instant::now());
    if !report {
        debug!("cannot reach {message}");
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
}

fn report_recovery() {
    let recovered = ACCESS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .recovered();
    if recovered {
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
    device: Device,
    product_id: u16,
    link: Link,
    level: Option<u8>,
    /// Which of the reader's battery messages `level` came from (see
    /// [`Reader::readings`]); zero before the first.
    sample: u64,
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
        let device = Device::open(&dongle.node)?;
        Ok(Self {
            device,
            product_id: dongle.product_id,
            link,
            level: None,
            sample: 0,
            opened_at: Instant::now(),
            asked_at: None,
            asks: 0,
            pending: false,
            unanswered: 0,
        })
    }

    /// Fetches the report and takes in whatever it says. `readings` is the
    /// reader's count of battery messages, which every level is numbered by.
    fn listen(&mut self, readings: &mut u64) -> io::Result<()> {
        let frame = fetch(&self.device)?;
        self.take_in(&frame, readings);
        Ok(())
    }

    /// Takes in what a report says.
    fn take_in(&mut self, frame: &[u8], readings: &mut u64) {
        for message in messages(frame) {
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
                    *readings += 1;
                    self.sample = *readings;
                    self.pending = false;
                    self.unanswered = 0;
                }
            }
        }
    }

    /// Whether a battery request is due: regularly while the headset is linked,
    /// and otherwise a few quick questions per session, then slow ones.
    fn should_ask(&self, now: Instant, interval: Duration, listen_first: Duration) -> bool {
        match self.link {
            Link::Up => self
                .asked_at
                .is_none_or(|at| now.duration_since(at) >= interval),
            // The first question after having listened for a while, a couple
            // more shortly after, then one every `ASK_IDLE_RETRY`. Requests
            // sent to nobody are what this reader exists to avoid, hence the
            // quick ones are few and the slow ones are slow.
            Link::Unknown => match (self.asked_at, self.asks) {
                (None, _) => now.duration_since(self.opened_at) >= listen_first,
                (Some(at), asks) => {
                    let delay = asks
                        .checked_sub(1)
                        .and_then(|extra| usize::try_from(extra).ok())
                        .and_then(|index| ASK_AGAIN_AFTER.get(index))
                        .copied()
                        .unwrap_or(ASK_IDLE_RETRY);
                    now.duration_since(at) >= delay
                }
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
            // Each of those requests counted as an ask, so the quick questions
            // of `ASK_AGAIN_AFTER` are spent and only the slow cadence is left.
            warn!(
                "the headset is reported linked but left {MAX_UNANSWERED} battery requests \
                 unanswered; asking again every {ASK_IDLE_RETRY:?} at most, or when the \
                 dongle says something. If this persists while the headset works, \
                 power-cycle the dongle"
            );
            self.link = Link::Unknown;
            self.level = None;
            self.pending = false;
            self.unanswered = 0;
            self.asked_at = Some(now);
            return Ok(());
        }

        // One output report, report ID first (`BATTERY_REQUEST[0]`), padded to
        // the dongle's fixed report size.
        let mut request = [0u8; MSG_SIZE];
        request[..BATTERY_REQUEST.len()].copy_from_slice(&BATTERY_REQUEST);
        self.device.write(&request)?;
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
    /// How many battery messages have been received, over every session. It
    /// numbers the readings handed out as [`Headset::sample`], and is kept
    /// here rather than per session so that the numbering survives the
    /// dongle's re-enumerations: a counter restarting at zero made a reading
    /// from the new session look like the previous session's first one.
    readings: u64,
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
        let dongles = discover();
        self.reconcile(&dongles, wired, Instant::now(), interval, listen_first)
    }

    /// One pass over the dongles sysfs currently lists: sessions of nodes that
    /// are gone are closed, the rest are listened to, and every dongle is
    /// reported as a headset.
    fn reconcile(
        &mut self,
        dongles: &[Dongle],
        wired: bool,
        now: Instant,
        interval: Duration,
        listen_first: Duration,
    ) -> Vec<Headset> {
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
                let (battery, sample) =
                    match self.listen_to(dongle, now, interval, listen_first, wired) {
                        Ok(reading) => {
                            report_recovery();
                            reading
                        }
                        Err(err) => {
                            // Only a dongle that is gone - `ENODEV`, the node
                            // vanishing mid-exchange as it re-enumerates - loses
                            // its session; the node that replaces it starts a
                            // new one anyway. Every other failure keeps it: a
                            // transfer the dongle did not take (`EIO`, `EPIPE`,
                            // `ETIMEDOUT`), or a kernel without the ioctl, says
                            // nothing about the headset, and closing the session
                            // would throw away the link belief and, above all,
                            // the questions already asked - the new session would
                            // listen, then ask again, which is the request budget
                            // this module exists to protect. (It used to close
                            // on any error; the two cases that motivated it, the
                            // node vanishing and a node not yet having its
                            // permissions, are a gone device and a failed open,
                            // and are handled the same as before.) The level is
                            // withheld while the failure lasts, and the bridge's
                            // graces decide what becomes of the entry.
                            if hidraw::is_gone(&err) {
                                if let Some(session) = self.sessions.remove(&dongle.node) {
                                    self.last_link.insert(session.product_id, session.link);
                                }
                            }
                            report_error(&dongle.node, &err);
                            (BatteryState::Unavailable, 0)
                        }
                    };
                Headset {
                    name: "Audeze Maxwell".to_owned(),
                    product: dongle.product.clone(),
                    vendor_id: VENDOR_ID,
                    product_id: dongle.product_id,
                    supports_battery: true,
                    battery,
                    // The level is cached between the dongle's messages, and this
                    // runs every second: say which message it came from.
                    sample: Some(sample),
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
    ) -> io::Result<(BatteryState, u64)> {
        if !self.sessions.contains_key(&dongle.node) {
            let link = self
                .last_link
                .get(&dongle.product_id)
                .copied()
                .unwrap_or(Link::Unknown);
            let session = Session::open(dongle, link)?;
            debug!("opened {} ({})", dongle.node.display(), dongle.product);
            self.sessions.insert(dongle.node.clone(), session);
        }
        let session = self
            .sessions
            .get_mut(&dongle.node)
            .expect("the session was just inserted");

        session.listen(&mut self.readings)?;
        if session.should_ask(now, interval, listen_first) {
            session.ask(now)?;
        }
        Ok((session.battery(wired), session.sample))
    }
}

/// A Maxwell dongle found in sysfs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Dongle {
    node: PathBuf,
    /// The kernel's name for the device (`HID_NAME`, the USB manufacturer and
    /// product strings), reported as [`Headset::product`].
    product: String,
    product_id: u16,
}

impl Dongle {
    /// The dongle behind a hidraw node, if the node is one. [`discover`] has
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

/// Lists the hidraw nodes that belong to a supported dongle, without opening
/// them.
///
/// Only the USB bus is looked at. The virtual battery this daemon creates
/// carries the dongle's vendor and product IDs too, on `BUS_VIRTUAL`, and has
/// a hidraw node of its own: without the bus filter the reader would mistake
/// it for a second dongle and start sending it battery requests.
fn discover() -> Vec<Dongle> {
    discover_in(Path::new(hidraw::SYSFS_ROOT), Path::new(hidraw::DEV_ROOT))
}

/// As [`discover`], with the hidraw class directory and the device directory
/// given, so that a fake tree can stand in for sysfs under test.
///
/// A sysfs that cannot be listed is reported as no dongle at all, as a reader
/// that runs every second must not fail over it; the bridge says so when it
/// lasts.
fn discover_in(sysfs_root: &Path, dev_root: &Path) -> Vec<Dongle> {
    let filter = Filter::new().bus(Bus::Usb).vendor(VENDOR_ID);
    match hidraw::discover_in(sysfs_root, dev_root, &filter) {
        Ok(nodes) => nodes.into_iter().filter_map(Dongle::from_node).collect(),
        Err(err) => {
            debug!("cannot list {}: {err}", sysfs_root.display());
            Vec::new()
        }
    }
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

/// Fetches the current value of the dongle's answer report (`HIDIOCGINPUT`,
/// [`Device::get_input`]).
///
/// The dongle never pushes its answers on the interrupt endpoint - a plain
/// `read()` on the hidraw node sees nothing - so they have to be fetched with a
/// `GET_REPORT` control transfer, which hidraw only exposes through that ioctl.
/// The report is always [`MSG_SIZE`] bytes, so the buffer is that large and
/// the count the kernel returns is not needed: a shorter answer would leave
/// zeros behind it, and zeros say nothing (see [`messages`]).
fn fetch(device: &Device) -> io::Result<[u8; MSG_SIZE]> {
    // Byte 0 tells the kernel which report is wanted; the report overwrites it.
    let mut frame = [0u8; MSG_SIZE];
    frame[0] = REPLY_REPORT_ID;
    device.get_input(&mut frame)?;
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;

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

    /// A session on `/dev/null`: it swallows what is written and answers no
    /// ioctl, so a battery request goes nowhere and a fetch fails with
    /// `ENOTTY` - a failure that is not a gone device.
    fn session(link: Link, level: Option<u8>, asked: Option<Instant>) -> Session {
        let null = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
            .unwrap();
        Session {
            device: Device::from_fd(null, "/dev/null"),
            product_id: 0x4b18,
            link,
            level,
            sample: u64::from(level.is_some()),
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
    fn a_silent_dongle_is_asked_a_few_times_quickly_then_slowly() {
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

        // ...and then only every ten minutes, whatever the interval: a request
        // every ten seconds to a headset that was off is what wedged the
        // dongle, but never asking again left a headset that came back
        // unannounced unlisted for good.
        let mut asked_out = session(Link::Unknown, None, ago(31));
        asked_out.asks = 3;
        assert!(!asked_out.should_ask(now, minute, Duration::ZERO));
        assert!(!asked_out.should_ask(now, Duration::ZERO, Duration::ZERO));
        asked_out.asked_at = ago(599);
        assert!(!asked_out.should_ask(now, Duration::ZERO, Duration::ZERO));
        asked_out.asked_at = ago(600);
        assert!(asked_out.should_ask(now, minute, Duration::ZERO));
        // And the same for however many questions went unanswered.
        asked_out.asks = 40;
        assert!(asked_out.should_ask(now, minute, Duration::ZERO));
        asked_out.asked_at = ago(599);
        assert!(!asked_out.should_ask(now, minute, Duration::ZERO));
    }

    #[test]
    fn a_linked_headset_that_stops_answering_is_left_alone() {
        // Second line of defence: requests must not pile up behind a headset
        // that is announced but mute. /dev/null swallows the writes.
        let mut session = session(Link::Up, Some(90), None);
        let now = Instant::now();

        for _ in 0..=MAX_UNANSWERED {
            assert_eq!(session.link, Link::Up);
            session.ask(now).unwrap();
        }
        // It gave up: no more asking until the dongle speaks again, or the
        // slow cadence comes round.
        assert_eq!(session.link, Link::Unknown);
        assert!(!session.pending);
        assert!(!session.should_ask(now, Duration::from_secs(60), Duration::ZERO));
        assert!(!session.should_ask(now + ASK_IDLE_RETRY / 2, Duration::ZERO, Duration::ZERO));
        assert!(session.should_ask(now + ASK_IDLE_RETRY, Duration::ZERO, Duration::ZERO));
    }

    #[test]
    fn readings_are_numbered_across_sessions() {
        // The dongle re-enumerates after every link change, which opens a new
        // session. The bridge tells a confirmation from a repeated reading by
        // the number, so the numbering must not restart with the session.
        let mut readings = 0;
        let mut first = session(Link::Unknown, None, None);
        first.take_in(&frame_with(&hex(POWER_ON)), &mut readings);
        assert_eq!((first.level, first.sample), (Some(99), 1));

        let mut second = session(Link::Unknown, None, None);
        second.take_in(&frame_with(&hex(POWER_ON)), &mut readings);
        assert_eq!((second.level, second.sample), (Some(99), 2));

        // A report with no level in it leaves the number alone.
        second.take_in(&STALE_FRAME, &mut readings);
        assert_eq!((second.sample, readings), (2, 2));
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
    fn a_transfer_that_merely_failed_keeps_the_session() {
        // The fetch fails on /dev/null (ENOTTY), which is not what an
        // unplugged dongle answers (ENODEV): the session survives, and with
        // it the link belief and the questions already asked - a fresh
        // session would listen, then spend its quick questions again.
        let dongle = Dongle {
            node: PathBuf::from("/dev/null"),
            product: "Audeze Dongle".to_owned(),
            product_id: 0x4b18,
        };
        let now = Instant::now();
        let mut reader = Reader::new();
        let mut session = session(Link::Up, Some(90), Some(now));
        session.asks = 3;
        reader.sessions.insert(dongle.node.clone(), session);

        let minute = Duration::from_secs(60);
        let headsets = reader.reconcile(std::slice::from_ref(&dongle), false, now, minute, minute);
        assert_eq!(headsets.len(), 1);
        // No level while the failure lasts: the bridge's graces take over.
        assert_eq!(headsets[0].battery, BatteryState::Unavailable);
        assert_eq!(headsets[0].product, "Audeze Dongle");
        let kept = reader
            .sessions
            .get(&dongle.node)
            .expect("the session is kept");
        assert_eq!((kept.link, kept.level, kept.asks), (Link::Up, Some(90), 3));

        // A node that sysfs no longer lists takes its session with it, and
        // what it knew of the link is handed to the next one.
        let headsets = reader.reconcile(&[], false, now, minute, minute);
        assert_eq!(headsets, []);
        assert_eq!(reader.sessions.len(), 0);
        assert_eq!(reader.last_link.get(&0x4b18), Some(&Link::Up));
    }

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
    fn only_maxwell_dongles_are_claimed() {
        assert!(supports(0x3329, 0x4b18));
        assert!(supports(0x3329, 0x4b19));
        assert!(!supports(0x3329, 0x0001));
        assert!(!supports(0x1038, 0x4b18));
    }

    #[test]
    fn discovery_survives_a_missing_sysfs() {
        assert_eq!(
            discover_in(Path::new("/nonexistent/hidraw"), Path::new("/dev")),
            [] as [Dongle; 0]
        );
    }

    #[test]
    fn an_access_error_is_reported_once_it_has_lasted_and_only_once() {
        let mut watch = AccessWatch::new();
        let start = Instant::now();
        let denied = || "/dev/hidraw3: Permission denied".to_owned();

        // Right after the dongle re-enumerates its node exists for a moment
        // without its permissions: not news yet.
        assert!(!watch.failing(denied(), start));
        assert!(!watch.failing(denied(), start + ERROR_SETTLE / 2));
        // Lasted: said, once.
        assert!(watch.failing(denied(), start + ERROR_SETTLE));
        assert!(!watch.failing(denied(), start + ERROR_SETTLE * 2));
        assert!(!watch.failing(denied(), start + Duration::from_secs(3600)));

        // The recovery is worth a line, because the failure was.
        assert!(watch.recovered());
        assert!(!watch.recovered(), "said once too");

        // A failure that never settled is not: nothing was said about it.
        assert!(!watch.failing(denied(), start));
        assert!(!watch.recovered());

        // A different error starts its own clock.
        assert!(!watch.failing(denied(), start));
        assert!(!watch.failing(
            "/dev/hidraw3: Inappropriate ioctl".to_owned(),
            start + ERROR_SETTLE
        ));
        assert!(watch.failing(
            "/dev/hidraw3: Inappropriate ioctl".to_owned(),
            start + ERROR_SETTLE * 2
        ));
    }

    #[test]
    fn the_headset_going_away_forgets_the_level_and_the_pending_request() {
        let mut readings = 0;
        let mut session = session(Link::Up, Some(90), Some(Instant::now()));
        session.pending = true;
        session.unanswered = 2;

        // The dongle says the headset is gone: no level to report, and the
        // request that was out is not going to be answered - nor is it a
        // strike against the headset.
        session.take_in(&frame_with(&hex(POWER_OFF)), &mut readings);
        assert_eq!(session.link, Link::Down);
        assert_eq!(session.level, None);
        assert!(!session.pending);
        assert_eq!(session.unanswered, 0);
        assert_eq!(session.battery(false), BatteryState::Disconnected);

        // Back on: the link, then the level the dongle volunteers.
        session.take_in(&frame_with(&hex(POWER_ON)), &mut readings);
        assert_eq!(session.link, Link::Up);
        assert_eq!(session.level, Some(99));
        assert_eq!(session.battery(false), BatteryState::Discharging(99));
    }

    #[test]
    fn a_dongle_that_re_enumerates_hands_its_link_to_the_new_session() {
        // /dev/null and /dev/zero open read/write and answer no ioctl, so
        // they can stand in for the node before and after re-enumeration.
        let dongle = |node: &str| Dongle {
            node: PathBuf::from(node),
            product: "Audeze Dongle".to_owned(),
            product_id: 0x4b18,
        };
        let now = Instant::now();
        let minute = Duration::from_secs(60);
        let mut reader = Reader::new();

        // A first session, which the dongle tells that the headset is off.
        let before = dongle("/dev/null");
        reader.reconcile(std::slice::from_ref(&before), false, now, minute, minute);
        assert_eq!(reader.sessions.len(), 1);
        let session = reader.sessions.get_mut(&before.node).unwrap();
        assert_eq!(session.link, Link::Unknown, "a fresh session knows nothing");
        session.link = Link::Down;

        // The node vanishes: its session goes, and what it knew stays.
        assert_eq!(reader.reconcile(&[], false, now, minute, minute), []);
        assert_eq!(reader.sessions.len(), 0);
        assert_eq!(reader.last_link.get(&0x4b18), Some(&Link::Down));

        // The node that replaces it inherits the belief, so "the headset is
        // gone" is not forgotten for the second it took to re-enumerate -
        // and only one question goes out, after listening.
        let after = dongle("/dev/zero");
        reader.reconcile(std::slice::from_ref(&after), false, now, minute, minute);
        let session = reader.sessions.get(&after.node).unwrap();
        assert_eq!(session.link, Link::Down);
        assert_eq!(session.battery(false), BatteryState::Disconnected);
        let opened = session.opened_at;
        assert!(!session.should_ask(opened, minute, LISTEN_FIRST));
        assert!(session.should_ask(opened + LISTEN_FIRST, minute, LISTEN_FIRST));

        // A dongle of another model gets no belief from this one.
        let other = Dongle {
            product_id: 0x4b19,
            ..dongle("/dev/null")
        };
        reader.reconcile(&[after.clone(), other.clone()], false, now, minute, minute);
        assert_eq!(
            reader.sessions.get(&other.node).unwrap().link,
            Link::Unknown
        );
    }
}
