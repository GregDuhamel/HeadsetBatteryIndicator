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
//!
//! [`Device::get_input`]: hidraw::Device::get_input
//! [`Device::write`]: hidraw::Device::write

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, Instant};

use log::debug;

use crate::headset::{BatteryState, Headset};

mod discover;
mod frame;
mod session;

use discover::{Dongle, discover_in, headset_is_wired};
use session::{LISTEN_FIRST, Link, Session, report_error, report_recovery};

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

/// How often, and how many times, a one-shot reading looks for its answer.
const ANSWER_DELAY: Duration = Duration::from_millis(60);
const ANSWER_POLLS: u32 = 10;

/// Whether this module handles the given USB device.
#[must_use]
pub fn supports(vendor_id: u16, product_id: u16) -> bool {
    vendor_id == VENDOR_ID && PRODUCT_IDS.contains(&product_id)
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

#[cfg(test)]
mod tests {
    use super::session::tests::session;
    use super::*;

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
    fn only_maxwell_dongles_are_claimed() {
        assert!(supports(0x3329, 0x4b18));
        assert!(supports(0x3329, 0x4b19));
        assert!(!supports(0x3329, 0x0001));
        assert!(!supports(0x1038, 0x4b18));
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
