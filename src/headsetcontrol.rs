//! Runs the `headsetcontrol` binary and turns its JSON output into something
//! this crate can work with.
//!
//! Shelling out rather than linking against HeadsetControl is deliberate: the
//! per-headset protocol knowledge stays upstream, where it is maintained, and
//! this daemon only has to track a stable JSON shape.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, sleep};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::headset::{BatteryState, Headset};

/// Interval between two checks on a running `headsetcontrol` child. It bounds
/// how long a stop request waits for the child, and how much later than its
/// exit the child is noticed; a report takes seconds, so both are noise.
const REAP_INTERVAL: Duration = Duration::from_millis(25);

/// How much of the child's output an error message quotes.
const QUOTED_OUTPUT: usize = 200;

/// A configured `headsetcontrol` invocation.
#[derive(Debug, Clone)]
pub struct HeadsetControl {
    binary: PathBuf,
    timeout: Duration,
}

impl HeadsetControl {
    /// Binds to a `headsetcontrol` binary.
    #[must_use]
    pub fn new(binary: PathBuf, timeout: Duration) -> Self {
        Self { binary, timeout }
    }

    /// Path of the binary being invoked.
    #[must_use]
    pub fn binary(&self) -> &Path {
        &self.binary
    }

    /// Asks HeadsetControl for the battery state of every connected headset.
    ///
    /// `stop` is looked at while the child runs: a daemon that was asked to
    /// shut down should not sit out the whole timeout first.
    ///
    /// # Errors
    ///
    /// Fails if the binary cannot be spawned, does not finish within the
    /// configured timeout, is cut short by `stop`, or writes something other
    /// than the expected JSON.
    pub fn probe(&self, stop: &AtomicBool) -> Result<Vec<Headset>> {
        let output = self.run(&["--battery", "--output", "json"], stop)?;
        parse(&output.stdout).with_context(|| {
            // What the child said on stderr is usually the actual explanation
            // ("device not found", a missing library), so quote it too.
            let stderr = output.stderr.trim();
            let stderr = if stderr.is_empty() {
                String::new()
            } else {
                format!(" (stderr: {})", quote(stderr))
            };
            format!(
                "unexpected output from {}: {}{stderr}",
                self.binary.display(),
                quote(&output.stdout)
            )
        })
    }

    /// Runs the binary to completion, or kills it at the timeout or when
    /// `stop` is raised.
    ///
    /// Both pipes are drained by threads while the child runs. Waiting for
    /// the exit first and reading afterwards would deadlock on a child that
    /// writes more than a pipe holds (64 KiB): it blocks on the full pipe,
    /// and nobody empties it until it exits.
    fn run(&self, args: &[&str], stop: &AtomicBool) -> Result<Output> {
        let mut child = Command::new(&self.binary)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("could not run {}", self.binary.display()))?;

        let stdout = drain(child.stdout.take());
        let stderr = drain(child.stderr.take());

        let deadline = Instant::now() + self.timeout;
        loop {
            if child
                .try_wait()
                .context("waiting for headsetcontrol")?
                .is_some()
            {
                break;
            }
            if stop.load(Ordering::Relaxed) {
                kill(&mut child);
                bail!("stopped while waiting for {}", self.binary.display());
            }
            if Instant::now() >= deadline {
                kill(&mut child);
                bail!(
                    "{} did not answer within {:?}",
                    self.binary.display(),
                    self.timeout
                );
            }
            sleep(REAP_INTERVAL);
        }

        // The child is gone, so its end of each pipe is closed and the
        // readers are finishing. (A grandchild left holding a pipe would
        // keep them going, as it kept `wait_with_output()` going before;
        // headsetcontrol forks nothing.)
        Ok(Output {
            stdout: stdout.join().unwrap_or_default(),
            stderr: stderr.join().unwrap_or_default(),
        })
    }
}

/// What a finished child wrote.
struct Output {
    stdout: String,
    stderr: String,
}

/// Reads a pipe to its end on a thread of its own, lossily as text.
fn drain(pipe: Option<impl Read + Send + 'static>) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            // A read error ends the stream like EOF; whatever came before
            // is still worth quoting.
            let _ = pipe.read_to_end(&mut bytes);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

/// Kills a child that outstayed its welcome and reaps it.
///
/// The reader threads are left to themselves rather than joined: a shell
/// wrapper that forked `headsetcontrol` instead of exec'ing it dies here while
/// its child keeps the pipes open, and joining would wait for that grandchild.
/// They end on their own once the last writer goes.
fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// The first [`QUOTED_OUTPUT`] characters of some output, for an error message.
fn quote(output: &str) -> String {
    output.trim().chars().take(QUOTED_OUTPUT).collect()
}

#[derive(Debug, Deserialize)]
struct Report {
    #[serde(default)]
    devices: Vec<Device>,
}

#[derive(Debug, Deserialize)]
struct Device {
    #[serde(default, rename = "device")]
    model: String,
    #[serde(default)]
    product: String,
    #[serde(default)]
    id_vendor: String,
    #[serde(default)]
    id_product: String,
    #[serde(default)]
    capabilities: Vec<String>,
    #[serde(default)]
    battery: Option<Battery>,
}

#[derive(Debug, Deserialize)]
struct Battery {
    #[serde(default)]
    status: String,
    #[serde(default)]
    level: i32,
}

/// Parses the JSON document produced by `headsetcontrol --output json`.
///
/// # Errors
///
/// Fails if the payload is not the JSON document HeadsetControl documents.
pub fn parse(stdout: &str) -> Result<Vec<Headset>> {
    // HeadsetControl occasionally prefixes its JSON with a plain-text notice,
    // so start at the first brace rather than trusting the whole stream.
    let json = stdout
        .find('{')
        .map_or(stdout, |start| &stdout[start..])
        .trim();
    if json.is_empty() {
        return Ok(Vec::new());
    }

    let report: Report = serde_json::from_str(json).context("parsing JSON report")?;
    Ok(report.devices.into_iter().map(Headset::from).collect())
}

impl From<Device> for Headset {
    fn from(device: Device) -> Self {
        let supports_battery = device
            .capabilities
            .iter()
            .any(|capability| capability == "CAP_BATTERY_STATUS");
        let battery = device
            .battery
            .as_ref()
            .map_or(BatteryState::Unavailable, battery_state);

        Self {
            name: device.model,
            product: device.product,
            vendor_id: parse_id(&device.id_vendor),
            product_id: parse_id(&device.id_product),
            supports_battery,
            battery,
            // Every probe runs the binary again: each reading is a new one.
            sample: None,
        }
    }
}

fn battery_state(battery: &Battery) -> BatteryState {
    let level = u8::try_from(battery.level)
        .ok()
        .filter(|level| *level <= 100);
    match battery.status.as_str() {
        "BATTERY_AVAILABLE" => level.map_or(BatteryState::Unavailable, BatteryState::Discharging),
        "BATTERY_CHARGING" => BatteryState::Charging(level),
        _ => BatteryState::Unavailable,
    }
}

/// Parses the `0x3329` form HeadsetControl uses for USB identifiers.
fn parse_id(raw: &str) -> u16 {
    let trimmed = raw.trim();
    let digits = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .unwrap_or(trimmed);
    u16::from_str_radix(digits, 16).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAXWELL: &str = include_str!("../tests/fixtures/maxwell-unavailable.json");
    const MAXWELL_CHARGING: &str = include_str!("../tests/fixtures/maxwell-charging.json");
    const MAXWELL_DISCHARGING: &str = include_str!("../tests/fixtures/maxwell-discharging.json");

    #[test]
    fn parses_a_connected_headset() {
        let headsets = parse(MAXWELL_DISCHARGING).expect("valid report");
        assert_eq!(headsets.len(), 1);

        let headset = &headsets[0];
        assert_eq!(headset.name, "Audeze Maxwell");
        assert_eq!(headset.product, "Audeze Maxwell XBOX Dongle");
        assert_eq!(headset.vendor_id, 0x3329);
        assert_eq!(headset.product_id, 0x4b18);
        assert!(headset.supports_battery);
        assert_eq!(headset.battery, BatteryState::Discharging(73));
        assert_eq!(headset.key(), "3329:4b18/Audeze Maxwell");
        assert_eq!(headset.uniq(), "headset-3329-4b18");
    }

    #[test]
    fn parses_a_charging_headset() {
        let headsets = parse(MAXWELL_CHARGING).expect("valid report");
        assert_eq!(headsets[0].battery, BatteryState::Charging(Some(64)));
    }

    #[test]
    fn treats_an_offline_headset_as_unavailable() {
        let headsets = parse(MAXWELL).expect("valid report");
        assert_eq!(headsets[0].battery, BatteryState::Unavailable);
        assert!(headsets[0].supports_battery);
    }

    #[test]
    fn charging_without_a_level_is_still_charging() {
        let json =
            r#"{"devices":[{"device":"X","battery":{"status":"BATTERY_CHARGING","level":-1}}]}"#;
        assert_eq!(
            parse(json).unwrap()[0].battery,
            BatteryState::Charging(None)
        );
    }

    #[test]
    fn tolerates_a_text_preamble_and_an_empty_document() {
        let json = "No config file found\n{\"devices\":[]}";
        assert!(parse(json).unwrap().is_empty());
        assert!(parse("").unwrap().is_empty());
        assert!(parse("   \n").unwrap().is_empty());
    }

    #[test]
    fn rejects_output_that_is_not_the_expected_document() {
        assert!(parse("{ this is not json }").is_err());
    }

    #[test]
    fn devices_without_a_battery_capability_are_flagged() {
        let json = r#"{"devices":[{"device":"X","capabilities":["CAP_SIDETONE"]}]}"#;
        let headsets = parse(json).unwrap();
        assert!(!headsets[0].supports_battery);
        assert_eq!(headsets[0].battery, BatteryState::Unavailable);
    }

    #[test]
    fn parses_usb_identifiers() {
        assert_eq!(parse_id("0x3329"), 0x3329);
        assert_eq!(parse_id("3329"), 0x3329);
        assert_eq!(parse_id(""), 0);
        assert_eq!(parse_id("nonsense"), 0);
    }

    #[test]
    fn out_of_range_levels_are_dropped() {
        let json =
            r#"{"devices":[{"device":"X","battery":{"status":"BATTERY_AVAILABLE","level":120}}]}"#;
        assert_eq!(parse(json).unwrap()[0].battery, BatteryState::Unavailable);
    }
}
