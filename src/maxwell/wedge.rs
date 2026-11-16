//! Telling a wedged dongle from a headset that is switched off.
//!
//! To a reader that only listens the two are the same: nothing comes back.
//! The difference shows when the dongle itself is asked something. A battery
//! request addressed to the dongle rather than relayed to the headset
//! ([`LIVENESS_REQUEST`](super::frame::LIVENESS_REQUEST)) never goes over the
//! air; a dongle that is alive answers it within some 50 ms, headset or no
//! headset, and a wedged one never does. The [module documentation](super)
//! tells the incidents that taught this.
//!
//! Two parts: [`WedgeWatch`], the decisions - when a liveness request is due,
//! when the silence means a wedge, and when to say so again - with every
//! instant passed in, so that it is tested on instants of the test's
//! choosing; and [`Trace`], the last exchanges with the dongle, kept so that
//! the journal shows what led to a wedge when one is found.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How often the dongle is asked whether it is alive. Five minutes dates a
/// wedge to that precision, which is what finding its cause needs; the
/// request stays in the dongle, so it costs the headset nothing.
pub(super) const PROBE_EVERY: Duration = Duration::from_secs(300);

/// How long a liveness request is given. The answer takes some 50 ms and the
/// reader fetches once a second; five seconds also covers the dongle
/// re-enumerating in between, which takes two or three.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long after an unanswered liveness request the next one goes out, until
/// [`MAX_UNANSWERED`] have been lost. An answer can be lost without the
/// dongle being wedged - it re-enumerated, or another program fetched the
/// report first - so one miss proves nothing and is checked again at once.
const PROBE_RETRY: Duration = Duration::from_secs(10);

// A retry must not go out while the previous request can still be answered.
const _: () = assert!(PROBE_RETRY.as_secs() >= PROBE_TIMEOUT.as_secs());

/// Liveness requests lost in a row before the dongle is called wedged.
const MAX_UNANSWERED: u32 = 3;

/// How long between two warnings about the same wedge. The remedy is a hand
/// on the dongle; once a day is enough to be noticed without flooding the
/// journal for the two weeks the first incident went unseen.
pub(super) const WEDGE_REMINDER: Duration = Duration::from_secs(24 * 3600);

/// A wedge worth a warning now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Wedged {
    /// How long ago the dongle last said anything.
    pub(super) silence: Duration,
    /// Whether this wedge was already reported, a day or more ago.
    pub(super) reminder: bool,
}

/// The dongle's liveness: when it last spoke, the request that checks it, and
/// whether its silence has been reported.
///
/// One per reader, not per session: the dongle re-enumerates at every link
/// change, which opens a new session, and the silence is the dongle's, not
/// the session's. Only the decisions live here; the request and the logging
/// are the reader's.
#[derive(Debug)]
pub(super) struct WedgeWatch {
    /// When a dongle last said anything, or when the watch began.
    heard_at: Instant,
    /// When the last liveness request went out.
    probed_at: Option<Instant>,
    /// Whether that request is still waiting for its answer.
    pending: bool,
    /// How many liveness requests in a row went unanswered.
    unanswered: u32,
    /// When the wedge was last reported, if it was.
    warned_at: Option<Instant>,
}

impl WedgeWatch {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            heard_at: now,
            probed_at: None,
            pending: false,
            unanswered: 0,
            warned_at: None,
        }
    }

    /// Takes in that a dongle said something - anything: the answer to the
    /// liveness request, or whatever else proves it alive - and says whether
    /// the recovery is worth a line: only if the wedge was reported.
    pub(super) fn heard(&mut self, now: Instant) -> bool {
        self.heard_at = now;
        self.pending = false;
        self.unanswered = 0;
        self.warned_at.take().is_some()
    }

    /// How long nothing has been heard.
    pub(super) fn silence(&self, now: Instant) -> Duration {
        now.duration_since(self.heard_at)
    }

    /// Whether a liveness request is due now: at once on the first pass, every
    /// [`PROBE_EVERY`] after that, and [`PROBE_RETRY`] after one that was
    /// lost, until enough were lost to call the dongle wedged. A request
    /// still out for longer than [`PROBE_TIMEOUT`] is counted as lost here.
    /// The caller sends the request and reports it with [`WedgeWatch::probed`].
    pub(super) fn probe_due(&mut self, now: Instant) -> bool {
        let since_probe = self.probed_at.map(|at| now.duration_since(at));
        if self.pending {
            if since_probe.is_some_and(|since| since < PROBE_TIMEOUT) {
                return false;
            }
            self.pending = false;
            self.unanswered = self.unanswered.saturating_add(1);
        }
        let wait = if (1..MAX_UNANSWERED).contains(&self.unanswered) {
            PROBE_RETRY
        } else {
            PROBE_EVERY
        };
        since_probe.is_none_or(|since| since >= wait)
    }

    /// Takes in that a liveness request went out.
    pub(super) fn probed(&mut self, now: Instant) {
        self.probed_at = Some(now);
        self.pending = true;
    }

    /// Whether a liveness request is out with nothing heard since. For the
    /// one-shot reading of the `status` command, which has no time for three.
    pub(super) fn unanswered_probe(&self) -> bool {
        self.pending
    }

    /// Whether the dongle is taken for wedged: [`MAX_UNANSWERED`] liveness
    /// requests in a row were lost, and nothing was heard since.
    pub(super) fn is_wedged(&self) -> bool {
        self.unanswered >= MAX_UNANSWERED
    }

    /// Takes in that a dongle's node was just opened. While the dongle is
    /// taken for wedged that is worth a liveness request at once rather than
    /// in five minutes: replugging the dongle - the remedy - is what makes a
    /// new node, and the alert should not outlive the wedge by that long.
    pub(super) fn reopened(&mut self) {
        if self.is_wedged() {
            self.pending = false;
            self.probed_at = None;
        }
    }

    /// Whether a warning is due now: the first time [`MAX_UNANSWERED`]
    /// liveness requests in a row were lost, and again every
    /// [`WEDGE_REMINDER`] while the dongle stays silent.
    pub(super) fn wedged(&mut self, now: Instant) -> Option<Wedged> {
        if self.unanswered < MAX_UNANSWERED {
            return None;
        }
        let reminder = match self.warned_at {
            None => false,
            Some(at) if now.duration_since(at) >= WEDGE_REMINDER => true,
            Some(_) => return None,
        };
        self.warned_at = Some(now);
        Some(Wedged {
            silence: self.silence(now),
            reminder,
        })
    }
}

/// How many exchanges [`Trace`] keeps. A quiet dongle is one liveness request
/// and its answer every five minutes, so this is a couple of hours of them -
/// or the last minutes of a busy stretch, which is the part that matters.
const TRACE_CAPACITY: usize = 48;

/// The last exchanges with the dongle - sessions opened, requests sent, and
/// every report that carried something - with when each happened.
///
/// It exists for the day the dongle wedges: nobody knows yet what does it,
/// and by the time the wedge is noticed the journal has nothing on what
/// preceded it. The reader writes this out with the warning.
#[derive(Debug, Default)]
pub(super) struct Trace {
    events: VecDeque<(Instant, String)>,
}

impl Trace {
    /// Adds an exchange, dropping the oldest once [`TRACE_CAPACITY`] are kept.
    pub(super) fn record(&mut self, now: Instant, event: String) {
        if self.events.len() == TRACE_CAPACITY {
            self.events.pop_front();
        }
        self.events.push_back((now, event));
    }

    /// The exchanges kept, oldest first, each with how long before `now` it
    /// happened.
    pub(super) fn lines(&self, now: Instant) -> impl Iterator<Item = String> + '_ {
        self.events.iter().map(move |(at, event)| {
            format!(
                "{:>8.1}s ago  {event}",
                now.duration_since(*at).as_secs_f64()
            )
        })
    }
}

/// The bytes of a report as they are written in the documentation and the
/// tests: two hexadecimal digits each, separated by spaces.
pub(super) fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A duration as a person would say it in a log line: `45s`, `10min`,
/// `3h 12min`, `2d 4h`. Seconds are dropped past a minute, and minutes past a
/// day; the silence quoted is in hours, not milliseconds.
pub(super) fn human(duration: Duration) -> String {
    let secs = duration.as_secs();
    let (days, hours, mins) = (secs / 86_400, secs / 3600 % 24, secs / 60 % 60);
    match (days, hours, mins) {
        (0, 0, 0) => format!("{secs}s"),
        (0, 0, mins) => format!("{mins}min"),
        (0, hours, 0) => format!("{hours}h"),
        (0, hours, mins) => format!("{hours}h {mins}min"),
        (days, 0, _) => format!("{days}d"),
        (days, hours, _) => format!("{days}d {hours}h"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A second short of `duration`.
    fn shy(duration: Duration) -> Duration {
        Duration::from_secs(duration.as_secs() - 1)
    }

    /// Sends every liveness request that is due between `from` and `to`,
    /// one pass a second as the reader does, and returns when each went out.
    fn run(watch: &mut WedgeWatch, from: Instant, to: Instant) -> Vec<Instant> {
        let mut sent = Vec::new();
        let mut now = from;
        while now <= to {
            if watch.probe_due(now) {
                watch.probed(now);
                sent.push(now);
            }
            now += Duration::from_secs(1);
        }
        sent
    }

    #[test]
    fn a_dongle_that_answers_is_asked_every_five_minutes() {
        let start = Instant::now();
        let mut watch = WedgeWatch::new(start);

        // At once on the first pass: a daemon started on a wedged dongle
        // should say so within the half minute, not after five.
        assert!(watch.probe_due(start));
        watch.probed(start);
        // Out, and not sent twice.
        assert!(watch.unanswered_probe());
        assert!(!watch.probe_due(start + Duration::from_secs(1)));

        // Answered: nothing more until five minutes after the request.
        let answered = start + Duration::from_secs(1);
        assert!(!watch.heard(answered));
        assert!(!watch.unanswered_probe());
        assert!(!watch.probe_due(start + shy(PROBE_EVERY)));
        assert!(watch.probe_due(start + PROBE_EVERY));
        assert_eq!(watch.wedged(start + PROBE_EVERY), None);
    }

    #[test]
    fn three_requests_lost_in_a_row_are_a_wedge() {
        let start = Instant::now();
        let mut watch = WedgeWatch::new(start);
        let s = Duration::from_secs;

        // Nothing ever answers: the request, then two retries ten seconds
        // after each was sent, then back to the slow cadence.
        let sent = run(&mut watch, start, start + s(400));
        assert_eq!(sent, [start, start + s(10), start + s(20), start + s(320)]);

        // Two lost requests are not a wedge; the third is, once it timed out.
        let mut watch = WedgeWatch::new(start);
        run(&mut watch, start, start + s(24));
        assert_eq!(watch.wedged(start + s(24)), None);
        run(&mut watch, start + s(25), start + s(25));
        assert_eq!(
            watch.wedged(start + s(25)),
            Some(Wedged {
                silence: s(25),
                reminder: false
            })
        );
        // Said once.
        assert_eq!(watch.wedged(start + s(26)), None);
    }

    #[test]
    fn a_node_opened_on_a_wedged_dongle_is_asked_at_once() {
        let start = Instant::now();
        let mut watch = WedgeWatch::new(start);
        let s = Duration::from_secs;

        // Not wedged: a new node - the dongle re-enumerates at every link
        // change - does not bring the next request forward.
        run(&mut watch, start, start);
        watch.heard(start + s(1));
        watch.reopened();
        assert!(!watch.is_wedged());
        assert!(!watch.probe_due(start + s(2)));

        // Wedged: the slow cadence, until a node is opened - the dongle was
        // replugged, with luck - and then the request goes out at once.
        let mut watch = WedgeWatch::new(start);
        run(&mut watch, start, start + s(25));
        assert!(watch.is_wedged());
        assert!(!watch.probe_due(start + s(60)));
        watch.reopened();
        assert!(watch.probe_due(start + s(60)));
        watch.probed(start + s(60));
        // Answered: the wedge is over.
        assert!(watch.wedged(start + s(60)).is_some());
        assert!(watch.heard(start + s(61)));
        assert!(!watch.is_wedged());
    }

    #[test]
    fn one_lost_answer_is_not_a_wedge() {
        // The dongle re-enumerated, or another program fetched the report
        // first: the retry is answered, and the count starts over.
        let start = Instant::now();
        let mut watch = WedgeWatch::new(start);
        let s = Duration::from_secs;

        run(&mut watch, start, start + s(21));
        watch.heard(start + s(21));
        // Two were lost, the third answered: back to the slow cadence, and a
        // later loss counts from one again.
        assert_eq!(watch.wedged(start + s(30)), None);
        let sent = run(
            &mut watch,
            start + s(22),
            start + s(20) + PROBE_EVERY + s(12),
        );
        assert_eq!(
            sent,
            [start + s(20) + PROBE_EVERY, start + s(30) + PROBE_EVERY]
        );
        assert_eq!(watch.wedged(start + s(32) + PROBE_EVERY), None);
    }

    #[test]
    fn the_wedge_is_said_again_once_a_day() {
        let start = Instant::now();
        let mut watch = WedgeWatch::new(start);
        let s = Duration::from_secs;

        run(&mut watch, start, start + s(25));
        let warned = start + s(25);
        assert!(watch.wedged(warned).is_some());

        // The slow cadence goes on, unanswered, and nothing more is said...
        run(&mut watch, warned, warned + shy(WEDGE_REMINDER));
        assert_eq!(watch.wedged(warned + shy(WEDGE_REMINDER)), None);
        // ...until a day has passed, with the silence it has grown to.
        assert_eq!(
            watch.wedged(warned + WEDGE_REMINDER),
            Some(Wedged {
                silence: s(25) + WEDGE_REMINDER,
                reminder: true
            })
        );
    }

    #[test]
    fn the_dongle_speaking_again_is_a_recovery_and_restarts_the_clock() {
        let start = Instant::now();
        let mut watch = WedgeWatch::new(start);
        let s = Duration::from_secs;

        // A dongle that was never reported has nothing to recover from.
        assert!(!watch.heard(start));

        run(&mut watch, start, start + s(25));
        assert!(watch.wedged(start + s(25)).is_some());

        // The dongle was replugged and says something: worth a line, one.
        let replugged = start + s(3 * 3600);
        assert!(watch.heard(replugged));
        let last = replugged + s(1);
        assert!(!watch.heard(last));
        assert_eq!(watch.silence(last + s(5)), s(5));
        assert_eq!(watch.wedged(last + s(5)), None);

        // Wedged again later: a fresh warning, not a reminder a day away.
        let again = run(&mut watch, last, last + PROBE_EVERY + s(30));
        let third = *again.last().unwrap();
        run(&mut watch, third + s(5), third + s(5));
        assert_eq!(
            watch.wedged(third + s(5)).map(|wedged| wedged.reminder),
            Some(false)
        );
    }

    #[test]
    fn the_trace_keeps_the_last_exchanges_with_their_age() {
        let start = Instant::now();
        let mut trace = Trace::default();
        for second in 0..60 {
            trace.record(
                start + Duration::from_secs(second),
                format!("event {second}"),
            );
        }

        let lines: Vec<String> = trace.lines(start + Duration::from_secs(60)).collect();
        assert_eq!(lines.len(), TRACE_CAPACITY);
        // The oldest were dropped; what is left is in order, oldest first.
        assert_eq!(lines[0], "    48.0s ago  event 12");
        assert_eq!(lines[TRACE_CAPACITY - 1], "     1.0s ago  event 59");
    }

    #[test]
    fn bytes_are_written_as_the_documentation_writes_them() {
        assert_eq!(
            hex(&[0x05, 0x5b, 0x03, 0x00, 0xd6, 0x0c, 0x02]),
            "05 5b 03 00 d6 0c 02"
        );
        assert_eq!(hex(&[]), "");
    }

    #[test]
    fn durations_read_like_a_person_would_say_them() {
        let s = Duration::from_secs;
        assert_eq!(human(s(0)), "0s");
        assert_eq!(human(s(45)), "45s");
        assert_eq!(human(s(600)), "10min");
        assert_eq!(human(s(3600)), "1h");
        assert_eq!(human(s(3 * 3600 + 12 * 60 + 7)), "3h 12min");
        assert_eq!(human(s(2 * 86_400 + 4 * 3600 + 59 * 60)), "2d 4h");
        assert_eq!(human(s(14 * 86_400)), "14d");
    }
}
