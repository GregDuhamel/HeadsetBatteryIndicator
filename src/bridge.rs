//! The daemon itself: read the batteries, mirror them onto virtual HID
//! batteries, and keep answering the kernel in between.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::ops::{Index, IndexMut};
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

/// Default for [`Config::interval`], in seconds.
pub const DEFAULT_INTERVAL_SECS: u64 = 60;
/// How often the native reader is run. It only listens to the dongle - nothing
/// goes over the air - and that is how it learns within a second that the
/// headset came or went.
const NATIVE_TICK: Duration = Duration::from_secs(1);

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

/// Once a poll goes unanswered, how soon to ask again. Several retries fit in
/// the grace, so one lost reading never makes the entry flap - and withdrawing
/// is cheap to undo, the battery is back on the first poll that answers.
const UNANSWERED_RETRY: Duration = Duration::from_secs(3);

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

/// How long to wait before the next poll.
///
/// Same shape as razerd's pacing: a battery that just went quiet is asked again
/// shortly, so that it is withdrawn within seconds of the headset being switched
/// off rather than a minute later; otherwise the cadence is whatever the reader
/// that answered can afford.
fn next_delay(unanswered: bool, native: bool, config: &Config) -> Duration {
    let regular = if native { NATIVE_TICK } else { config.interval };
    if unanswered {
        UNANSWERED_RETRY.min(regular)
    } else {
        regular
    }
}

/// Starts, keeps or clears the clock of a condition that is `fine` or not on
/// this poll. The clock starts on the first poll that is not fine.
fn note(since: &mut Option<Instant>, fine: bool, now: Instant) {
    if fine {
        *since = None;
    } else {
        since.get_or_insert(now);
    }
}

/// Why a virtual battery is being taken down, if it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Withdrawal {
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
fn withdrawal(silent_for: Duration, missing_for: Duration, config: &Config) -> Withdrawal {
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
fn second_opinion(
    deferred: Option<u8>,
    deferred_sample: Option<u64>,
    sample: Option<u64>,
) -> Option<u8> {
    let same_reading = sample.is_some() && sample == deferred_sample;
    deferred.filter(|_| !same_reading)
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

/// What the daemon knows about one virtual battery, besides the device itself
/// - which lives in [`Batteries`], at the same index.
#[derive(Debug)]
struct VirtualBattery {
    key: String,
    name: String,
    /// When the headset last gave us a usable level.
    last_reading: Instant,
    /// When the headset was last reported at all, answering or not.
    last_seen: Instant,
    /// Since when the headset has been silent, if it is: the poll on which it
    /// first failed to answer. The grace runs from there and not from the last
    /// answer - which is a whole polling interval older, so that with a grace
    /// no longer than the interval the first lost reading would already have
    /// outlived it, and the retries would never get their chance.
    silent_since: Option<Instant>,
    /// Since when the headset has been missing from the polls, likewise.
    missing_since: Option<Instant>,
    /// Since when the reader has been saying the headset is gone.
    gone_since: Option<Instant>,
    /// A reading that was too far off to publish, waiting for confirmation,
    /// and which reading that was.
    deferred: Option<u8>,
    deferred_sample: Option<u64>,
    /// The level the last INFO line mentioned.
    logged_percent: u8,
    /// When a reading was last pushed to the kernel.
    published_at: Instant,
    /// How many times in a row the device could not be talked to.
    failures: u32,
}

/// The virtual batteries: the devices, and what the daemon knows about each.
///
/// Two vectors rather than one of pairs, because [`serve_all`] wants the
/// [`Battery`] values contiguous (`&mut [Battery]`) and nothing else about
/// them. An index means the same in both; every change goes through here so
/// that they cannot drift apart. Indexing yields the bookkeeping, which is
/// what most of the daemon reads; the device is asked for by name.
#[derive(Debug, Default)]
struct Batteries {
    devices: Vec<Battery>,
    states: Vec<VirtualBattery>,
}

impl Batteries {
    fn len(&self) -> usize {
        self.states.len()
    }

    /// The index of the battery mirroring the headset with this key.
    fn position(&self, key: &str) -> Option<usize> {
        self.states.iter().position(|state| state.key == key)
    }

    fn push(&mut self, device: Battery, state: VirtualBattery) {
        self.devices.push(device);
        self.states.push(state);
    }

    /// Takes a battery out, closing the gap: every index past it moves down
    /// by one, as with `Vec::remove`.
    fn remove(&mut self, index: usize) -> (Battery, VirtualBattery) {
        (self.devices.remove(index), self.states.remove(index))
    }

    fn drain(&mut self) -> impl Iterator<Item = (Battery, VirtualBattery)> + '_ {
        self.devices.drain(..).zip(self.states.drain(..))
    }

    fn device(&self, index: usize) -> &Battery {
        &self.devices[index]
    }

    /// The device and its bookkeeping, borrowed together: they are separate
    /// fields, so both can be held mutably at once.
    fn pair_mut(&mut self, index: usize) -> (&mut Battery, &mut VirtualBattery) {
        (&mut self.devices[index], &mut self.states[index])
    }

    /// Every device, the way [`serve_all`] wants them.
    fn devices_mut(&mut self) -> &mut [Battery] {
        &mut self.devices
    }

    fn iter(&self) -> impl Iterator<Item = &VirtualBattery> {
        self.states.iter()
    }

    fn iter_mut(&mut self) -> impl Iterator<Item = &mut VirtualBattery> {
        self.states.iter_mut()
    }
}

impl Index<usize> for Batteries {
    type Output = VirtualBattery;

    fn index(&self, index: usize) -> &Self::Output {
        &self.states[index]
    }
}

impl IndexMut<usize> for Batteries {
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        &mut self.states[index]
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
            sample: None,
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

    #[test]
    fn the_watchdog_outlasts_the_slowest_turn_of_the_loop() {
        // The unit's WatchdogSec= is sized from the loop (see MAX_WAIT): a
        // wait, a HeadsetControl reading at the unit's --timeout, and a
        // device registration. Keep the two in step.
        const UNIT: &str = include_str!("../packaging/systemd/headset-battery-indicator.service");
        let watchdog_sec: u64 = UNIT
            .lines()
            .find_map(|line| line.strip_prefix("WatchdogSec="))
            .expect("the unit sets WatchdogSec=")
            .trim()
            .parse()
            .expect("WatchdogSec= in whole seconds");
        assert!(WATCHDOG_WORST_CASE >= MAX_WAIT + REGISTRATION_TIMEOUT);
        assert!(Duration::from_secs(watchdog_sec) >= WATCHDOG_WORST_CASE * 3);
        assert!(UNIT.contains("\nType=notify\n"));
    }

    #[test]
    fn default_config_is_conservative() {
        let config = Config::default();
        assert_eq!(config.interval, Duration::from_secs(60));
        assert!(config.missing_grace > config.offline_grace);
        assert_eq!(config.uhid_path, PathBuf::from("/dev/uhid"));
    }

    /// The bridge on a scripted source and a fake kernel: the real
    /// `Battery`, `serve_all` and bookkeeping, over a socket pair, with the
    /// clock in the test's hands.
    mod live {
        use std::collections::VecDeque;

        use uhid_battery::fake::{Kernel, RTYPE_INPUT, Sent, get_report};

        use super::*;

        /// A source that answers each probe with the next scripted reading,
        /// and with an empty poll once the script runs out.
        #[derive(Debug, Default)]
        struct Scripted {
            answers: VecDeque<Result<Vec<Headset>>>,
            native: bool,
        }

        impl BatterySource for Scripted {
            fn describe(&self) -> String {
                "a script".to_owned()
            }

            fn last_probe_was_native(&self) -> bool {
                self.native
            }

            fn probe(&mut self, _stop: &AtomicBool) -> Result<Vec<Headset>> {
                self.answers.pop_front().unwrap_or_else(|| Ok(Vec::new()))
            }
        }

        /// A uhid whose kernel is a socket pair per device, and that never
        /// looks at sysfs: the one thing the fake kernel cannot do is list a
        /// power supply, so the test says whether it did.
        #[derive(Debug)]
        struct FakeUhid {
            /// The kernel's end of every device created so far, oldest first;
            /// `None` once the test made one vanish.
            kernels: Vec<Option<Kernel>>,
            /// Whether the kernel registers a power supply for a new device.
            registers: bool,
            /// Whether the next identity is to be refused, the way the kernel
            /// would refuse one it cannot hold. The bridge builds identities
            /// that always fit, so the test has to force it.
            refuse_identity: bool,
            /// How many handles were handed out.
            acquired: usize,
        }

        impl FakeUhid {
            fn new() -> Self {
                Self {
                    kernels: Vec::new(),
                    registers: true,
                    refuse_identity: false,
                    acquired: 0,
                }
            }

            fn kernel(&self, index: usize) -> &Kernel {
                self.kernels[index]
                    .as_ref()
                    .expect("the kernel is still there")
            }

            fn vanish(&mut self, index: usize) {
                self.kernels[index]
                    .take()
                    .expect("the kernel was there")
                    .vanish();
            }
        }

        impl Uhid for FakeUhid {
            fn acquire(&mut self) -> Result<Handle> {
                let (kernel, handle) = Kernel::new();
                self.kernels.push(Some(kernel));
                self.acquired += 1;
                Ok(handle)
            }

            /// One fake kernel per handle: a handle given back is closed, and
            /// the next device gets a kernel of its own. The real pool keeps
            /// the handle, which `the_pool_hands_out_inherited_handles_first`
            /// covers.
            fn put_back(&mut self, handle: Handle) {
                drop(handle);
            }

            fn create(
                &mut self,
                handle: Handle,
                identity: &Identity,
                kind: Kind,
                reading: Reading,
            ) -> Result<Battery, CreateError> {
                if self.refuse_identity {
                    // A genuine `InvalidIdentity`, with nothing written.
                    let bad = Identity::new("Test", "not valid");
                    return Battery::create(handle, &bad, kind, reading);
                }
                Battery::create(handle, identity, kind, reading)
            }

            fn wait_for_power_supply(
                &mut self,
                device: &mut Battery,
                _timeout: Duration,
            ) -> io::Result<Option<PathBuf>> {
                // The real wait services the device while it looks.
                device.service()?;
                Ok(self
                    .registers
                    .then(|| PathBuf::from("/sys/class/power_supply/hid-fake-battery")))
            }
        }

        type TestBridge = Bridge<Scripted, FakeUhid>;

        fn bridge() -> TestBridge {
            Bridge::over(Config::default(), Scripted::default(), FakeUhid::new())
        }

        impl TestBridge {
            /// One tick, with the source reporting `headsets`.
            fn poll(&mut self, headsets: Vec<Headset>, now: Instant) -> bool {
                self.source.answers.push_back(Ok(headsets));
                self.tick(&AtomicBool::new(false), now)
            }

            /// One tick, with the source failing.
            fn poll_fails(&mut self, now: Instant) -> bool {
                self.source
                    .answers
                    .push_back(Err(anyhow::anyhow!("no reader")));
                self.tick(&AtomicBool::new(false), now)
            }

            fn kernel(&self, index: usize) -> &Kernel {
                self.uhid.kernel(index)
            }

            /// Forgets what the kernel has been sent so far.
            fn drain(&self, index: usize) {
                let _ = self.kernel(index).sent_all();
            }
        }

        /// The report the kernel is told: its ID, the level, charging.
        fn report(percent: u8, charging: bool) -> Sent {
            Sent::Input2(vec![Kind::Headset.report_id(), percent, u8::from(charging)])
        }

        fn maxwell(battery: BatteryState) -> Headset {
            headset("Audeze Maxwell", true, battery)
        }

        /// A reading the native reader hands out: cached between the dongle's
        /// messages, hence numbered.
        fn sampled(percent: u8, sample: u64) -> Headset {
            Headset {
                sample: Some(sample),
                ..maxwell(BatteryState::Discharging(percent))
            }
        }

        fn seconds(n: u64) -> Duration {
            Duration::from_secs(n)
        }

        #[test]
        fn the_first_reading_creates_the_device_and_the_level_is_pushed_again_every_minute() {
            let mut bridge = bridge();
            let start = Instant::now();

            // Nothing to publish for a headset that has no level yet.
            bridge.poll(vec![maxwell(BatteryState::Unavailable)], start);
            assert_eq!(bridge.uhid.acquired, 0);
            assert_eq!(bridge.batteries.len(), 0);

            // The first level creates the device, with the daemon's identity,
            // and pushes it.
            assert!(!bridge.poll(vec![maxwell(BatteryState::Discharging(80))], start));
            assert_eq!(bridge.batteries.len(), 1);
            let sent = bridge.kernel(0).sent_all();
            let Sent::Create2 {
                name,
                phys,
                uniq,
                vendor,
                product,
                ..
            } = &sent[0]
            else {
                panic!("the device was not created: {sent:?}");
            };
            assert_eq!(name, "Audeze Maxwell");
            assert_eq!(uniq, "headset-3329-4b18");
            assert_eq!(phys, "headset-battery-indicator/headset-3329-4b18");
            assert_eq!((*vendor, *product), (0x3329, 0x4b18));
            assert_eq!(sent[1..], [report(80, false)]);

            // The same level again is not pushed again...
            bridge.poll(
                vec![maxwell(BatteryState::Discharging(80))],
                start + seconds(1),
            );
            assert_eq!(bridge.kernel(0).sent_all(), []);
            // ...until a minute has passed since the last push, as a safety
            // net for one the kernel dropped while probing the device.
            bridge.poll(
                vec![maxwell(BatteryState::Discharging(80))],
                start + seconds(59),
            );
            assert_eq!(bridge.kernel(0).sent_all(), []);
            bridge.poll(
                vec![maxwell(BatteryState::Discharging(80))],
                start + seconds(60),
            );
            assert_eq!(bridge.kernel(0).sent_all(), [report(80, false)]);

            // A change is pushed at once, on the same device.
            bridge.poll(
                vec![maxwell(BatteryState::Discharging(79))],
                start + seconds(61),
            );
            assert_eq!(bridge.kernel(0).sent_all(), [report(79, false)]);
            assert_eq!(bridge.uhid.acquired, 1);
        }

        #[test]
        fn a_switched_off_headset_is_withdrawn_once_the_dongle_has_said_so_for_a_while() {
            let mut bridge = bridge();
            let start = Instant::now();
            bridge.poll(vec![maxwell(BatteryState::Discharging(80))], start);
            bridge.drain(0);

            // The dongle says the headset is gone: not believed at once, since
            // the link drops for a second while a freshly powered headset
            // settles.
            bridge.poll(
                vec![maxwell(BatteryState::Disconnected)],
                start + seconds(1),
            );
            assert_eq!(bridge.batteries.len(), 1);
            bridge.poll(
                vec![maxwell(BatteryState::Disconnected)],
                start + seconds(3),
            );
            assert_eq!(bridge.batteries.len(), 1);
            assert_eq!(bridge.kernel(0).sent_all(), []);

            // A level in between is the headset back, and the clock resets.
            bridge.poll(
                vec![maxwell(BatteryState::Discharging(80))],
                start + seconds(3),
            );
            bridge.poll(
                vec![maxwell(BatteryState::Disconnected)],
                start + seconds(4),
            );
            bridge.poll(
                vec![maxwell(BatteryState::Disconnected)],
                start + seconds(6),
            );
            assert_eq!(bridge.batteries.len(), 1);

            // Said for DISCONNECT_SETTLE: the device goes, and the kernel is
            // told so.
            bridge.poll(
                vec![maxwell(BatteryState::Disconnected)],
                start + seconds(4) + DISCONNECT_SETTLE,
            );
            assert_eq!(bridge.batteries.len(), 0);
            assert_eq!(bridge.kernel(0).sent_all(), [Sent::Destroy]);

            // Switched back on: a new device, on a handle of its own.
            bridge.poll(
                vec![maxwell(BatteryState::Discharging(80))],
                start + seconds(10),
            );
            assert_eq!(bridge.batteries.len(), 1);
            assert_eq!(bridge.uhid.acquired, 2);
            assert_eq!(bridge.kernel(1).sent_all().len(), 2);
        }

        #[test]
        fn a_device_the_kernel_builds_no_power_supply_for_is_tried_again_after_a_minute() {
            let mut bridge = bridge();
            bridge.uhid.registers = false;
            let start = Instant::now();

            // Created, then destroyed when no power supply showed up.
            bridge.poll(vec![maxwell(BatteryState::Discharging(80))], start);
            assert_eq!(bridge.batteries.len(), 0);
            let sent = bridge.kernel(0).sent_all();
            assert_eq!(sent.len(), 3, "{sent:?}");
            assert_eq!(sent[2], Sent::Destroy);

            // Not tried again every second: that would log an error every
            // second for as long as the failure lasts.
            for elapsed in [1, 30, 59] {
                bridge.poll(
                    vec![maxwell(BatteryState::Discharging(80))],
                    start + seconds(elapsed),
                );
                assert_eq!(bridge.uhid.acquired, 1, "after {elapsed}s");
            }

            // After ATTACH_RETRY it is, and succeeds once the kernel plays.
            bridge.uhid.registers = true;
            bridge.poll(
                vec![maxwell(BatteryState::Discharging(81))],
                start + ATTACH_RETRY,
            );
            assert_eq!(bridge.uhid.acquired, 2);
            assert_eq!(bridge.batteries.len(), 1);
            assert_eq!(bridge.kernel(1).sent_all()[1], report(81, false));
            assert_eq!(bridge.attach_retry_at.len(), 0);
        }

        #[test]
        fn charging_without_a_level_keeps_the_last_level() {
            let mut bridge = bridge();
            let start = Instant::now();

            // Nothing to show yet: no device.
            bridge.poll(vec![maxwell(BatteryState::Charging(None))], start);
            assert_eq!(bridge.uhid.acquired, 0);

            bridge.poll(
                vec![maxwell(BatteryState::Discharging(80))],
                start + seconds(1),
            );
            bridge.drain(0);

            // On the cable, level unknown: the last level, now charging.
            bridge.poll(
                vec![maxwell(BatteryState::Charging(None))],
                start + seconds(2),
            );
            assert_eq!(bridge.kernel(0).sent_all(), [report(80, true)]);
            assert_eq!(bridge.batteries[0].silent_since, None, "it did answer");

            // With a level, that level.
            bridge.poll(
                vec![maxwell(BatteryState::Charging(Some(82)))],
                start + seconds(3),
            );
            assert_eq!(bridge.kernel(0).sent_all(), [report(82, true)]);
            // And the same level again is nothing new.
            bridge.poll(
                vec![maxwell(BatteryState::Charging(None))],
                start + seconds(4),
            );
            assert_eq!(bridge.kernel(0).sent_all(), []);
            assert_eq!(bridge.batteries.len(), 1);
        }

        #[test]
        fn an_implausible_reading_waits_for_a_second_opinion_and_a_repeat_is_not_one() {
            let mut bridge = bridge();
            let start = Instant::now();
            bridge.poll(vec![sampled(92, 1)], start);
            bridge.drain(0);

            // The glitch a Maxwell produces: 0% out of nowhere. Held back.
            bridge.poll(vec![sampled(0, 2)], start + seconds(1));
            assert_eq!(bridge.kernel(0).sent_all(), []);
            assert_eq!(bridge.batteries[0].deferred, Some(0));
            // The native reader hands the same reading out again a second
            // later: still the same reading, not a confirmation.
            bridge.poll(vec![sampled(0, 2)], start + seconds(2));
            assert_eq!(bridge.kernel(0).sent_all(), []);
            assert_eq!(bridge.batteries[0].deferred, Some(0));
            // Back to normal: nothing bogus ever reached the kernel.
            bridge.poll(vec![sampled(92, 3)], start + seconds(3));
            assert_eq!(bridge.kernel(0).sent_all(), []);
            assert_eq!(bridge.batteries[0].deferred, None);

            // A genuine change, confirmed by a *new* reading, goes through.
            bridge.poll(vec![sampled(40, 4)], start + seconds(4));
            assert_eq!(bridge.kernel(0).sent_all(), []);
            bridge.poll(vec![sampled(38, 5)], start + seconds(5));
            assert_eq!(bridge.kernel(0).sent_all(), [report(38, false)]);
        }

        #[test]
        fn a_silent_headset_is_withdrawn_after_the_offline_grace() {
            let mut bridge = bridge();
            let config = bridge.config.clone();
            let start = Instant::now();
            bridge.poll(vec![maxwell(BatteryState::Discharging(80))], start);
            bridge.drain(0);

            // Listed but not answering: the grace runs from this poll, and
            // the tick asks for a prompt retry.
            let first_miss = start + config.interval;
            assert!(bridge.poll(vec![maxwell(BatteryState::Unavailable)], first_miss));
            assert_eq!(bridge.batteries.len(), 1);
            assert!(bridge.poll(
                vec![maxwell(BatteryState::Unavailable)],
                first_miss + config.offline_grace
            ));
            assert_eq!(bridge.batteries.len(), 1);
            bridge.poll(
                vec![maxwell(BatteryState::Unavailable)],
                first_miss + config.offline_grace + seconds(1),
            );
            assert_eq!(bridge.batteries.len(), 0);
            assert_eq!(bridge.kernel(0).sent_all(), [Sent::Destroy]);
        }

        #[test]
        fn a_missing_headset_is_withdrawn_after_the_missing_grace_not_the_offline_one() {
            let mut bridge = bridge();
            let config = bridge.config.clone();
            let start = Instant::now();
            bridge.poll(vec![maxwell(BatteryState::Discharging(80))], start);
            bridge.drain(0);

            // Dongle unplugged: no reader reports the headset at all. The
            // longer grace applies, even though nothing answers either.
            let gone = start + seconds(1);
            bridge.poll(vec![], gone);
            bridge.poll(vec![], gone + config.offline_grace + seconds(1));
            assert_eq!(
                bridge.batteries.len(),
                1,
                "the offline grace does not apply"
            );
            bridge.poll(vec![], gone + config.missing_grace);
            assert_eq!(bridge.batteries.len(), 1);
            bridge.poll(vec![], gone + config.missing_grace + seconds(1));
            assert_eq!(bridge.batteries.len(), 0);
            assert_eq!(bridge.kernel(0).sent_all(), [Sent::Destroy]);
        }

        #[test]
        fn a_failing_reader_is_an_empty_poll_said_once() {
            let mut bridge = bridge();
            let start = Instant::now();
            bridge.poll(vec![maxwell(BatteryState::Discharging(80))], start);

            bridge.poll_fails(start + seconds(1));
            assert!(bridge.probe_failing);
            assert_eq!(bridge.batteries.len(), 1);
            assert!(bridge.batteries[0].missing_since.is_some());
            bridge.poll_fails(start + seconds(2));
            assert!(bridge.probe_failing);

            // Back before the grace is out: the entry never blinked.
            bridge.poll(
                vec![maxwell(BatteryState::Discharging(80))],
                start + seconds(3),
            );
            assert!(!bridge.probe_failing);
            assert_eq!(bridge.batteries[0].missing_since, None);
            assert_eq!(bridge.uhid.acquired, 1);
        }

        #[test]
        fn a_service_failure_is_a_strike_and_three_strikes_destroy_the_device() {
            let mut bridge = bridge();
            let start = Instant::now();
            bridge.poll(vec![maxwell(BatteryState::Discharging(80))], start);
            bridge.drain(0);

            // A request the daemon will try to answer after the kernel has
            // gone: the reply fails, and `serve_all` names the battery. (The
            // fake kernel is a socket: once gone it fails the next write and
            // then goes quiet, where a broken uhid device fails every time.
            // So the first strike comes from servicing, the other two from
            // pushes - the same policy either way.)
            let id = Kind::Headset.report_id();
            bridge.kernel(0).send(&get_report(1, id, RTYPE_INPUT));
            bridge.uhid.vanish(0);
            bridge.wait(Duration::from_millis(20)).unwrap();
            assert_eq!(bridge.batteries.len(), 1);
            assert_eq!(bridge.batteries[0].failures, 1);

            bridge.poll(
                vec![maxwell(BatteryState::Discharging(79))],
                start + seconds(1),
            );
            assert_eq!(bridge.batteries[0].failures, 2);
            let third = start + seconds(2);
            bridge.poll(vec![maxwell(BatteryState::Discharging(78))], third);
            assert_eq!(bridge.batteries.len(), 0);
            let key = maxwell(BatteryState::Unavailable).key();
            assert_eq!(
                bridge.attach_retry_at.get(&key),
                Some(&(third + ATTACH_RETRY))
            );

            // Not recreated before the retry delay...
            bridge.poll(
                vec![maxwell(BatteryState::Discharging(80))],
                third + seconds(30),
            );
            assert_eq!(bridge.uhid.acquired, 1);
            // ...and recreated after it, from scratch, with a clean record.
            bridge.poll(
                vec![maxwell(BatteryState::Discharging(80))],
                third + ATTACH_RETRY,
            );
            assert_eq!(bridge.uhid.acquired, 2);
            assert_eq!(bridge.batteries.len(), 1);
            assert_eq!(bridge.batteries[0].failures, 0);
            assert_eq!(bridge.kernel(1).sent_all()[1], report(80, false));
            assert!(!bridge.attach_retry_at.contains_key(&key));
        }

        #[test]
        fn a_push_that_fails_counts_as_a_strike_and_one_that_works_clears_them() {
            let mut bridge = bridge();
            let start = Instant::now();
            bridge.poll(vec![maxwell(BatteryState::Discharging(80))], start);
            bridge.drain(0);
            bridge.uhid.vanish(0);

            // Two pushes fail: logged, kept.
            bridge.poll(
                vec![maxwell(BatteryState::Discharging(79))],
                start + seconds(1),
            );
            bridge.poll(
                vec![maxwell(BatteryState::Discharging(78))],
                start + seconds(2),
            );
            assert_eq!(bridge.batteries.len(), 1);
            assert_eq!(bridge.batteries[0].failures, 2);
            // The third is the limit.
            let third = start + seconds(3);
            bridge.poll(vec![maxwell(BatteryState::Discharging(77))], third);
            assert_eq!(bridge.batteries.len(), 0);

            // On a working device a success ends the streak.
            bridge.poll(
                vec![maxwell(BatteryState::Discharging(77))],
                third + ATTACH_RETRY,
            );
            assert_eq!(bridge.batteries.len(), 1);
            bridge.batteries[0].failures = 2;
            bridge.poll(
                vec![maxwell(BatteryState::Discharging(76))],
                third + ATTACH_RETRY + seconds(1),
            );
            assert_eq!(bridge.batteries[0].failures, 0);
        }

        #[test]
        fn an_identity_the_kernel_can_never_accept_is_given_up_on_once() {
            let mut bridge = bridge();
            bridge.uhid.refuse_identity = true;
            let start = Instant::now();

            bridge.poll(vec![maxwell(BatteryState::Discharging(80))], start);
            assert_eq!(bridge.batteries.len(), 0);
            // Nothing reached the kernel, and nothing will: not even after
            // the retry delay, since a retry would fail the same way.
            assert_eq!(bridge.kernel(0).sent_all(), []);
            let key = maxwell(BatteryState::Unavailable).key();
            assert!(bridge.abandoned.contains(&key));
            assert!(!bridge.attach_retry_at.contains_key(&key));
            bridge.uhid.refuse_identity = false;
            bridge.poll(
                vec![maxwell(BatteryState::Discharging(80))],
                start + ATTACH_RETRY * 2,
            );
            assert_eq!(bridge.uhid.acquired, 1);
            assert_eq!(bridge.batteries.len(), 0);
        }

        #[test]
        fn a_new_state_is_announced_once_it_has_settled() {
            let mut bridge = bridge();
            let start = Instant::now();
            let on = maxwell(BatteryState::Discharging(80));

            bridge.poll(vec![on.clone()], start);
            assert_eq!(bridge.last_summary, None, "not yet: it may not last");
            assert_eq!(bridge.unsettled, Some((Summary::Online, start)));
            bridge.poll(vec![on.clone()], start + seconds(4));
            assert_eq!(bridge.last_summary, None);
            bridge.poll(vec![on.clone()], start + SUMMARY_SETTLE);
            assert_eq!(bridge.last_summary, Some(Summary::Online));
            assert_eq!(bridge.unsettled, None);

            // The dongle drops off the bus for a moment: nothing said.
            bridge.poll(vec![], start + seconds(6));
            assert_eq!(
                bridge.unsettled,
                Some((Summary::NoDevice, start + seconds(6)))
            );
            bridge.poll(vec![on], start + seconds(8));
            assert_eq!(bridge.unsettled, None);
            assert_eq!(bridge.last_summary, Some(Summary::Online));
        }

        #[test]
        fn every_battery_is_withdrawn_at_shutdown_and_the_manager_is_told() {
            let mut bridge = bridge();
            let start = Instant::now();
            let other = Headset {
                product_id: 0x4b19,
                ..maxwell(BatteryState::Discharging(50))
            };
            bridge.poll(vec![maxwell(BatteryState::Discharging(80)), other], start);
            assert_eq!(bridge.batteries.len(), 2);
            bridge.drain(0);
            bridge.drain(1);

            // A stop already raised: `run` makes no tick, and still takes
            // every device down, telling the manager on the way.
            let (manager, address) = {
                use std::os::linux::net::SocketAddrExt;
                use std::os::unix::net::{SocketAddr, UnixDatagram};
                let name = format!("headset-battery-indicator-test-{}-run", std::process::id());
                let addr = SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
                let socket = UnixDatagram::bind_addr(&addr).unwrap();
                socket.set_nonblocking(true).unwrap();
                (socket, std::ffi::OsString::from(format!("@{name}")))
            };
            let notify = Notifier::to(&address).unwrap();
            bridge.run(&AtomicBool::new(true), &notify).unwrap();
            assert_eq!(bridge.batteries.len(), 0);
            assert_eq!(bridge.kernel(0).sent_all(), [Sent::Destroy]);
            assert_eq!(bridge.kernel(1).sent_all(), [Sent::Destroy]);

            let mut buf = [0u8; 64];
            let mut heard = Vec::new();
            while let Ok(n) = manager.recv(&mut buf) {
                heard.push(String::from_utf8_lossy(&buf[..n]).into_owned());
            }
            assert_eq!(heard, ["READY=1\n", "STOPPING=1\n"]);
        }

        #[test]
        fn the_pool_hands_out_inherited_handles_first_and_never_opens_the_node_then() {
            let (_kernel_a, a) = Kernel::new();
            let (_kernel_b, b) = Kernel::new();
            let mut pool = DevicePool::new(vec![a, b], PathBuf::from("/nonexistent/uhid"));
            let first = pool.acquire().unwrap();
            let second = pool.acquire().unwrap();
            // Both inherited ones are out: the node is not opened, even
            // though opening it would fail here anyway - the message says
            // what to do about it.
            let err = pool.acquire().unwrap_err();
            assert!(err.to_string().contains("OpenFile="), "{err:#}");
            // Given back, a handle is handed out again.
            pool.put_back(first);
            drop(pool.acquire().unwrap());
            drop(second);

            // With nothing inherited the node is opened, and that fails here.
            let mut own = DevicePool::new(Vec::new(), PathBuf::from("/nonexistent/uhid"));
            let err = own.acquire().unwrap_err();
            assert!(
                err.to_string().contains("opening /nonexistent/uhid"),
                "{err:#}"
            );
        }
    }
}
