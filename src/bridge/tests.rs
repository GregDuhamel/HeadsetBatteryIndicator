//! The bridge's tests: the summary of a poll, the loop's sizing, and - in
//! [`live`] - the whole thing driven over a scripted source and a fake kernel.

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
fn the_watchdog_outlasts_the_slowest_turn_of_the_loop() {
    // The unit's WatchdogSec= is sized from the loop (see MAX_WAIT): a
    // wait, a HeadsetControl reading at the unit's --timeout, and a
    // device registration. Keep the two in step.
    const UNIT: &str = include_str!("../../packaging/systemd/headset-battery-indicator.service");
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
    fn while_discharging_the_gauge_settling_upward_never_reaches_the_kernel() {
        let mut bridge = bridge();
        let start = Instant::now();

        // Observed on 2026-10-09, after the headset woke: 59%, 60%, 62%,
        // 59% within two minutes, then 59% for good. Only the 59% is
        // pushed, once.
        bridge.poll(vec![sampled(59, 1)], start);
        assert_eq!(bridge.kernel(0).sent_all()[1..], [report(59, false)]);
        bridge.poll(vec![sampled(60, 2)], start + seconds(30));
        bridge.poll(vec![sampled(62, 3)], start + seconds(50));
        assert_eq!(bridge.kernel(0).sent_all(), []);
        assert_eq!(bridge.batteries[0].held, Some(62));
        assert_eq!(bridge.batteries[0].deferred, None, "it is not a glitch");

        // The periodic republish sends the published level, not the one
        // being held - the gauge is still saying 62%.
        bridge.poll(vec![sampled(62, 3)], start + REPUBLISH_EVERY);
        assert_eq!(bridge.kernel(0).sent_all(), [report(59, false)]);
        bridge.poll(vec![sampled(59, 4)], start + seconds(90));
        assert_eq!(bridge.kernel(0).sent_all(), []);
        assert_eq!(bridge.batteries[0].held, None);

        // The discharge itself goes through at once, as before.
        bridge.poll(vec![sampled(58, 5)], start + seconds(100));
        assert_eq!(bridge.kernel(0).sent_all(), [report(58, false)]);
        assert_eq!(bridge.batteries.len(), 1);
    }

    #[test]
    fn a_charge_lets_the_level_rise_again() {
        let mut bridge = bridge();
        let start = Instant::now();
        bridge.poll(vec![maxwell(BatteryState::Discharging(59))], start);
        bridge.drain(0);

        // Plugged in, no level reported: the last level, charging. Then
        // unplugged, fuller: the first reading off the cable is taken whole.
        bridge.poll(
            vec![maxwell(BatteryState::Charging(None))],
            start + seconds(1),
        );
        assert_eq!(bridge.kernel(0).sent_all(), [report(59, true)]);
        bridge.poll(
            vec![maxwell(BatteryState::Discharging(72))],
            start + seconds(2),
        );
        assert_eq!(bridge.kernel(0).sent_all(), [report(72, false)]);
        // And from there the rule applies again.
        bridge.poll(
            vec![maxwell(BatteryState::Discharging(74))],
            start + seconds(3),
        );
        assert_eq!(bridge.kernel(0).sent_all(), []);
        assert_eq!(bridge.batteries[0].held, Some(74));
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
