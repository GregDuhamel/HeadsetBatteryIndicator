//! A one-function wrapper over `poll(2)`.
//!
//! It exists to keep the rest of the crate free of the differences between
//! `rustix` releases, and to speak `Duration` instead of a raw `timespec`.

use std::io;
use std::time::Duration;

use rustix::event::{PollFd, Timespec};

/// Waits until one of `fds` is ready, or `timeout` elapses.
///
/// Returns the number of ready descriptors, which is `0` on timeout.
///
/// # Errors
///
/// Propagates `poll(2)` failures, including `EINTR` when a signal arrives.
pub fn poll(fds: &mut [PollFd<'_>], timeout: Duration) -> io::Result<usize> {
    let timeout = Timespec {
        tv_sec: timeout.as_secs().try_into().unwrap_or(i64::MAX),
        tv_nsec: timeout.subsec_nanos().into(),
    };
    rustix::event::poll(fds, Some(&timeout)).map_err(Into::into)
}
