//! The daemon itself: read the batteries, mirror them onto virtual HID
//! batteries, and keep answering the kernel in between.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use log::{debug, error, info, trace, warn};
use rustix::event::{PollFd, PollFlags};

use crate::headset::{BatteryState, Headset};
use crate::source::Source;
use uhid_battery::{Battery, DEV_UHID, Handle, Identity, Kind};

/// Default for [`Config::interval`], in seconds.
pub const DEFAULT_INTERVAL_SECS: u64 = 60;
/// Default for [`Config::native_interval`], in seconds.
pub const DEFAULT_NATIVE_INTERVAL_SECS: u64 = 10;
/// Default for [`Config::offline_grace`], in seconds.
pub const DEFAULT_OFFLINE_GRACE_SECS: u64 = 10;
/// Default for [`Config::missing_grace`], in seconds.
pub const DEFAULT_MISSING_GRACE_SECS: u64 = 30;

/// Once a poll goes unanswered, how soon to ask again. Several retries fit in
/// the grace, so one lost reading never makes the entry flap - and withdrawing
/// is cheap to undo, the battery is back on the first poll that answers.
const UNANSWERED_RETRY: Duration = Duration::from_secs(3);

/// The longest a single `poll()` may block. A signal interrupts `poll()`, so
/// this only bounds the shutdown delay in the unlucky case where the signal
/// lands between checking the stop flag and entering the call.
const MAX_WAIT: Duration = Duration::from_secs(5);

/// First component of the virtual devices' `phys`, which the udev rule that
/// makes UPower report a headset matches on.
pub const PHYS_PREFIX: &str = "headset-battery-indicator";

/// How long the kernel gets to register the power supply after a device is
/// created. It happens synchronously in practice; this is slack.
const REGISTRATION_TIMEOUT: Duration = Duration::from_secs(2);

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

/// Level change worth an INFO line; anything smaller is logged at debug, so a
/// headset hovering between 91% and 92% does not fill the journal.
const LOG_STEP: u8 = 5;

/// What a poll found, coarse enough that it only changes when something worth
/// saying happened.
///
/// Without this the daemon is silent both when the headset is switched off and
/// when it cannot see the dongle at all, which are very different problems.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Summary {
    /// HeadsetControl found no supported device.
    NoDevice,
    /// Devices were found, but none of them reports a battery.
    NoBattery,
    /// A headset is known, but it is not answering battery queries.
    Offline,
    /// At least one headset reported a level.
    Online,
}

impl Summary {
    fn of(headsets: &[Headset]) -> Self {
        let mut with_battery = headsets.iter().filter(|h| h.supports_battery).peekable();
        if headsets.is_empty() {
            Self::NoDevice
        } else if with_battery.peek().is_none() {
            Self::NoBattery
        } else if with_battery.all(|h| h.battery == BatteryState::Unavailable) {
            Self::Offline
        } else {
            Self::Online
        }
    }
}

/// How long to wait before the next poll.
///
/// Same shape as razerd's pacing: a battery that just went quiet is asked again
/// shortly, so that it is withdrawn within seconds of the headset being switched
/// off rather than a minute later; otherwise the cadence is whatever the reader
/// that answered can afford.
fn next_delay(unanswered: bool, native: bool, config: &Config) -> Duration {
    let regular = if native {
        config.native_interval.min(config.interval)
    } else {
        config.interval
    };
    if unanswered {
        UNANSWERED_RETRY.min(regular)
    } else {
        regular
    }
}

/// Why a virtual battery is being taken down, if it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Withdrawal {
    /// Keep publishing the last known level.
    Keep,
    /// The headset is still detected but has not answered in a long time.
    Silent,
    /// HeadsetControl no longer reports the headset at all.
    Missing,
}

/// Decides whether a battery still deserves its entry.
///
/// `silent_for` is the time since the last usable level, `missing_for` the
/// time since the headset last appeared in a poll at all.
fn withdrawal(silent_for: Duration, missing_for: Duration, config: &Config) -> Withdrawal {
    if missing_for > config.missing_grace {
        Withdrawal::Missing
    } else if silent_for > config.offline_grace {
        Withdrawal::Silent
    } else {
        Withdrawal::Keep
    }
}

/// What to do with a freshly read battery level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// The reading is consistent with what we knew.
    Accept,
    /// The reading is too far off to be trusted on its own.
    Defer,
}

/// Vets `candidate` against the level we last published, and against the
/// reading we deferred on the previous poll, if any.
fn vet(published: u8, candidate: u8, deferred: Option<u8>) -> Verdict {
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

/// Runtime configuration of the bridge.
#[derive(Debug, Clone)]
pub struct Config {
    /// Delay between two readings through HeadsetControl, which is expensive:
    /// twenty packets and close to three seconds each.
    pub interval: Duration,
    /// Delay between two readings when the native reader answers. One request,
    /// 70 ms: cheap enough to notice within seconds that the headset was
    /// switched off, or on again.
    pub native_interval: Duration,
    /// How long a headset that is still detected, but no longer answering
    /// battery queries, keeps its entry: long enough for a few retries, so one
    /// lost reading does not make the applet blink, and no longer - the way a
    /// Bluetooth peripheral's battery vanishes on disconnect.
    ///
    /// It used to be a quarter of an hour, on the theory that headsets park
    /// their radio when idle. The gaps that theory explained turned out to be
    /// HeadsetControl misreading the Maxwell; a headset that stops answering a
    /// reader that does not miss has been switched off.
    pub offline_grace: Duration,
    /// How long a headset that is no longer reported at all keeps its entry:
    /// the unplugged dongle, or a reader failing outright. A little longer than
    /// [`Config::offline_grace`], because a failing `headsetcontrol` takes
    /// seconds per attempt.
    pub missing_grace: Duration,
    /// Path of the uhid character device.
    pub uhid_path: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(DEFAULT_INTERVAL_SECS),
            native_interval: Duration::from_secs(DEFAULT_NATIVE_INTERVAL_SECS),
            offline_grace: Duration::from_secs(DEFAULT_OFFLINE_GRACE_SECS),
            missing_grace: Duration::from_secs(DEFAULT_MISSING_GRACE_SECS),
            uhid_path: PathBuf::from(DEV_UHID),
        }
    }
}

/// A virtual battery mirroring one headset.
#[derive(Debug)]
struct VirtualBattery {
    key: String,
    name: String,
    device: Battery,
    /// When the headset last gave us a usable level.
    last_reading: Instant,
    /// When the headset was last reported at all, answering or not.
    last_seen: Instant,
    /// A reading that was too far off to publish, waiting for confirmation.
    deferred: Option<u8>,
    /// The level the last INFO line mentioned.
    logged_percent: u8,
}

impl VirtualBattery {
    /// Pushes a reading to the kernel.
    fn publish(&mut self, percent: u8, charging: bool) -> Result<()> {
        self.device
            .update(percent, charging)
            .with_context(|| format!("publishing the level of {}", self.name))
    }

    /// Answers the kernel, which would otherwise keep whoever is reading the
    /// level from sysfs waiting for five seconds.
    fn service(&mut self) -> Result<()> {
        self.device
            .service()
            .with_context(|| format!("servicing the virtual device of {}", self.name))
    }
}

/// Hands out uhid handles, reusing the ones the service manager passed.
#[derive(Debug)]
struct DevicePool {
    spare: Vec<Handle>,
    path: PathBuf,
    /// Whether opening the device node ourselves is allowed. It is not when
    /// systemd handed us descriptors: the node stays `root:root 0600` and the
    /// daemon has no business reaching for it.
    may_open: bool,
}

impl DevicePool {
    fn new(inherited: Vec<Handle>, path: PathBuf) -> Self {
        let may_open = inherited.is_empty();
        Self {
            spare: inherited,
            path,
            may_open,
        }
    }

    fn acquire(&mut self) -> Result<Handle> {
        if let Some(handle) = self.spare.pop() {
            return Ok(handle);
        }
        anyhow::ensure!(
            self.may_open,
            "out of inherited /dev/uhid descriptors; pass another one with a second \
             OpenFile=/dev/uhid:uhid2 line in the unit file"
        );
        Handle::open(&self.path).with_context(|| format!("opening {}", self.path.display()))
    }

    /// Withdraws a battery and keeps its handle: one passed by the service
    /// manager cannot be reopened, so losing it would leave the daemon unable
    /// to publish anything until it is restarted.
    fn release(&mut self, battery: Battery) {
        self.put_back(battery.destroy());
    }

    fn put_back(&mut self, handle: Handle) {
        self.spare.push(handle);
    }
}

/// The bridge between HeadsetControl and UPower.
#[derive(Debug)]
pub struct Bridge {
    config: Config,
    source: Source,
    pool: DevicePool,
    batteries: Vec<VirtualBattery>,
    probe_failing: bool,
    last_summary: Option<Summary>,
}

impl Bridge {
    /// Builds a bridge from its configuration and any inherited uhid handle.
    #[must_use]
    pub fn new(config: Config, source: Source, inherited: Vec<Handle>) -> Self {
        let pool = DevicePool::new(inherited, config.uhid_path.clone());
        Self {
            config,
            source,
            pool,
            batteries: Vec::new(),
            probe_failing: false,
            last_summary: None,
        }
    }

    /// Runs until `stop` is raised, then withdraws every virtual battery.
    ///
    /// # Errors
    ///
    /// Only unrecoverable failures propagate; a headset that disappears or a
    /// failing reader is logged and retried on the next tick.
    pub fn run(&mut self, stop: &AtomicBool) -> Result<()> {
        info!(
            "reading batteries with {} (every {:?} natively, {:?} otherwise)",
            self.source.describe(),
            self.config.native_interval,
            self.config.interval
        );

        while !stop.load(Ordering::Relaxed) {
            let unanswered = self.tick();
            let delay = next_delay(
                unanswered,
                self.source.last_probe_was_native(),
                &self.config,
            );

            let deadline = Instant::now() + delay;
            while !stop.load(Ordering::Relaxed) {
                let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                    break;
                };
                self.wait(remaining.min(MAX_WAIT))?;
            }
        }

        self.shutdown();
        Ok(())
    }

    /// Reads the batteries once and reconciles the virtual devices with them.
    ///
    /// Returns whether a published battery went unanswered, which calls for a
    /// prompt retry rather than the usual wait.
    fn tick(&mut self) -> bool {
        let headsets = match self.source.probe() {
            Ok(headsets) => {
                if self.probe_failing {
                    info!("battery readings are back");
                    self.probe_failing = false;
                }
                self.announce(Summary::of(&headsets), &headsets);
                headsets
            }
            Err(err) => {
                // Only shout about the first failure of a streak: a dongle that
                // is unplugged for a week should not fill the journal.
                if self.probe_failing {
                    debug!("still failing: {err:#}");
                } else {
                    warn!("could not read batteries: {err:#}");
                    self.probe_failing = true;
                }
                Vec::new()
            }
        };

        let now = Instant::now();
        for headset in &headsets {
            if !headset.supports_battery {
                trace!("{} does not report a battery, skipping", headset.name);
                continue;
            }
            if let Err(err) = self.apply(headset, now) {
                error!("{}: {err:#}", headset.name);
            }
        }

        self.expire(now);
        self.batteries
            .iter()
            .any(|battery| battery.last_reading != now)
    }

    /// Says what the poll found, but only when that changed since last time.
    fn announce(&mut self, summary: Summary, headsets: &[Headset]) {
        if self.last_summary == Some(summary) {
            return;
        }
        self.last_summary = Some(summary);

        let names = || {
            headsets
                .iter()
                .map(|headset| headset.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };

        match summary {
            Summary::NoDevice => info!(
                "no supported headset found: check that the dongle is plugged in, and that this \
                 service may reach its hidraw node (run `headset-battery-indicator udev-rules` \
                 and install the result)"
            ),
            Summary::NoBattery => info!("{} found, but none reports a battery", names()),
            Summary::Offline => info!(
                "{} is detected but not answering battery queries (switched off?); keeping its \
                 entry for a few more polls",
                names()
            ),
            // Nothing to say: the level lines speak for themselves.
            Summary::Online => debug!("{} is answering", names()),
        }
    }

    /// Creates or updates the virtual battery backing `headset`.
    fn apply(&mut self, headset: &Headset, now: Instant) -> Result<()> {
        let key = headset.key();
        let existing = self.batteries.iter().position(|b| b.key == key);

        // Being listed at all is what keeps the entry alive; answering is what
        // refreshes the level.
        if let Some(index) = existing {
            self.batteries[index].last_seen = now;
        }

        let (percent, charging) = match headset.battery {
            BatteryState::Discharging(percent) => (percent, false),
            BatteryState::Charging(Some(percent)) => (percent, true),
            // A headset that reports charging without a level keeps whatever
            // we last knew; there is nothing better to show, and dropping the
            // device would make it blink out of the applet.
            BatteryState::Charging(None) => {
                let Some(index) = existing else {
                    debug!(
                        "{} is charging but has not reported a level yet",
                        headset.name
                    );
                    return Ok(());
                };
                (self.batteries[index].device.percent(), true)
            }
            BatteryState::Unavailable => {
                trace!("{} is offline", headset.name);
                return Ok(());
            }
        };

        let Some(index) = existing else {
            return self.attach(headset, percent, charging, now);
        };

        let battery = &mut self.batteries[index];
        // The headset answered, so it is alive even if we end up distrusting
        // the level it gave us.
        battery.last_reading = now;

        let (known_percent, known_charging) = (battery.device.percent(), battery.device.charging());
        if vet(known_percent, percent, battery.deferred) == Verdict::Defer {
            debug!(
                "{}: holding back an implausible {percent}% (last known {known_percent}%)",
                battery.name
            );
            battery.deferred = Some(percent);
            return Ok(());
        }
        battery.deferred = None;

        // An unchanged level is pushed all the same. The kernel drops input
        // reports without telling anyone while it is probing the device, so
        // this periodic push is what guarantees a lost one is made up for; it
        // rate-limits the resulting uevents itself.
        if known_percent != percent || known_charging != charging {
            let suffix = if charging { ", charging" } else { "" };
            if known_charging != charging || battery.logged_percent.abs_diff(percent) >= LOG_STEP {
                battery.logged_percent = percent;
                info!("{}: {percent}%{suffix}", battery.name);
            } else {
                debug!("{}: {percent}%{suffix}", battery.name);
            }
        }
        battery.publish(percent, charging)
    }

    /// Registers a new virtual battery with the kernel.
    fn attach(
        &mut self,
        headset: &Headset,
        percent: u8,
        charging: bool,
        now: Instant,
    ) -> Result<()> {
        let uniq = headset.uniq();
        let identity = Identity {
            name: headset.name.clone(),
            phys: format!("{PHYS_PREFIX}/{uniq}"),
            uniq: uniq.clone(),
            vendor: u32::from(headset.vendor_id),
            product: u32::from(headset.product_id),
        };

        let handle = self.pool.acquire()?;
        let mut device = match Battery::create(handle, &identity, Kind::Headset, percent, charging)
        {
            Ok(device) => device,
            Err(err) => {
                let (handle, source) = err.into_parts();
                self.pool.put_back(handle);
                return Err(source).context("creating the virtual HID battery");
            }
        };

        // Creating the HID device is not the goal; the power supply is. The
        // kernel accepts a device it then builds no battery for without a
        // word, so look for the result instead of assuming it.
        let sysfs = match device.wait_for_power_supply(REGISTRATION_TIMEOUT) {
            Ok(Some(sysfs)) => sysfs,
            Ok(None) => {
                self.pool.release(device);
                anyhow::bail!(
                    "the kernel created the HID device but no hid-{uniq}-battery power supply; \
                     is CONFIG_HID_BATTERY_STRENGTH enabled? (see `journalctl -k`)"
                );
            }
            Err(err) => {
                self.pool.release(device);
                return Err(err).context("waiting for the power supply");
            }
        };

        info!(
            "{} appeared: {percent}%{} ({})",
            headset.name,
            if charging { ", charging" } else { "" },
            sysfs.display()
        );
        self.batteries.push(VirtualBattery {
            key: headset.key(),
            name: headset.name.clone(),
            device,
            last_reading: now,
            last_seen: now,
            deferred: None,
            logged_percent: percent,
        });
        Ok(())
    }

    /// Withdraws batteries that have outlived their grace, saying which one.
    fn expire(&mut self, now: Instant) {
        let mut index = 0;
        while index < self.batteries.len() {
            let battery = &self.batteries[index];
            let verdict = withdrawal(
                now.duration_since(battery.last_reading),
                now.duration_since(battery.last_seen),
                &self.config,
            );

            let reason = match verdict {
                Withdrawal::Keep => {
                    index += 1;
                    continue;
                }
                Withdrawal::Silent => "has not reported a level in a long while",
                Withdrawal::Missing => "is no longer detected",
            };

            let battery = self.batteries.remove(index);
            info!("{} {reason}, removing its battery", battery.name);
            self.pool.release(battery.device);
        }
    }

    /// Waits for uhid events, for at most `timeout`, then services every
    /// battery.
    ///
    /// With no battery to watch this is a plain sleep, but still through
    /// `poll()`: unlike `thread::sleep`, it returns when a signal arrives.
    fn wait(&mut self, timeout: Duration) -> Result<()> {
        // Wake up early only if a battery has a push of its own coming up.
        let now = Instant::now();
        let timeout = self
            .batteries
            .iter()
            .filter_map(|battery| battery.device.next_deadline())
            .map(|due| due.saturating_duration_since(now))
            .fold(timeout, Duration::min);

        let mut fds: Vec<PollFd<'_>> = self
            .batteries
            .iter()
            .map(|battery| PollFd::new(&battery.device, PollFlags::IN))
            .collect();
        match crate::poll::poll(&mut fds, timeout) {
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => return Ok(()),
            Err(err) => return Err(err).context("waiting on /dev/uhid"),
        }
        drop(fds);

        // Servicing is a non-blocking read, so there is no point in working
        // out which descriptor was ready - and a due push needs it regardless.
        for battery in &mut self.batteries {
            if let Err(err) = battery.service() {
                error!("{err:#}");
            }
        }
        Ok(())
    }

    /// Removes every virtual battery, so the applet does not keep a stale entry
    /// around while the daemon is restarting.
    fn shutdown(&mut self) {
        for battery in self.batteries.drain(..) {
            debug!("withdrawing {}", battery.name);
            drop(battery.device.destroy());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headset(name: &str, supports_battery: bool, battery: BatteryState) -> Headset {
        Headset {
            name: name.to_owned(),
            product: format!("{name} dongle"),
            vendor_id: 0x3329,
            product_id: 0x4b18,
            supports_battery,
            battery,
        }
    }

    #[test]
    fn an_empty_poll_is_told_apart_from_a_sleeping_headset() {
        // Nothing found at all: the dongle is unplugged, or the service cannot
        // reach its hidraw node. Those need a very different fix from a
        // headset that is merely switched off.
        assert_eq!(Summary::of(&[]), Summary::NoDevice);

        let off = headset("Audeze Maxwell", true, BatteryState::Unavailable);
        assert_eq!(Summary::of(std::slice::from_ref(&off)), Summary::Offline);

        let no_battery = headset("Some Dongle", false, BatteryState::Unavailable);
        assert_eq!(Summary::of(&[no_battery]), Summary::NoBattery);

        let on = headset("Audeze Maxwell", true, BatteryState::Discharging(92));
        assert_eq!(Summary::of(std::slice::from_ref(&on)), Summary::Online);

        // One headset answering is enough to call the whole poll online.
        assert_eq!(Summary::of(&[off, on]), Summary::Online);
    }

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
    fn the_cadence_follows_what_the_reader_can_afford() {
        let config = Config::default();
        // The native reader is cheap: look often, to notice a power-off quickly.
        assert_eq!(next_delay(false, true, &config), Duration::from_secs(10));
        // HeadsetControl is not.
        assert_eq!(next_delay(false, false, &config), Duration::from_secs(60));
        // A battery that just went quiet is asked again at once, either way.
        assert_eq!(next_delay(true, true, &config), UNANSWERED_RETRY);
        assert_eq!(next_delay(true, false, &config), UNANSWERED_RETRY);

        // Several retries fit in the grace, so one lost reading never flaps it.
        assert!(config.offline_grace >= UNANSWERED_RETRY * 3);

        // A user asking for a slow cadence gets it for both readers.
        let lazy = Config {
            interval: Duration::from_secs(5),
            ..Config::default()
        };
        assert_eq!(next_delay(false, true, &lazy), Duration::from_secs(5));
    }

    #[test]
    fn default_config_is_conservative() {
        let config = Config::default();
        assert_eq!(config.interval, Duration::from_secs(60));
        assert!(config.native_interval < config.interval);
        assert!(config.missing_grace > config.offline_grace);
        assert_eq!(config.uhid_path, PathBuf::from("/dev/uhid"));
    }
}
