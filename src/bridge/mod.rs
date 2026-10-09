//! The daemon itself: read the batteries, mirror them onto virtual HID
//! batteries, and keep answering the kernel in between.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use log::{debug, error, info, trace, warn};

use crate::headset::{BatteryState, Headset};
use crate::notify::Notifier;
use crate::source::BatterySource;
use uhid_battery::{
    Battery, CreateError, CreateErrorKind, DEV_UHID, Handle, Identity, Kind, Reading, Wakeup,
    serve_all,
};

mod batteries;
mod policy;
#[cfg(test)]
mod tests;

use batteries::{Batteries, VirtualBattery};
use policy::{NATIVE_TICK, Verdict, Withdrawal, next_delay, note, second_opinion, vet, withdrawal};

/// Default for [`Config::interval`], in seconds.
pub const DEFAULT_INTERVAL_SECS: u64 = 60;

/// How long to wait before trying again to create a virtual battery that could
/// not be created, or that was given up on (see [`DEVICE_FAILURE_LIMIT`]). The
/// native reader comes round every second; without this a lasting failure -
/// no uhid descriptor left, a kernel without HID battery support - would
/// create and destroy a device, and log an error, every second for as long as
/// it lasts.
const ATTACH_RETRY: Duration = Duration::from_secs(60);

/// How many times in a row talking to a virtual device - pushing a level, or
/// answering the kernel - may fail before the device is destroyed and created
/// afresh, through the attach path and its retry. One failure can be the
/// kernel still probing the device; a streak means the device is broken, and
/// an error that is never acted on would otherwise be logged on every tick
/// for ever. Three, so that a transient failure never costs a recreation,
/// while a lasting one is acted on within a few ticks - or at once, when it
/// is servicing that fails (see [`Bridge::wait`]).
const DEVICE_FAILURE_LIMIT: u32 = 3;

/// How long the dongle has to keep saying the headset is gone before it is
/// believed. The link drops for a second while a freshly powered headset
/// settles, and the entry should not blink out and back for that.
const DISCONNECT_SETTLE: Duration = Duration::from_secs(3);

/// How long a new overall state has to last before it is worth a log line.
const SUMMARY_SETTLE: Duration = Duration::from_secs(5);

/// An unchanged level is pushed again this often, as a safety net for a push
/// the kernel dropped while it was probing the device.
const REPUBLISH_EVERY: Duration = Duration::from_secs(60);
/// Default for [`Config::offline_grace`], in seconds.
pub const DEFAULT_OFFLINE_GRACE_SECS: u64 = 10;
/// Default for [`Config::missing_grace`], in seconds.
pub const DEFAULT_MISSING_GRACE_SECS: u64 = 30;

/// The longest a single [`serve_all`] may block. A signal interrupts the
/// `poll()` underneath, so this only bounds the shutdown delay in the unlucky
/// case where the signal lands between checking the stop flag and entering
/// the call.
///
/// It also paces the watchdog: the service manager is pinged after every
/// wait, so the pings are never further apart than this plus one tick - a
/// HeadsetControl reading, which is cut off at `--timeout` (10 s by
/// default), plus the [`REGISTRATION_TIMEOUT`] of a device being created.
/// `WatchdogSec=` in the unit has to leave room for that worst case; see
/// [`WATCHDOG_WORST_CASE`].
const MAX_WAIT: Duration = Duration::from_secs(5);

/// The longest the loop can go without pinging the watchdog, with the unit's
/// `--timeout` (10 s) for HeadsetControl: a wait, a reading, a device
/// registration. `WatchdogSec=` in the shipped unit is 60 s, over three
/// times this, so that a machine that is merely slow is not restarted.
pub const WATCHDOG_WORST_CASE: Duration = Duration::from_secs(5 + 10 + 2);

/// First component of the virtual devices' `phys`, which the udev rule that
/// makes UPower report a headset matches on.
pub const PHYS_PREFIX: &str = "headset-battery-indicator";

/// How long the kernel gets to register the power supply after a device is
/// created. It happens synchronously in practice; this is slack.
const REGISTRATION_TIMEOUT: Duration = Duration::from_secs(2);

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
    /// No reader found a supported device.
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
        } else if with_battery.all(|h| {
            matches!(
                h.battery,
                BatteryState::Unavailable | BatteryState::Disconnected
            )
        }) {
            Self::Offline
        } else {
            Self::Online
        }
    }
}

/// Runtime configuration of the bridge.
#[derive(Debug, Clone)]
pub struct Config {
    /// Delay between two readings through HeadsetControl, which is expensive:
    /// twenty packets and close to three seconds each.
    pub interval: Duration,
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
            offline_grace: Duration::from_secs(DEFAULT_OFFLINE_GRACE_SECS),
            missing_grace: Duration::from_secs(DEFAULT_MISSING_GRACE_SECS),
            uhid_path: PathBuf::from(DEV_UHID),
        }
    }
}

/// Why a virtual battery could not be created.
#[derive(Debug)]
enum AttachFailure {
    /// The kernel said no this time - a device still being torn down on the
    /// handle, no descriptor left - or the power supply never showed up. Worth
    /// another try after [`ATTACH_RETRY`].
    Retry(anyhow::Error),
    /// The identity itself was refused, before anything reached the kernel.
    /// It is built from the vendor and product IDs and the model name, none
    /// of which will change: trying again every minute would only log the
    /// same error every minute, for as long as the daemon runs.
    GiveUp(anyhow::Error),
}

/// Whatever `?` propagates out of the attach path is worth a retry; the one
/// failure that is not is sorted out by hand.
impl From<anyhow::Error> for AttachFailure {
    fn from(err: anyhow::Error) -> Self {
        Self::Retry(err)
    }
}

/// The uhid side of the bridge: where handles on `/dev/uhid` come from, and
/// the kernel behind them that builds a power supply out of what is written
/// to one.
///
/// [`DevicePool`] in the daemon. The unit tests put a fake kernel behind it -
/// a socket pair standing in for the node - and that is the only seam: the
/// [`Battery`] values the bridge drives are the real ones, down to the
/// events they write. The two things a fake kernel cannot do are what the
/// trait abstracts: hand out a descriptor on the real node, and list a power
/// supply under sysfs.
pub trait Uhid {
    /// A handle to create the next device on.
    ///
    /// # Errors
    ///
    /// Fails if no handle is to be had: every inherited descriptor is in use
    /// and the node may not be opened, or opening it failed.
    fn acquire(&mut self) -> Result<Handle>;

    /// Keeps a handle for a later device.
    fn put_back(&mut self, handle: Handle);

    /// Creates a device on `handle`: [`Battery::create`], which hands the
    /// handle back in the error.
    ///
    /// # Errors
    ///
    /// As [`Battery::create`].
    fn create(
        &mut self,
        handle: Handle,
        identity: &Identity,
        kind: Kind,
        reading: Reading,
    ) -> Result<Battery, CreateError>;

    /// Services `device` until the kernel has registered its power supply,
    /// and says where; `None` when that did not happen within `timeout`.
    ///
    /// # Errors
    ///
    /// Fails if servicing the device fails.
    fn wait_for_power_supply(
        &mut self,
        device: &mut Battery,
        timeout: Duration,
    ) -> io::Result<Option<PathBuf>>;

    /// Withdraws a battery and keeps its handle: one passed by the service
    /// manager cannot be reopened, so losing it would leave the daemon unable
    /// to publish anything until it is restarted.
    fn release(&mut self, battery: Battery) {
        self.put_back(battery.destroy());
    }
}

/// Hands out uhid handles, reusing the ones the service manager passed.
#[derive(Debug)]
pub struct DevicePool {
    spare: Vec<Handle>,
    path: PathBuf,
    /// Whether opening the device node ourselves is allowed. It is not when
    /// systemd handed us descriptors: the node stays `root:root 0600` and the
    /// daemon has no business reaching for it.
    may_open: bool,
}

impl DevicePool {
    /// A pool over the inherited handles, or over the node at `path` when
    /// there are none.
    #[must_use]
    pub fn new(inherited: Vec<Handle>, path: PathBuf) -> Self {
        let may_open = inherited.is_empty();
        Self {
            spare: inherited,
            path,
            may_open,
        }
    }
}

impl Uhid for DevicePool {
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

    fn put_back(&mut self, handle: Handle) {
        self.spare.push(handle);
    }

    fn create(
        &mut self,
        handle: Handle,
        identity: &Identity,
        kind: Kind,
        reading: Reading,
    ) -> Result<Battery, CreateError> {
        Battery::create(handle, identity, kind, reading)
    }

    fn wait_for_power_supply(
        &mut self,
        device: &mut Battery,
        timeout: Duration,
    ) -> io::Result<Option<PathBuf>> {
        device.wait_for_power_supply(timeout)
    }
}

/// The bridge between the battery readers and UPower.
///
/// Generic over where the readings come from and where the devices go, so
/// that the unit tests can script the one and fake the other; the daemon
/// runs it over [`crate::source::Source`] and [`DevicePool`].
#[derive(Debug)]
pub struct Bridge<S, U = DevicePool> {
    config: Config,
    source: S,
    uhid: U,
    batteries: Batteries,
    probe_failing: bool,
    /// Headsets whose battery could not be created, and when to try again.
    attach_retry_at: BTreeMap<String, Instant>,
    /// Headsets whose identity the kernel can never accept
    /// ([`AttachFailure::GiveUp`]): said once, then never tried again.
    abandoned: BTreeSet<String>,
    last_summary: Option<Summary>,
    /// A summary that differs from the announced one, and since when.
    unsettled: Option<(Summary, Instant)>,
}

impl<S: BatterySource> Bridge<S, DevicePool> {
    /// Builds a bridge from its configuration and any inherited uhid handle.
    #[must_use]
    pub fn new(config: Config, source: S, inherited: Vec<Handle>) -> Self {
        let pool = DevicePool::new(inherited, config.uhid_path.clone());
        Self::over(config, source, pool)
    }
}

impl<S: BatterySource, U: Uhid> Bridge<S, U> {
    /// A bridge over any source of readings and any uhid.
    #[must_use]
    pub fn over(config: Config, source: S, uhid: U) -> Self {
        Self {
            config,
            source,
            uhid,
            batteries: Batteries::default(),
            probe_failing: false,
            attach_retry_at: BTreeMap::new(),
            abandoned: BTreeSet::new(),
            last_summary: None,
            unsettled: None,
        }
    }

    /// Runs until `stop` is raised, then withdraws every virtual battery.
    ///
    /// The service manager hears `READY=1` once the loop is about to start,
    /// `WATCHDOG=1` on every turn of it - at least every `MAX_WAIT` plus a
    /// tick, see [`WATCHDOG_WORST_CASE`] - and `STOPPING=1` before the
    /// batteries are withdrawn.
    ///
    /// # Errors
    ///
    /// Only unrecoverable failures propagate; a headset that disappears or a
    /// failing reader is logged and retried on the next tick.
    pub fn run(&mut self, stop: &AtomicBool, notify: &Notifier) -> Result<()> {
        info!(
            "reading batteries with {}; passes every {:?} natively, every {:?} otherwise",
            self.source.describe(),
            NATIVE_TICK,
            self.config.interval
        );
        notify.ready();

        while !stop.load(Ordering::Relaxed) {
            notify.watchdog();
            let unanswered = self.tick(stop, Instant::now());
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
                notify.watchdog();
            }
        }

        notify.stopping();
        self.shutdown();
        Ok(())
    }

    /// Reads the batteries once and reconciles the virtual devices with them.
    ///
    /// `now` is when the poll started, and stamps every clock this tick
    /// touches: a HeadsetControl reading takes seconds, and one instant for
    /// the whole tick is what lets "answered on this tick" be an equality.
    /// The unit tests pass the instants they please, and never sleep.
    ///
    /// Returns whether a published battery went unanswered, which calls for a
    /// prompt retry rather than the usual wait.
    fn tick(&mut self, stop: &AtomicBool, now: Instant) -> bool {
        let headsets = match self.source.probe(stop) {
            Ok(headsets) => {
                if self.probe_failing {
                    info!("battery readings are back");
                    self.probe_failing = false;
                }
                self.announce(Summary::of(&headsets), &headsets, now);
                headsets
            }
            Err(err) if stop.load(Ordering::Relaxed) => {
                // A reading cut short by the shutdown is not a failure.
                debug!("{err:#}");
                Vec::new()
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

        for headset in &headsets {
            if !headset.supports_battery {
                trace!("{} does not report a battery, skipping", headset.name);
                continue;
            }
            if let Err(err) = self.apply(headset, now) {
                error!("{}: {err:#}", headset.name);
            }
        }

        for battery in self.batteries.iter_mut() {
            note(&mut battery.silent_since, battery.last_reading == now, now);
            note(&mut battery.missing_since, battery.last_seen == now, now);
        }
        self.expire(now);
        self.batteries
            .iter()
            .any(|battery| battery.silent_since.is_some())
    }

    /// Says what the poll found, but only when that changed since last time.
    fn announce(&mut self, summary: Summary, headsets: &[Headset], now: Instant) {
        if self.last_summary == Some(summary) {
            self.unsettled = None;
            return;
        }
        // Say nothing about a state that may not last: the dongle drops off the
        // bus for a couple of seconds whenever the headset comes or goes, and
        // "check that the dongle is plugged in" is not what that calls for.
        // That goes for the first summary too: the reader listens for a few
        // seconds before asking anything, and "not answering" is not news then.
        match self.unsettled {
            Some((pending, since)) if pending == summary => {
                if now.duration_since(since) < SUMMARY_SETTLE {
                    return;
                }
            }
            _ => {
                self.unsettled = Some((summary, now));
                return;
            }
        }
        self.unsettled = None;
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
                "{} is detected but not answering battery queries (switched off?)",
                names()
            ),
            // Nothing to say: the level lines speak for themselves.
            Summary::Online => debug!("{} is answering", names()),
        }
    }

    /// Creates or updates the virtual battery backing `headset`.
    fn apply(&mut self, headset: &Headset, now: Instant) -> Result<()> {
        let key = headset.key();
        let existing = self.batteries.position(&key);

        // Being listed at all is what keeps the entry alive; answering is what
        // refreshes the level.
        if let Some(index) = existing {
            self.batteries[index].last_seen = now;
        }

        // The daemon's own notion of a battery is richer than uhid-battery's
        // `Reading` - it knows about a headset that is off, or charging with
        // no level to show. This is where the one narrows to the other.
        let reading = match headset.battery {
            BatteryState::Discharging(percent) => Reading::new(percent, false),
            BatteryState::Charging(Some(percent)) => Reading::new(percent, true),
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
                Reading::new(self.batteries.device(index).reading().percent, true)
            }
            BatteryState::Unavailable => {
                trace!("{} is offline", headset.name);
                return Ok(());
            }
            // The reader was told the headset is gone: no grace to sit out.
            BatteryState::Disconnected => {
                if let Some(index) = existing {
                    let since = *self.batteries[index].gone_since.get_or_insert(now);
                    if now.duration_since(since) >= DISCONNECT_SETTLE {
                        let (device, battery) = self.batteries.remove(index);
                        info!("{} was switched off, removing its battery", battery.name);
                        self.uhid.release(device);
                    }
                }
                return Ok(());
            }
        };

        let Some(index) = existing else {
            let retry_pending = self
                .attach_retry_at
                .get(&key)
                .is_some_and(|retry_at| now < *retry_at);
            if retry_pending || self.abandoned.contains(&key) {
                return Ok(());
            }
            return match self.attach(headset, reading, now) {
                Ok(()) => {
                    self.attach_retry_at.remove(&key);
                    Ok(())
                }
                Err(AttachFailure::Retry(err)) => {
                    self.attach_retry_at.insert(key, now + ATTACH_RETRY);
                    Err(err)
                }
                // Logged once by the caller, like any other error - and then
                // never again, since the headset is skipped from here on.
                Err(AttachFailure::GiveUp(err)) => {
                    self.abandoned.insert(key);
                    Err(err.context("giving up on this headset until the daemon restarts"))
                }
            };
        };

        let (device, battery) = self.batteries.pair_mut(index);
        // The headset answered, so it is alive even if we end up distrusting
        // the level it gave us.
        battery.last_reading = now;
        battery.gone_since = None;

        let known = device.reading();
        let confirming = second_opinion(battery.deferred, battery.deferred_sample, headset.sample);
        if vet(known.percent, reading.percent, confirming) == Verdict::Defer {
            debug!(
                "{}: holding back an implausible {}% (last known {}%)",
                battery.name, reading.percent, known.percent
            );
            battery.deferred = Some(reading.percent);
            battery.deferred_sample = headset.sample;
            return Ok(());
        }
        battery.deferred = None;
        battery.deferred_sample = None;

        // An unchanged level is still pushed now and then. The kernel drops input
        // reports without telling anyone while it is probing the device, so
        // this is what guarantees a lost push is made up for.
        let changed = known != reading;
        if !changed && now.duration_since(battery.published_at) < REPUBLISH_EVERY {
            return Ok(());
        }
        if changed {
            let percent = reading.percent;
            let suffix = if reading.charging { ", charging" } else { "" };
            if known.charging != reading.charging
                || battery.logged_percent.abs_diff(percent) >= LOG_STEP
            {
                battery.logged_percent = percent;
                info!("{}: {percent}%{suffix}", battery.name);
            } else {
                debug!("{}: {percent}%{suffix}", battery.name);
            }
        }
        battery.published_at = now;
        let pushed = device
            .update(reading)
            .with_context(|| format!("publishing the level of {}", battery.name));
        self.outcome(index, pushed, now);
        Ok(())
    }

    /// Takes in the result of talking to the virtual device at `index`, and
    /// says whether the battery is still there.
    ///
    /// A failure is logged once per streak, at error level, and then kept
    /// quiet; after [`DEVICE_FAILURE_LIMIT`] in a row the device is destroyed,
    /// its handle kept, and the headset goes back through the attach path
    /// after [`ATTACH_RETRY`], like one whose device could not be created.
    fn outcome(&mut self, index: usize, result: Result<()>, now: Instant) -> bool {
        let battery = &mut self.batteries[index];
        let err = match result {
            Ok(()) => {
                if battery.failures > 0 {
                    info!("{}: its virtual device answers again", battery.name);
                    battery.failures = 0;
                }
                return true;
            }
            Err(err) => err,
        };

        battery.failures += 1;
        if battery.failures == 1 {
            error!("{err:#}");
        } else {
            debug!("still failing ({} in a row): {err:#}", battery.failures);
        }
        if battery.failures < DEVICE_FAILURE_LIMIT {
            return true;
        }

        let (device, battery) = self.batteries.remove(index);
        warn!(
            "{}: {DEVICE_FAILURE_LIMIT} failures in a row, destroying its virtual device; \
             creating it again in {ATTACH_RETRY:?}",
            battery.name
        );
        self.attach_retry_at.insert(battery.key, now + ATTACH_RETRY);
        self.uhid.release(device);
        false
    }

    /// Registers a new virtual battery with the kernel.
    fn attach(
        &mut self,
        headset: &Headset,
        reading: Reading,
        now: Instant,
    ) -> Result<(), AttachFailure> {
        let uniq = headset.uniq();
        let identity = Identity::new(headset.display_name(), uniq.clone())
            .phys(format!("{PHYS_PREFIX}/{uniq}"))
            .vendor(headset.vendor_id)
            .product(headset.product_id);

        let handle = self.uhid.acquire()?;
        let mut device = match self.uhid.create(handle, &identity, Kind::Headset, reading) {
            Ok(device) => device,
            Err(err) => {
                let kind = err.kind();
                let (handle, source) = err.into_parts();
                self.uhid.put_back(handle);
                // An identity the kernel would truncate is refused before
                // anything is written, and it is built from nothing that
                // changes while the daemon runs: a retry would fail the same
                // way. `display_name` and `uniq` are meant to make that
                // impossible, so this is a bug report in the making.
                if kind == CreateErrorKind::InvalidIdentity {
                    return Err(source)
                        .with_context(|| {
                            format!(
                                "the kernel can never accept its identity (name {:?}, uniq {:?})",
                                identity.name, identity.uniq
                            )
                        })
                        .map_err(AttachFailure::GiveUp);
                }
                return Err(source)
                    .context("creating the virtual HID battery")
                    .map_err(AttachFailure::Retry);
            }
        };

        // Creating the HID device is not the goal; the power supply is. The
        // kernel accepts a device it then builds no battery for without a
        // word, so look for the result instead of assuming it.
        let sysfs = match self
            .uhid
            .wait_for_power_supply(&mut device, REGISTRATION_TIMEOUT)
        {
            Ok(Some(sysfs)) => sysfs,
            Ok(None) => {
                self.uhid.release(device);
                return Err(AttachFailure::Retry(anyhow::anyhow!(
                    "the kernel created the HID device but no hid-{uniq}-battery power supply; \
                     is CONFIG_HID_BATTERY_STRENGTH enabled? (see `journalctl -k`)"
                )));
            }
            Err(err) => {
                self.uhid.release(device);
                return Err(err)
                    .context("waiting for the power supply")
                    .map_err(AttachFailure::Retry);
            }
        };

        info!(
            "{} appeared: {}%{} ({})",
            headset.name,
            reading.percent,
            if reading.charging { ", charging" } else { "" },
            sysfs.display()
        );
        self.batteries.push(
            device,
            VirtualBattery {
                key: headset.key(),
                name: headset.name.clone(),
                last_reading: now,
                last_seen: now,
                silent_since: None,
                missing_since: None,
                gone_since: None,
                deferred: None,
                deferred_sample: None,
                logged_percent: reading.percent,
                published_at: now,
                failures: 0,
            },
        );
        Ok(())
    }

    /// Withdraws batteries that have outlived their grace, saying which one.
    fn expire(&mut self, now: Instant) {
        let mut index = 0;
        while index < self.batteries.len() {
            let battery = &self.batteries[index];
            let since = |start: Option<Instant>| {
                start.map_or(Duration::ZERO, |start| now.duration_since(start))
            };
            let verdict = withdrawal(
                since(battery.silent_since),
                since(battery.missing_since),
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

            let (device, battery) = self.batteries.remove(index);
            info!("{} {reason}, removing its battery", battery.name);
            self.uhid.release(device);
        }
    }

    /// Serves every battery - answers the kernel, which would otherwise keep
    /// whoever is reading the level from sysfs waiting for five seconds, and
    /// pushes whatever is due - for at most `timeout`.
    ///
    /// With no battery to watch this is a plain sleep, but still through
    /// `poll()`: unlike `thread::sleep`, it returns when a signal arrives.
    fn wait(&mut self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            let err = match serve_all(self.batteries.devices_mut(), Some(deadline), None) {
                // `Wake` cannot happen - no wake descriptor is handed over -
                // and a signal is the caller's business: it checks the stop
                // flag before calling again.
                Ok(Wakeup::Deadline | Wakeup::Interrupted | Wakeup::Wake) => return Ok(()),
                Err(err) => err,
            };
            // No index: the wait itself failed, and no battery is to blame.
            let Some(index) = err.index() else {
                return Err(err.into_source()).context("waiting on /dev/uhid");
            };

            // `serve_all` stops at the first battery that fails, before the
            // ones after it are serviced. The failure goes through the usual
            // policy, then the loop serves again with the same deadline: the
            // others get their turn, and so does the culprit if it is still
            // there. A device that keeps failing fails again at once - the
            // read never blocks - so it collects its DEVICE_FAILURE_LIMIT
            // strikes and is destroyed within this call, which is also what
            // stops a broken descriptor from waking `poll()` for ever.
            let serviced = Err(err.into_source()).with_context(|| {
                format!(
                    "servicing the virtual device of {}",
                    self.batteries[index].name
                )
            });
            self.outcome(index, serviced, Instant::now());
        }
    }

    /// Removes every virtual battery, so the applet does not keep a stale entry
    /// around while the daemon is restarting.
    fn shutdown(&mut self) {
        for (device, battery) in self.batteries.drain() {
            debug!("withdrawing {}", battery.name);
            drop(device.destroy());
        }
    }
}
