//! The decisions the bridge makes about a reading, kept as pure functions of
//! their inputs so that the tests can hand them instants of their choosing:
//! when to poll again, whether a level is to be believed, and whether a
//! battery still deserves its entry.

use std::time::{Duration, Instant};

use super::Config;

/// How often the native reader is run. It only listens to the dongle - nothing
/// goes over the air - and that is how it learns within a second that the
/// headset came or went.
pub(super) const NATIVE_TICK: Duration = Duration::from_secs(1);

/// Once a poll goes unanswered, how soon to ask again. Several retries fit in
/// the grace, so one lost reading never makes the entry flap - and withdrawing
/// is cheap to undo, the battery is back on the first poll that answers.
const UNANSWERED_RETRY: Duration = Duration::from_secs(3);

/// A reading further than this from the last known level is held back until the
/// next poll confirms it.
///
/// Wireless dongles hand out the occasional bogus frame - an Audeze Maxwell
/// will answer `0%` or `44%` between two `92%` readings - and a spurious `0%`
/// is enough to make the desktop shout about a critical battery. No headset
/// moves fifteen points in one polling interval, so a jump that large is either
/// noise or a genuine change that will still be there on the next poll.
const MAX_PLAUSIBLE_STEP: u8 = 15;

/// How close a confirmation has to be to the reading it confirms.
const CONFIRM_TOLERANCE: u8 = 5;

/// How long to wait before the next poll.
///
/// Same shape as razerd's pacing: a battery that just went quiet is asked again
/// shortly, so that it is withdrawn within seconds of the headset being switched
/// off rather than a minute later; otherwise the cadence is whatever the reader
/// that answered can afford.
pub(super) fn next_delay(unanswered: bool, native: bool, config: &Config) -> Duration {
    let regular = if native { NATIVE_TICK } else { config.interval };
    if unanswered {
        UNANSWERED_RETRY.min(regular)
    } else {
        regular
    }
}

/// Starts, keeps or clears the clock of a condition that is `fine` or not on
/// this poll. The clock starts on the first poll that is not fine.
pub(super) fn note(since: &mut Option<Instant>, fine: bool, now: Instant) {
    if fine {
        *since = None;
    } else {
        since.get_or_insert(now);
    }
}

/// Why a virtual battery is being taken down, if it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Withdrawal {
    /// Keep publishing the last known level.
    Keep,
    /// The headset is still detected but has not answered in a long time.
    Silent,
    /// No reader reports the headset at all any more.
    Missing,
}

/// Decides whether a battery still deserves its entry.
///
/// `silent_for` is the time since the last usable level, `missing_for` the
/// time since the headset last appeared in a poll at all.
///
/// A headset that is missing is silent too - nobody answers for it - so the
/// missing grace is judged on its own, and the offline grace only applies to
/// a headset that is still listed. Judging the silence regardless withdrew an
/// unplugged dongle after the *offline* grace, and the longer missing grace
/// never got its say.
pub(super) fn withdrawal(
    silent_for: Duration,
    missing_for: Duration,
    config: &Config,
) -> Withdrawal {
    if missing_for > Duration::ZERO {
        if missing_for > config.missing_grace {
            Withdrawal::Missing
        } else {
            Withdrawal::Keep
        }
    } else if silent_for > config.offline_grace {
        Withdrawal::Silent
    } else {
        Withdrawal::Keep
    }
}

/// The deferred reading, if the one at hand can confirm it.
///
/// A reader that caches the level reports the same reading on every poll until
/// a new one comes in. Held back once, such a reading would otherwise confirm
/// itself a second later: a second opinion has to be a second reading.
pub(super) fn second_opinion(
    deferred: Option<u8>,
    deferred_sample: Option<u64>,
    sample: Option<u64>,
) -> Option<u8> {
    let same_reading = sample.is_some() && sample == deferred_sample;
    deferred.filter(|_| !same_reading)
}

/// What to do with a freshly read battery level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Verdict {
    /// The reading is consistent with what we knew.
    Accept,
    /// The reading is too far off to be trusted on its own.
    Defer,
}

/// Vets `candidate` against the level we last published, and against the
/// reading we deferred on the previous poll, if any.
pub(super) fn vet(published: u8, candidate: u8, deferred: Option<u8>) -> Verdict {
    if published.abs_diff(candidate) <= MAX_PLAUSIBLE_STEP {
        return Verdict::Accept;
    }
    match deferred {
        // The same surprising level twice in a row is a real change: a laptop
        // that slept for a night comes back to a genuinely emptier headset.
        Some(previous) if previous.abs_diff(candidate) <= CONFIRM_TOLERANCE => Verdict::Accept,
        _ => Verdict::Defer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_moves_are_taken_at_face_value() {
        assert_eq!(vet(92, 91, None), Verdict::Accept);
        assert_eq!(vet(92, 92, None), Verdict::Accept);
        assert_eq!(vet(50, 65, None), Verdict::Accept);
        // Exactly at the limit.
        assert_eq!(vet(92, 77, None), Verdict::Accept);
    }

    #[test]
    fn the_glitches_an_audeze_maxwell_actually_produces_are_held_back() {
        // Observed on a real dongle: 92%, 0%, 92% within eighteen seconds.
        assert_eq!(vet(92, 0, None), Verdict::Defer);
        // And 92%, 44%, 92%.
        assert_eq!(vet(92, 44, None), Verdict::Defer);
        // The value that follows the glitch is back in range, so it is taken,
        // and nothing bogus was ever published.
        assert_eq!(vet(92, 92, Some(0)), Verdict::Accept);
    }

    #[test]
    fn a_large_change_confirmed_by_the_next_poll_is_accepted() {
        // A machine that slept all night comes back to an emptier headset:
        // the first reading waits, the second one confirms it.
        assert_eq!(vet(92, 40, None), Verdict::Defer);
        assert_eq!(vet(92, 38, Some(40)), Verdict::Accept);
    }

    #[test]
    fn a_cached_reading_does_not_confirm_itself() {
        // The native reader caches the level and is polled every second. A
        // glitch held back on one poll comes round again on the next: that is
        // the same reading, not a confirmation.
        assert_eq!(second_opinion(Some(0), Some(7), Some(7)), None);
        assert_eq!(
            vet(92, 0, second_opinion(Some(0), Some(7), Some(7))),
            Verdict::Defer
        );
        // A new reading saying the same thing is one.
        assert_eq!(second_opinion(Some(0), Some(7), Some(8)), Some(0));
        assert_eq!(
            vet(92, 0, second_opinion(Some(0), Some(7), Some(8))),
            Verdict::Accept
        );
        // A reader that reads afresh on every poll has no samples to compare:
        // each poll is a new reading.
        assert_eq!(second_opinion(Some(40), None, None), Some(40));
        // Nothing deferred, nothing to confirm.
        assert_eq!(second_opinion(None, None, Some(3)), None);
    }

    #[test]
    fn a_second_unrelated_glitch_does_not_confirm_the_first() {
        assert_eq!(vet(92, 0, None), Verdict::Defer);
        assert_eq!(vet(92, 44, Some(0)), Verdict::Defer);
    }

    #[test]
    fn a_silent_headset_outlives_a_few_retries_and_no_more() {
        let config = Config::default();
        let second = Duration::from_secs(1);

        // One or two lost readings: still listed, keep the entry.
        assert_eq!(
            withdrawal(6 * second, Duration::ZERO, &config),
            Withdrawal::Keep
        );
        // Switched off: the dongle is still there, but nothing answers.
        assert_eq!(
            withdrawal(11 * second, Duration::ZERO, &config),
            Withdrawal::Silent
        );
        // Dongle unplugged: gone a little later, whatever the level clock says.
        assert_eq!(
            withdrawal(Duration::ZERO, 20 * second, &config),
            Withdrawal::Keep
        );
        assert_eq!(
            withdrawal(Duration::ZERO, 31 * second, &config),
            Withdrawal::Missing
        );
    }

    #[test]
    fn the_grace_runs_from_the_first_lost_reading() {
        // The bug this guards against: the grace (10 s) is shorter than the
        // polling interval. Counted from the last answer, the very first lost
        // reading had already outlived it, and the battery was withdrawn on a
        // single miss without one retry.
        let config = Config::default();

        let start = Instant::now();
        let mut silent_since = None;
        let silence =
            |since: Option<Instant>, now: Instant| since.map_or(Duration::ZERO, |s| now - s);

        // Answered at `start`; a whole interval later the first poll goes
        // unanswered. Counted from `start`, the grace would already be over.
        let first_miss = start + config.interval;
        assert!(config.interval > config.offline_grace);
        note(&mut silent_since, false, first_miss);
        assert_eq!(
            withdrawal(silence(silent_since, first_miss), Duration::ZERO, &config),
            Withdrawal::Keep
        );

        // The retries get their chance...
        for retry in 1..=3 {
            let now = first_miss + UNANSWERED_RETRY * retry;
            note(&mut silent_since, false, now);
            assert_eq!(
                withdrawal(silence(silent_since, now), Duration::ZERO, &config),
                Withdrawal::Keep,
                "retry {retry}"
            );
        }
        // ...and only then is the headset given up on.
        let now = first_miss + UNANSWERED_RETRY * 4;
        note(&mut silent_since, false, now);
        assert_eq!(
            withdrawal(silence(silent_since, now), Duration::ZERO, &config),
            Withdrawal::Silent
        );

        // One answer in between resets the clock.
        note(&mut silent_since, true, now);
        assert_eq!(silent_since, None);
    }

    #[test]
    fn the_cadence_follows_what_the_reader_can_afford() {
        let config = Config::default();
        // The native reader is cheap: look often, to notice a power-off quickly.
        assert_eq!(next_delay(false, true, &config), NATIVE_TICK);
        // HeadsetControl is not.
        assert_eq!(next_delay(false, false, &config), Duration::from_secs(60));
        // A battery that just went quiet is looked at again shortly. The native
        // reader is already faster than that.
        assert_eq!(next_delay(true, true, &config), NATIVE_TICK);
        assert_eq!(next_delay(true, false, &config), UNANSWERED_RETRY);

        // Several retries fit in the grace, so one lost reading never flaps it.
        assert!(config.offline_grace >= UNANSWERED_RETRY * 3);
    }
}
