//! One open dongle: what it says about the headset's link, when to ask for
//! the level, and the access errors worth a line.
//!
//! The cadence constants below are the reader's request budget; the [module
//! documentation](super) says why it has to be a budget.

use std::io;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use hidraw::Device;
use log::{debug, info, trace, warn};

use super::discover::Dongle;
use super::frame::{BATTERY_REQUEST, MSG_SIZE, Message, fetch, messages};
use crate::headset::BatteryState;

/// Requests a linked headset may leave unanswered before the reader stops
/// asking. See the module documentation for why it must stop.
const MAX_UNANSWERED: u32 = 3;

/// After opening a dongle, listen this long before asking anything: if the
/// headset is there the dongle usually says so on its own, and if it just left
/// there is nobody to ask.
pub(super) const LISTEN_FIRST: Duration = Duration::from_secs(3);

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
pub(super) fn report_error(node: &Path, err: &io::Error) {
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

pub(super) fn report_recovery() {
    let recovered = ACCESS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .recovered();
    if recovered {
        info!("the dongle is reachable again");
    }
}

/// What the dongle last said about the headset's radio link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Link {
    /// Nothing heard yet, either way.
    Unknown,
    /// The headset is linked.
    Up,
    /// The headset is gone: switched off, or out of range.
    Down,
}

/// One open dongle, and what is known about the headset behind it.
#[derive(Debug)]
pub(super) struct Session {
    device: Device,
    pub(super) product_id: u16,
    pub(super) link: Link,
    pub(super) level: Option<u8>,
    /// Which of the reader's battery messages `level` came from (see
    /// [`Reader::readings`](super::Reader::readings)); zero before the first.
    pub(super) sample: u64,
    pub(super) opened_at: Instant,
    /// When a battery request was last sent, and how many were in this session.
    asked_at: Option<Instant>,
    pub(super) asks: u32,
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
    pub(super) fn open(dongle: &Dongle, link: Link) -> io::Result<Self> {
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
    pub(super) fn listen(&mut self, readings: &mut u64) -> io::Result<()> {
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
    pub(super) fn should_ask(
        &self,
        now: Instant,
        interval: Duration,
        listen_first: Duration,
    ) -> bool {
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

    pub(super) fn ask(&mut self, now: Instant) -> io::Result<()> {
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

    pub(super) fn battery(&self, wired: bool) -> BatteryState {
        match (self.link, self.level) {
            (Link::Down, _) => BatteryState::Disconnected,
            (_, Some(level)) if wired => BatteryState::Charging(Some(level)),
            (_, Some(level)) => BatteryState::Discharging(level),
            (_, None) => BatteryState::Unavailable,
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::fs::OpenOptions;

    use super::super::frame::tests::{POWER_OFF, POWER_ON, STALE_FRAME, frame_with, hex};
    use super::*;

    /// A session on `/dev/null`: it swallows what is written and answers no
    /// ioctl, so a battery request goes nowhere and a fetch fails with
    /// `ENOTTY` - a failure that is not a gone device.
    pub(in crate::maxwell) fn session(
        link: Link,
        level: Option<u8>,
        asked: Option<Instant>,
    ) -> Session {
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
}
