//! Descriptors inherited from the service manager.
//!
//! The unit shipped with this project never grants the daemon access to
//! `/dev/uhid`. Instead systemd opens the node itself and passes the
//! descriptor down (`OpenFile=/dev/uhid:uhid`), which is what lets the process
//! run as an unprivileged `DynamicUser=` with no capabilities while the node
//! stays `root:root 0600`.
//!
//! This is the `sd_listen_fds()` protocol, reimplemented so the crate does not
//! need libsystemd: the manager exports `LISTEN_PID`, `LISTEN_FDS` and
//! `LISTEN_FDNAMES`, and the descriptors start at `SD_LISTEN_FDS_START`.

use std::env;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

use log::debug;
use rustix::io::FdFlags;
use rustix::io::fcntl_setfd;

/// First descriptor number passed by the service manager.
const LISTEN_FDS_START: RawFd = 3;

/// Name given to the `/dev/uhid` descriptor in the unit file.
pub const UHID_FD_NAME: &str = "uhid";

/// Takes ownership of the descriptors the service manager passed for `name`.
///
/// Returns an empty vector when the process was not started by systemd, or
/// when no descriptor carries that name. The environment variables are removed
/// so a re-entrant call — or a child process — cannot claim the same
/// descriptors twice.
#[must_use]
pub fn take_fds(name: &str) -> Vec<OwnedFd> {
    let Some(count) = listen_fds_count() else {
        return Vec::new();
    };

    let names = env::var("LISTEN_FDNAMES").unwrap_or_default();
    let mut names = names.split(':');

    // SAFETY: the descriptors in `LISTEN_FDS_START..LISTEN_FDS_START + count`
    // belong to this process by the systemd protocol, this is the only place
    // that adopts them, and the variables are cleared below so nothing takes
    // them a second time.
    #[allow(unsafe_code)]
    let fds = (0..count)
        .filter_map(|offset| {
            let fd_name = names.next().unwrap_or_default();
            if fd_name == name {
                Some(unsafe { OwnedFd::from_raw_fd(LISTEN_FDS_START + offset) })
            } else {
                debug!("ignoring inherited descriptor named {fd_name:?}");
                None
            }
        })
        .collect::<Vec<_>>();

    // Inherited descriptors come without FD_CLOEXEC; set it so the
    // `headsetcontrol` children we spawn never see /dev/uhid.
    for fd in &fds {
        if let Err(err) = fcntl_setfd(fd, FdFlags::CLOEXEC) {
            debug!("could not set FD_CLOEXEC on an inherited descriptor: {err}");
        }
    }

    unset_listen_vars();
    fds
}

fn listen_fds_count() -> Option<RawFd> {
    let listen_pid: u32 = env::var("LISTEN_PID").ok()?.parse().ok()?;
    if listen_pid != std::process::id() {
        debug!("LISTEN_PID={listen_pid} is not us, ignoring inherited descriptors");
        unset_listen_vars();
        return None;
    }

    let count: RawFd = env::var("LISTEN_FDS").ok()?.parse().ok()?;
    (count > 0).then_some(count)
}

fn unset_listen_vars() {
    // SAFETY-adjacent note: this runs before any thread is spawned, which is
    // the only way `remove_var` can be misused.
    for key in ["LISTEN_PID", "LISTEN_FDS", "LISTEN_FDNAMES"] {
        // SAFETY: single-threaded at this point in the program.
        #[allow(unsafe_code)]
        unsafe {
            env::remove_var(key);
        }
    }
}
