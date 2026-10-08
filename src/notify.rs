//! `sd_notify(3)` without libsystemd: telling the service manager that the
//! daemon is up (`READY=1`), still alive (`WATCHDOG=1`) and on its way out
//! (`STOPPING=1`).
//!
//! The protocol is one datagram per message on the `AF_UNIX` socket named by
//! `NOTIFY_SOCKET`: a filesystem path, or an abstract name when the value
//! starts with `@`. The messages carry no credentials of their own; the
//! manager trusts the sender's PID, and only the main process's by default
//! (`NotifyAccess=main`). Run by hand, with no `NOTIFY_SOCKET` in the
//! environment, every method is a no-op, so nothing here needs to know
//! whether systemd is watching.
//!
//! This is the sending half of what [`uhid_battery::listen_fds`] receives,
//! and it is written the same way: `std` only, no `unsafe`. `sd_notify(3)`
//! can also remove `NOTIFY_SOCKET` from the environment so that children do
//! not inherit it; this does not, because editing the environment is a data
//! race once a thread exists and the daemon spawns some. A `headsetcontrol`
//! child that inherits the variable gets nowhere with it: its PID is not the
//! one the manager accepts messages from.

use std::env;
use std::ffi::OsStr;
use std::io;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use log::{debug, warn};

/// The variable the service manager leaves the socket's address in.
pub const NOTIFY_SOCKET: &str = "NOTIFY_SOCKET";

/// The messages this daemon sends, as `sd_notify(3)` spells them. Each is one
/// line; the trailing newline is the convention, not a requirement.
const READY: &str = "READY=1\n";
const WATCHDOG: &str = "WATCHDOG=1\n";
const STOPPING: &str = "STOPPING=1\n";

/// A connection to the service manager's notification socket, or nothing.
#[derive(Debug)]
pub struct Notifier {
    socket: Option<UnixDatagram>,
    /// Whether a message has failed to go out already. The first failure is
    /// worth a warning; the rest of the streak is not, as the watchdog ping
    /// comes round every few seconds for as long as the daemon runs.
    complained: AtomicBool,
}

impl Notifier {
    /// Connects to the socket `NOTIFY_SOCKET` names, or does nothing from
    /// here on when the variable is absent (or empty, which systemd treats
    /// the same).
    ///
    /// A variable that is set but names a socket that cannot be reached is
    /// logged and then ignored like an absent one: the daemon has its job to
    /// do either way, and a `Type=notify` unit will say so when `READY=1`
    /// never arrives.
    #[must_use]
    pub fn from_env() -> Self {
        let Some(address) = env::var_os(NOTIFY_SOCKET).filter(|value| !value.is_empty()) else {
            return Self::silent();
        };
        match Self::to(&address) {
            Ok(notifier) => notifier,
            Err(err) => {
                warn!(
                    "cannot reach the service manager's notification socket {}: {err}",
                    Path::new(&address).display()
                );
                Self::silent()
            }
        }
    }

    /// A notifier that sends nothing: no service manager is listening.
    #[must_use]
    pub const fn silent() -> Self {
        Self {
            socket: None,
            complained: AtomicBool::new(false),
        }
    }

    /// Connects to the socket at `address`, in the `NOTIFY_SOCKET` syntax: a
    /// path, or `@name` for an abstract socket.
    ///
    /// Connecting up front, rather than addressing every datagram, is what
    /// `sd_notify(3)` does not do - it binds nothing and uses `sendmsg` with
    /// the address each time - but it makes a wrong address fail here, once,
    /// instead of on every ping. systemd's own socket is a path
    /// (`/run/systemd/notify`), which stays reachable from a unit with
    /// `PrivateNetwork=`; an abstract name lives in the network namespace
    /// and would not.
    ///
    /// # Errors
    ///
    /// Fails if the socket cannot be created or connected, or if an abstract
    /// name is too long for one.
    pub fn to(address: &OsStr) -> io::Result<Self> {
        let socket = UnixDatagram::unbound()?;
        match address.as_bytes().strip_prefix(b"@") {
            Some(name) => socket.connect_addr(&SocketAddr::from_abstract_name(name)?)?,
            None => socket.connect(Path::new(address))?,
        }
        Ok(Self {
            socket: Some(socket),
            complained: AtomicBool::new(false),
        })
    }

    /// Whether messages go anywhere.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.socket.is_some()
    }

    /// `READY=1`: the daemon is up. A `Type=notify` unit is "activating"
    /// until this arrives, and fails after `TimeoutStartSec=` without it.
    pub fn ready(&self) {
        self.send(READY);
    }

    /// `WATCHDOG=1`: still alive. A unit with `WatchdogSec=` is killed and
    /// restarted when this stops coming.
    pub fn watchdog(&self) {
        self.send(WATCHDOG);
    }

    /// `STOPPING=1`: shutting down on purpose, so the manager shows
    /// "deactivating" rather than "active" for the last moments.
    pub fn stopping(&self) {
        self.send(STOPPING);
    }

    fn send(&self, message: &str) {
        let Some(socket) = &self.socket else {
            return;
        };
        match socket.send(message.as_bytes()) {
            Ok(_) => {}
            Err(err) if self.complained.swap(true, Ordering::Relaxed) => {
                debug!("still cannot notify the service manager: {err}");
            }
            Err(err) => {
                warn!(
                    "cannot notify the service manager ({}): {err}",
                    message.trim_end()
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    /// A listening socket standing in for the manager, and the address that
    /// reaches it in `NOTIFY_SOCKET` syntax. Abstract, so that nothing is
    /// left on the filesystem; named after the test, so that the tests do
    /// not hear each other.
    fn manager(name: &str) -> (UnixDatagram, OsString) {
        let name = format!(
            "headset-battery-indicator-test-{}-{name}",
            std::process::id()
        );
        let address = SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let socket = UnixDatagram::bind_addr(&address).unwrap();
        socket.set_nonblocking(true).unwrap();
        (socket, OsString::from(format!("@{name}")))
    }

    fn received(socket: &UnixDatagram) -> Vec<String> {
        let mut messages = Vec::new();
        let mut buf = [0u8; 64];
        while let Ok(n) = socket.recv(&mut buf) {
            messages.push(String::from_utf8_lossy(&buf[..n]).into_owned());
        }
        messages
    }

    #[test]
    fn the_three_messages_reach_an_abstract_socket_one_datagram_each() {
        let (socket, address) = manager("abstract");
        let notifier = Notifier::to(&address).unwrap();
        assert!(notifier.is_connected());
        notifier.ready();
        notifier.watchdog();
        notifier.watchdog();
        notifier.stopping();
        assert_eq!(
            received(&socket),
            ["READY=1\n", "WATCHDOG=1\n", "WATCHDOG=1\n", "STOPPING=1\n"]
        );
    }

    #[test]
    fn a_path_works_too() {
        // systemd's own socket is a path, /run/systemd/notify.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notify");
        let socket = UnixDatagram::bind(&path).unwrap();
        socket.set_nonblocking(true).unwrap();
        let notifier = Notifier::to(path.as_os_str()).unwrap();
        notifier.ready();
        assert_eq!(received(&socket), ["READY=1\n"]);
    }

    #[test]
    fn without_a_manager_every_message_is_a_no_op() {
        let notifier = Notifier::silent();
        assert!(!notifier.is_connected());
        notifier.ready();
        notifier.watchdog();
        notifier.stopping();
        // `from_env` does the same when the variable is absent, which it is
        // under `cargo test` - unless the tests run as a notify service.
        if env::var_os(NOTIFY_SOCKET).is_none() {
            assert!(!Notifier::from_env().is_connected());
        }
    }

    #[test]
    fn a_socket_that_cannot_be_reached_is_an_error_here_not_later() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nobody-listens");
        assert!(Notifier::to(missing.as_os_str()).is_err());
        // Far too long for an abstract name.
        let long = format!("@{}", "x".repeat(200));
        assert!(Notifier::to(OsStr::new(&long)).is_err());
    }

    #[test]
    fn a_manager_that_went_away_is_complained_about_once() {
        let (socket, address) = manager("gone");
        let notifier = Notifier::to(&address).unwrap();
        drop(socket);
        // Nothing to assert on the log; what matters is that neither send
        // panics, and that the second one is marked as already complained.
        notifier.watchdog();
        assert!(notifier.complained.load(Ordering::Relaxed));
        notifier.watchdog();
    }
}
