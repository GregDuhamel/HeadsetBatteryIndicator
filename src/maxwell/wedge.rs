//! Telling a wedged dongle from a headset that is switched off.
//!
//! From the HID side the two are the same: nothing comes back. The difference
//! is on the audio side. The dongle's USB audio interfaces - and with them its
//! ALSA card - only exist while a headset is linked, and a PCM of that card
//! in `RUNNING` state means sound is being streamed to a headset that is
//! there. A dongle that streams and still answers nothing is wedged. The
//! [module documentation](super) tells the incident that taught this.
//!
//! Two parts: [`playback_running`], the evidence, a pure read of sysfs and
//! `/proc/asound` with both roots given so that a fake tree can stand in; and
//! [`WedgeWatch`], the decision - how long the silence has to last, how often
//! the evidence is looked at, and when to say so again.

use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use log::debug;

use super::session::ASK_IDLE_RETRY;

/// How long a dongle has to say nothing before it is suspected. It matches
/// the slow cadence of the session: a session that has heard nothing is asked
/// again every ten minutes (`ASK_IDLE_RETRY`), so by the time the silence has
/// lasted this long at least one request went out, and a dongle with a
/// linked headset - which the audio proves - would have answered it.
pub(super) const WEDGE_SILENCE: Duration = Duration::from_secs(600);

// The reasoning above holds only while the silence outlasts the slow cadence.
const _: () = assert!(WEDGE_SILENCE.as_secs() >= ASK_IDLE_RETRY.as_secs());

/// How long between two warnings about the same wedge. The remedy is a hand
/// on the dongle; once a day is enough to be noticed without flooding the
/// journal for the two weeks the first incident went unseen.
pub(super) const WEDGE_REMINDER: Duration = Duration::from_secs(24 * 3600);

/// How often the ALSA state is read while a wedge is suspected. The reader
/// runs once a second; a few file reads every half minute is nothing, every
/// second would be a habit.
const PROBE_EVERY: Duration = Duration::from_secs(30);

/// Whether the dongle's sound card has a playback stream running.
///
/// `usb_device` is the dongle's USB device directory in sysfs (`1-5`, say):
/// its interfaces are `1-5:1.N`, and the one bound to `snd-usb-audio` holds
/// `sound/cardN`. The card's playback PCMs are then `/proc/asound/cardN/
/// pcm*p`, each with a `sub0/status` that reads `closed` at rest and starts
/// with `state: RUNNING` while sound plays.
///
/// `Some(true)` when any playback PCM is running, `Some(false)` when the
/// card is there and none is, and `None` when the device has no sound card
/// (the interfaces go with the headset's link) or its PCMs cannot be read,
/// which says nothing either way.
pub(super) fn playback_running(proc_asound: &Path, usb_device: &Path) -> Option<bool> {
    let card = sound_card(usb_device)?;
    let card_dir = proc_asound.join(&card);
    let pcms = match fs::read_dir(&card_dir) {
        Ok(pcms) => pcms,
        Err(err) => {
            debug!("cannot list {}: {err}", card_dir.display());
            return None;
        }
    };
    let running = pcms
        .filter_map(Result::ok)
        .filter(|pcm| {
            let name = pcm.file_name();
            let name = name.to_string_lossy();
            name.starts_with("pcm") && name.ends_with('p')
        })
        .any(|pcm| {
            fs::read_to_string(pcm.path().join("sub0/status"))
                .unwrap_or_default()
                .lines()
                .any(|line| line.trim() == "state: RUNNING")
        });
    Some(running)
}

/// The ALSA card directory name (`cardN`) under one of the USB device's
/// interfaces, if any has one.
fn sound_card(usb_device: &Path) -> Option<OsString> {
    fs::read_dir(usb_device)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|interface| fs::read_dir(interface.path().join("sound")).ok())
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .find(|name| name.to_string_lossy().starts_with("card"))
}

/// The silence being watched, and whether it is worth a line yet.
///
/// One per reader, not per session: the dongle re-enumerates at every link
/// change, which opens a new session, and the silence is the dongle's, not
/// the session's. Only the decisions live here; the logging is the reader's,
/// so that this can be tested with instants of the test's choosing.
#[derive(Debug)]
pub(super) struct WedgeWatch {
    /// When a dongle last said anything, or when the watch began.
    heard_at: Instant,
    /// When the wedge was last reported, if it was.
    warned_at: Option<Instant>,
    /// The last look at the ALSA state, and when it was taken.
    probed: Option<(Instant, Option<bool>)>,
}

impl WedgeWatch {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            heard_at: now,
            warned_at: None,
            probed: None,
        }
    }

    /// Takes in that a dongle said something, and says whether the recovery
    /// is worth a line: only if the silence was reported.
    pub(super) fn heard(&mut self, now: Instant) -> bool {
        self.heard_at = now;
        self.probed = None;
        self.warned_at.take().is_some()
    }

    /// How long nothing has been heard.
    pub(super) fn silence(&self, now: Instant) -> Duration {
        now.duration_since(self.heard_at)
    }

    /// Takes in a pass over a dongle whose headset is not known to be linked,
    /// and says whether a warning is due now, with the silence to quote.
    ///
    /// `probe` reads the ALSA state ([`playback_running`]); it is only called
    /// once the silence has lasted [`WEDGE_SILENCE`], and then at most every
    /// [`PROBE_EVERY`]. A warning is due the first time the dongle is found
    /// streaming, and again every [`WEDGE_REMINDER`] while it stays silent.
    pub(super) fn check(
        &mut self,
        now: Instant,
        probe: impl FnOnce() -> Option<bool>,
    ) -> Option<Duration> {
        let silence = self.silence(now);
        if silence < WEDGE_SILENCE {
            return None;
        }
        let running = match self.probed {
            Some((at, result)) if now.duration_since(at) < PROBE_EVERY => result,
            _ => {
                let result = probe();
                self.probed = Some((now, result));
                result
            }
        };
        if running != Some(true) {
            return None;
        }
        let due = self
            .warned_at
            .is_none_or(|at| now.duration_since(at) >= WEDGE_REMINDER);
        if due {
            self.warned_at = Some(now);
            Some(silence)
        } else {
            None
        }
    }
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
    use std::cell::Cell;
    use std::path::PathBuf;

    use super::*;

    /// A fake pair of trees: the dongle's USB device `1-5` with a sound card
    /// on its second interface, and `/proc/asound` next to it. Returns the
    /// two roots and the device directory.
    fn trees(card: Option<&str>) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let proc_asound = tmp.path().join("proc");
        let usb_device = tmp.path().join("sys/1-5");
        fs::create_dir_all(usb_device.join("1-5:1.0")).unwrap();
        fs::write(usb_device.join("idVendor"), "3329\n").unwrap();
        fs::create_dir_all(&proc_asound).unwrap();
        if let Some(card) = card {
            fs::create_dir_all(usb_device.join("1-5:1.1/sound").join(card)).unwrap();
        }
        (tmp, proc_asound, usb_device)
    }

    fn pcm(proc_asound: &Path, card: &str, pcm: &str, status: &str) {
        let dir = proc_asound.join(card).join(pcm).join("sub0");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("status"), status).unwrap();
    }

    const RUNNING: &str = "state: RUNNING\nowner_pid   : 2170\ntrigger_time: 1.2\n";

    /// A second short of `duration`.
    fn shy(duration: Duration) -> Duration {
        Duration::from_secs(duration.as_secs() - 1)
    }

    #[test]
    fn a_running_playback_pcm_is_the_evidence() {
        let (_tmp, proc_asound, usb_device) = trees(Some("card3"));

        // The card, nothing playing.
        pcm(&proc_asound, "card3", "pcm0p", "closed\n");
        pcm(&proc_asound, "card3", "pcm0c", "closed\n");
        assert_eq!(playback_running(&proc_asound, &usb_device), Some(false));

        // Sound plays.
        pcm(&proc_asound, "card3", "pcm0p", RUNNING);
        assert_eq!(playback_running(&proc_asound, &usb_device), Some(true));
    }

    #[test]
    fn any_playback_pcm_counts_but_capture_does_not() {
        let (_tmp, proc_asound, usb_device) = trees(Some("card3"));
        pcm(&proc_asound, "card3", "pcm0p", "closed\n");
        pcm(&proc_asound, "card3", "pcm1p", RUNNING);
        pcm(&proc_asound, "card3", "pcm0c", "closed\n");
        assert_eq!(playback_running(&proc_asound, &usb_device), Some(true));

        // The microphone alone is not the headset listening.
        pcm(&proc_asound, "card3", "pcm1p", "closed\n");
        pcm(&proc_asound, "card3", "pcm0c", RUNNING);
        assert_eq!(playback_running(&proc_asound, &usb_device), Some(false));
    }

    #[test]
    fn no_card_says_nothing_either_way() {
        // The dongle's audio interfaces go with the headset's link: without
        // them there is no card, and nothing to conclude.
        let (_tmp, proc_asound, usb_device) = trees(None);
        pcm(&proc_asound, "card3", "pcm0p", RUNNING);
        assert_eq!(playback_running(&proc_asound, &usb_device), None);
        assert_eq!(
            playback_running(&proc_asound, Path::new("/nonexistent/1-5")),
            None
        );

        // A card that sysfs lists but /proc does not show (`ProcSubset=pid`
        // hides /proc/asound) is not evidence of anything.
        let (_tmp, proc_asound, usb_device) = trees(Some("card3"));
        assert_eq!(playback_running(&proc_asound, &usb_device), None);
    }

    #[test]
    fn the_silence_is_reported_once_it_has_lasted_then_once_a_day() {
        let start = Instant::now();
        let mut watch = WedgeWatch::new(start);
        let probes = Cell::new(0);
        let streaming = || {
            probes.set(probes.get() + 1);
            Some(true)
        };

        // Under ten minutes nothing is suspected, and ALSA is not even read.
        assert_eq!(watch.check(start, streaming), None);
        assert_eq!(watch.check(start + shy(WEDGE_SILENCE), streaming), None);
        assert_eq!(probes.get(), 0);

        // Ten minutes of silence with sound playing: said, once.
        assert_eq!(
            watch.check(start + WEDGE_SILENCE, streaming),
            Some(WEDGE_SILENCE)
        );
        assert_eq!(probes.get(), 1);
        assert_eq!(
            watch.check(start + WEDGE_SILENCE + Duration::from_secs(1), streaming),
            None
        );
        assert_eq!(
            watch.check(start + WEDGE_SILENCE + Duration::from_secs(3600), streaming),
            None
        );

        // Said again a day later, with the silence it has grown to.
        assert_eq!(
            watch.check(start + WEDGE_SILENCE + shy(WEDGE_REMINDER), streaming),
            None
        );
        assert_eq!(
            watch.check(start + WEDGE_SILENCE + WEDGE_REMINDER, streaming),
            Some(WEDGE_SILENCE + WEDGE_REMINDER)
        );
    }

    #[test]
    fn the_evidence_is_read_every_half_minute_at_most() {
        let start = Instant::now();
        let mut watch = WedgeWatch::new(start);
        let probes = Cell::new(0);
        let quiet = || {
            probes.set(probes.get() + 1);
            Some(false)
        };

        // Once suspected, the reader passes every second; ALSA is read on
        // the first pass and then every PROBE_EVERY.
        let suspect = start + WEDGE_SILENCE;
        for second in 0..90 {
            assert_eq!(
                watch.check(suspect + Duration::from_secs(second), quiet),
                None
            );
        }
        assert_eq!(probes.get(), 3);
    }

    #[test]
    fn a_silent_dongle_without_sound_is_a_headset_that_is_off() {
        let start = Instant::now();
        let mut watch = WedgeWatch::new(start);
        let suspect = start + WEDGE_SILENCE;

        // No card: nothing to say, however long it lasts.
        assert_eq!(watch.check(suspect, || None), None);
        assert_eq!(
            watch.check(suspect + Duration::from_secs(7 * 86_400), || None),
            None
        );
        // Nor with a card that is idle.
        assert_eq!(
            watch.check(suspect + Duration::from_secs(8 * 86_400), || Some(false)),
            None
        );
        // And the dongle speaking was not a recovery, since nothing was said.
        assert!(!watch.heard(suspect + Duration::from_secs(9 * 86_400)));
    }

    #[test]
    fn the_dongle_speaking_again_is_a_recovery_and_restarts_the_clock() {
        let start = Instant::now();
        let mut watch = WedgeWatch::new(start);
        let suspect = start + WEDGE_SILENCE;
        assert_eq!(watch.check(suspect, || Some(true)), Some(WEDGE_SILENCE));

        // The dongle was replugged and announces the link: worth a line, one.
        let replugged = suspect + Duration::from_secs(3 * 3600);
        assert!(watch.heard(replugged));
        // Said once; and every message restarts the silence.
        let last = replugged + Duration::from_secs(1);
        assert!(!watch.heard(last));
        assert_eq!(
            watch.silence(last + Duration::from_secs(5)),
            Duration::from_secs(5)
        );

        // Wedged again later: a fresh silence, a fresh warning after ten
        // minutes - not a reminder a day away.
        assert_eq!(watch.check(last + shy(WEDGE_SILENCE), || Some(true)), None);
        assert_eq!(
            watch.check(last + WEDGE_SILENCE, || Some(true)),
            Some(WEDGE_SILENCE)
        );
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
