//! End-to-end checks of the command line, driven by a stub `headsetcontrol`.
//!
//! The stub keeps these runnable on a machine with no headset — and in CI.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard};

const BIN: &str = env!("CARGO_BIN_EXE_headset-battery-indicator");

/// The binary, pinned to the HeadsetControl backend: with `auto`, a machine
/// that has a real Maxwell plugged in would answer natively and never reach
/// the stub, making these tests depend on the hardware they run on.
fn bin() -> Command {
    let mut command = Command::new(BIN);
    command.args(["--backend", "headsetcontrol"]);
    command
}

/// Serialises the tests below.
///
/// Each writes a script and has the binary under test execute it. Run in
/// parallel, one thread forks while another still holds its script open for
/// writing; the child inherits that descriptor for an instant, and the kernel
/// refuses to execute a file that is open for writing anywhere: `ETXTBSY`,
/// "Text file busy". It only shows up now and then, which is worse than always.
static EXEC_LOCK: Mutex<()> = Mutex::new(());

/// Takes the lock. Every test that forks needs it, stub or no stub: it is the
/// forking thread that ends up holding somebody else's script open.
fn serialise() -> MutexGuard<'static, ()> {
    EXEC_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// An executable stub, and the lock that keeps other tests from forking while
/// it exists.
struct Stub {
    path: PathBuf,
    _serialised: MutexGuard<'static, ()>,
}

/// Writes an executable stub that runs `body`.
fn stub(name: &str, body: &str) -> Stub {
    let serialised = serialise();

    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    fs::create_dir_all(&dir).expect("creating the stub directory");

    let path = dir.join("headsetcontrol");
    fs::write(&path, body).expect("writing the stub");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
        .expect("making the stub runnable");
    Stub {
        path,
        _serialised: serialised,
    }
}

fn printing_stub(name: &str, fixture: &str) -> Stub {
    let payload = fs::read_to_string(Path::new("tests/fixtures").join(fixture)).expect("fixture");
    stub(name, &format!("#!/bin/sh\ncat <<'JSON'\n{payload}\nJSON\n"))
}

#[test]
fn status_reports_the_battery_level() {
    let stub = printing_stub("status-ok", "maxwell-discharging.json");
    let output = bin()
        .args(["--headsetcontrol"])
        .arg(&stub.path)
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
    // Either the path of the published supply or a note that there is none,
    // depending on whether a daemon happens to run on this machine.
    assert!(stdout.contains("  sysfs:   "), "{stdout}");
}

#[test]
fn status_says_when_the_headset_is_off() {
    let stub = printing_stub("status-off", "maxwell-unavailable.json");
    let output = bin()
        .args(["--headsetcontrol"])
        .arg(&stub.path)
        .arg("status")
        .output()
        .expect("running the binary");

    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("unavailable"));
}

#[test]
fn udev_rules_are_generated_for_the_detected_headset() {
    let stub = printing_stub("udev", "maxwell-charging.json");
    let output = bin()
        .args(["--headsetcontrol"])
        .arg(&stub.path)
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
    let _serialised = serialise();
    let output = bin()
        .args(["--headsetcontrol", "/nonexistent/headsetcontrol", "status"])
        .output()
        .expect("running the binary");

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("could not run"));
}

#[test]
fn garbage_output_is_reported_with_context() {
    let stub = stub("garbage", "#!/bin/sh\necho 'segmentation fault'\n");
    let output = bin()
        .arg("--headsetcontrol")
        .arg(&stub.path)
        .arg("status")
        .output()
        .expect("running the binary");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unexpected output"), "{stderr}");
}

#[test]
fn a_hanging_headsetcontrol_is_killed() {
    let stub = stub("hang", "#!/bin/sh\nsleep 30\n");
    let started = std::time::Instant::now();
    let output = bin()
        .arg("--headsetcontrol")
        .arg(&stub.path)
        .args(["--timeout", "1", "status"])
        .output()
        .expect("running the binary");

    assert!(!output.status.success());
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    assert!(String::from_utf8_lossy(&output.stderr).contains("did not answer"));
}
