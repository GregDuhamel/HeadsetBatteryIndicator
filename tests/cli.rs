//! End-to-end checks of the command line, driven by a stub `headsetcontrol`.
//!
//! The stub keeps these runnable on a machine with no headset — and in CI.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_headset-battery-indicator");

/// The binary, pinned to the HeadsetControl backend: with `auto`, a machine
/// that has a real Maxwell plugged in would answer natively and never reach
/// the stub, making these tests depend on the hardware they run on.
fn bin() -> Command {
    let mut command = Command::new(BIN);
    command.args(["--backend", "headsetcontrol"]);
    command
}

/// Writes an executable stub that prints `payload` and exits.
fn stub(name: &str, body: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    fs::create_dir_all(&dir).expect("creating the stub directory");

    let path = dir.join("headsetcontrol");
    fs::write(&path, body).expect("writing the stub");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
        .expect("making the stub runnable");
    path
}

fn printing_stub(name: &str, fixture: &str) -> PathBuf {
    let payload = fs::read_to_string(Path::new("tests/fixtures").join(fixture)).expect("fixture");
    stub(name, &format!("#!/bin/sh\ncat <<'JSON'\n{payload}\nJSON\n"))
}

#[test]
fn status_reports_the_battery_level() {
    let output = bin()
        .args(["--headsetcontrol"])
        .arg(printing_stub("status-ok", "maxwell-discharging.json"))
        .arg("status")
        .output()
        .expect("running the binary");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Audeze Maxwell [3329:4b18]"), "{stdout}");
    assert!(stdout.contains("battery: 73%"), "{stdout}");
    assert!(stdout.contains("hid-headset-3329-4b18-battery"), "{stdout}");
}

#[test]
fn status_says_when_the_headset_is_off() {
    let output = bin()
        .args(["--headsetcontrol"])
        .arg(printing_stub("status-off", "maxwell-unavailable.json"))
        .arg("status")
        .output()
        .expect("running the binary");

    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("unavailable"));
}

#[test]
fn udev_rules_are_generated_for_the_detected_headset() {
    let output = bin()
        .args(["--headsetcontrol"])
        .arg(printing_stub("udev", "maxwell-charging.json"))
        .args(["udev-rules", "--group", "gamers"])
        .output()
        .expect("running the binary");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(
            r#"KERNEL=="hidraw*", ATTRS{idVendor}=="3329", ATTRS{idProduct}=="4b18", GROUP="gamers", MODE="0660""#
        ),
        "{stdout}"
    );
    // The rule that turns the generic battery into a headset for UPower.
    assert!(
        stdout.contains(r#"ENV{SOUND_FORM_FACTOR}="headset""#),
        "{stdout}"
    );
}

#[test]
fn a_missing_headsetcontrol_is_an_error_not_a_panic() {
    let output = bin()
        .args(["--headsetcontrol", "/nonexistent/headsetcontrol", "status"])
        .output()
        .expect("running the binary");

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("could not run"));
}

#[test]
fn garbage_output_is_reported_with_context() {
    let path = stub("garbage", "#!/bin/sh\necho 'segmentation fault'\n");
    let output = bin()
        .arg("--headsetcontrol")
        .arg(path)
        .arg("status")
        .output()
        .expect("running the binary");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unexpected output"), "{stderr}");
}

#[test]
fn a_hanging_headsetcontrol_is_killed() {
    let path = stub("hang", "#!/bin/sh\nsleep 30\n");
    let started = std::time::Instant::now();
    let output = bin()
        .arg("--headsetcontrol")
        .arg(path)
        .args(["--timeout", "1", "status"])
        .output()
        .expect("running the binary");

    assert!(!output.status.success());
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    assert!(String::from_utf8_lossy(&output.stderr).contains("did not answer"));
}
