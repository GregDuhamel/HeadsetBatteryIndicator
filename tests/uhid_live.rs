//! Acceptance test for the real thing: create a virtual battery, then read it
//! back from sysfs the way UPower does.
//!
//! It needs write access to `/dev/uhid`, which the node grants to root only, so
//! it is ignored by default. Run it with:
//!
//! ```sh
//! cargo test --test uhid_live --no-run
//! sudo target/debug/deps/uhid_live-* --ignored --nocapture
//! ```

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::thread::{self, sleep};
use std::time::{Duration, Instant};

use headset_battery_indicator::uhid::{self, CreateParams, Event, Uhid};

/// The device side of the conversation, run from its own thread.
///
/// Reading a sysfs attribute can make the kernel send us a `GET_REPORT` and
/// wait up to five seconds for the answer, so whoever reads sysfs must not be
/// the one who answers - in production they are different processes. The same
/// thread keeps pushing the level, because the kernel silently drops input
/// reports until it has finished probing the device.
struct Responder {
    stop: Arc<AtomicBool>,
    level: Arc<AtomicU8>,
    charging: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Responder {
    fn start(device: Arc<Uhid>, level: u8, charging: bool) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let level = Arc::new(AtomicU8::new(level));
        let charging = Arc::new(AtomicBool::new(charging));

        let thread = {
            let (stop, level, charging) = (stop.clone(), level.clone(), charging.clone());
            thread::spawn(move || {
                let mut last_push: Option<Instant> = None;
                while !stop.load(Ordering::Relaxed) {
                    let report = uhid::battery_report(
                        level.load(Ordering::Relaxed),
                        charging.load(Ordering::Relaxed),
                    );
                    while let Ok(Some(event)) = device.read_event() {
                        match event {
                            Event::GetReport { id, .. } => {
                                let _ = device.reply_get_report(id, &report);
                            }
                            Event::SetReport { id } => {
                                let _ = device.reply_set_report(id);
                            }
                            _ => {}
                        }
                    }
                    if last_push.is_none_or(|at| at.elapsed() >= Duration::from_millis(200)) {
                        let _ = device.send_input(&report);
                        last_push = Some(Instant::now());
                    }
                    sleep(Duration::from_millis(10));
                }
            })
        };

        Self {
            stop,
            level,
            charging,
            thread: Some(thread),
        }
    }

    fn set(&self, level: u8, charging: bool) {
        self.level.store(level, Ordering::Relaxed);
        self.charging.store(charging, Ordering::Relaxed);
    }
}

impl Drop for Responder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Polls until `check` returns a value, or gives up after `timeout`.
fn wait_for<T>(timeout: Duration, mut check: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = check() {
            return Some(value);
        }
        if Instant::now() >= deadline {
            return None;
        }
        sleep(Duration::from_millis(50));
    }
}

fn attr(base: &Path, name: &str) -> Option<String> {
    fs::read_to_string(base.join(name))
        .ok()
        .map(|value| value.trim().to_owned())
}

/// Waits until a sysfs attribute reads `expected`.
fn expect_attr(base: &Path, name: &str, expected: &str) {
    let seen = wait_for(Duration::from_secs(10), || {
        attr(base, name).filter(|value| value == expected)
    });
    assert_eq!(
        seen.as_deref(),
        Some(expected),
        "{name} should read {expected:?}, last saw {:?}",
        attr(base, name)
    );
}

/// Asks UPower whether it noticed the device. Informational: UPower may not be
/// running, and the sysfs assertions are what actually matter.
fn report_upower(uniq: &str) {
    let wanted = uniq.replace('-', "_");
    let found = wait_for(Duration::from_secs(10), || {
        let output = Command::new("upower").arg("-e").output().ok()?;
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .find(|line| line.contains(&wanted))
            .map(str::to_owned)
    });

    match found {
        Some(path) => {
            eprintln!("UPower sees the device at {path}");
            if let Ok(details) = Command::new("upower").args(["-i", &path]).output() {
                eprintln!("{}", String::from_utf8_lossy(&details.stdout));
            }
        }
        None => eprintln!("note: UPower did not list the device within 10s"),
    }
}

#[test]
#[ignore = "needs write access to /dev/uhid; run as root"]
fn the_kernel_publishes_our_battery_to_sysfs() {
    let device = Arc::new(Uhid::open(Path::new(uhid::DEV_UHID)).expect(
        "opening /dev/uhid: this test must run as root, or with the descriptor passed by systemd",
    ));

    // Unique enough to survive a leftover device from a previous run.
    let uniq = format!("hbi-test-{}", std::process::id());
    let params = CreateParams {
        name: "Test Headset".to_owned(),
        phys: "headset-battery-indicator/test".to_owned(),
        uniq: uniq.clone(),
        vendor: 0x3329,
        product: 0x4b18,
    };
    device
        .create(&params, uhid::REPORT_DESCRIPTOR)
        .expect("creating the virtual device");
    let responder = Responder::start(Arc::clone(&device), 42, true);

    let base: PathBuf = wait_for(Duration::from_secs(5), || uhid::find_power_supply(&uniq))
        .expect("the kernel should have registered a power supply (see `journalctl -k`)");
    eprintln!("the kernel registered {}", base.display());

    expect_attr(&base, "capacity", "42");
    expect_attr(&base, "status", "Charging");
    // UPower must see a peripheral battery, not a system one.
    expect_attr(&base, "scope", "Device");
    // This is the label KDE shows.
    expect_attr(&base, "model_name", "Test Headset");

    report_upower(&uniq);

    responder.set(17, false);
    expect_attr(&base, "capacity", "17");
    expect_attr(&base, "status", "Discharging");

    drop(responder);
    device.destroy().expect("destroying the virtual device");
    let gone = wait_for(Duration::from_secs(5), || (!base.exists()).then_some(()));
    assert!(gone.is_some(), "the power supply should be gone: {base:?}");
}
