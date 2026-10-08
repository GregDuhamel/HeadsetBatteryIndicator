//! Where battery readings come from.
//!
//! HeadsetControl knows a few hundred headsets and is the general answer. For
//! the hardware this crate can talk to itself the native reader goes first: it
//! is far more reliable, and it listens to the dongle instead of polling the
//! headset, which is both quicker to notice a change and kinder to the dongle.

use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::headset::Headset;
use crate::headsetcontrol::HeadsetControl;
use crate::maxwell;

/// Default for how often the native reader asks a linked headset for its level,
/// in seconds.
pub const DEFAULT_NATIVE_INTERVAL_SECS: u64 = 60;

/// How long a vanished dongle is given to re-enumerate before HeadsetControl is
/// tried instead. It takes two or three seconds.
const REENUMERATION_GRACE: Duration = Duration::from_secs(30);

/// Which reader to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backend {
    /// The native reader when it recognises the hardware, HeadsetControl
    /// otherwise.
    #[default]
    Auto,
    /// Only the native reader; HeadsetControl does not need to be installed.
    Native,
    /// Only HeadsetControl.
    HeadsetControl,
}

/// Where the bridge gets its readings from.
///
/// [`Source`] in the daemon; the bridge's unit tests script one, so that the
/// bridge is tested on readings nobody has to plug in. Only what the bridge
/// asks of a source is here - the one-shot reading of the `status` command
/// stays on [`Source`].
pub trait BatterySource {
    /// A short description, for the startup log line.
    fn describe(&self) -> String;

    /// Whether the last [`BatterySource::probe`] was answered by the native
    /// reader.
    ///
    /// It matters for pacing: the native reader only listens, so it can - and
    /// has to - be run every second, where HeadsetControl replays twenty
    /// packets over 2.7 s.
    fn last_probe_was_native(&self) -> bool;

    /// Reads the battery state of every headset this source can see.
    ///
    /// `stop` cuts a slow reading short; a source that never waits for
    /// anything may ignore it.
    ///
    /// # Errors
    ///
    /// The reading failed outright, and no headset is known. A headset that
    /// is there but cannot be queried is better reported with an unavailable
    /// battery, so that it keeps its entry through the bridge's graces.
    fn probe(&mut self, stop: &AtomicBool) -> Result<Vec<Headset>>;
}

/// A configured source of battery readings.
#[derive(Debug)]
pub struct Source {
    backend: Backend,
    control: HeadsetControl,
    /// Whether the last probe was answered by the native reader.
    native: bool,
    /// The native reader keeps its dongles open between polls.
    reader: maxwell::Reader,
    /// How often the native reader asks a linked headset for its level.
    native_interval: Duration,
    /// When the native reader last had a dongle to listen to.
    native_seen_at: Option<Instant>,
}

impl Source {
    /// Builds a source.
    #[must_use]
    pub fn new(backend: Backend, control: HeadsetControl, native_interval: Duration) -> Self {
        Self {
            backend,
            control,
            native: false,
            reader: maxwell::Reader::new(),
            native_interval,
            native_seen_at: None,
        }
    }

    /// Whether a dongle the native reader was listening to vanished only
    /// moments ago.
    ///
    /// The Maxwell's dongle re-enumerates on USB whenever the headset comes or
    /// goes, and is gone for a couple of seconds each time. Falling back to
    /// HeadsetControl for that gap would be wrong twice over: it fires its
    /// twenty packets at a headset that has typically just been switched off,
    /// and it drops the daemon to HeadsetControl's slow cadence, so the headset
    /// coming back goes unnoticed for a minute.
    fn dongle_is_coming_back(&self) -> bool {
        self.native_seen_at
            .is_some_and(|at| at.elapsed() < REENUMERATION_GRACE)
    }

    /// One pass of the native reader: listen to the dongles, and ask a linked
    /// headset for its level if that is due.
    fn listen(&mut self) -> Vec<Headset> {
        self.reader.poll(self.native_interval)
    }

    /// A one-shot reading for the `status` command, which cannot wait for the
    /// next poll to collect the answer to the request it just sent.
    ///
    /// # Errors
    ///
    /// Same as [`BatterySource::probe`].
    pub fn probe_once(&mut self) -> Result<Vec<Headset>> {
        // Nothing to interrupt a one-shot command with.
        let stop = AtomicBool::new(false);
        if self.backend == Backend::HeadsetControl {
            return self.control.probe(&stop);
        }
        let native = self.reader.read_once();
        if native.is_empty() && self.backend == Backend::Auto {
            self.control.probe(&stop)
        } else {
            Ok(native)
        }
    }
}

impl BatterySource for Source {
    fn describe(&self) -> String {
        let binary = self.control.binary().display();
        match self.backend {
            Backend::Auto => format!(
                "the native reader (asking a linked headset every {:?}), falling back to {binary}",
                self.native_interval
            ),
            Backend::Native => format!(
                "the native reader (asking a linked headset every {:?})",
                self.native_interval
            ),
            Backend::HeadsetControl => binary.to_string(),
        }
    }

    fn last_probe_was_native(&self) -> bool {
        self.native
    }

    /// `stop` cuts a running `headsetcontrol` short; the native reader never
    /// waits for anything. Only HeadsetControl can fail; the native reader
    /// reports a dongle it cannot query as an unavailable battery instead.
    fn probe(&mut self, stop: &AtomicBool) -> Result<Vec<Headset>> {
        self.native = self.backend == Backend::Native;
        match self.backend {
            Backend::Native => Ok(self.listen()),
            Backend::HeadsetControl => self.control.probe(stop),
            Backend::Auto => {
                let native = self.listen();
                if native.is_empty() && self.dongle_is_coming_back() {
                    // Still the native reader's turn: see below.
                    self.native = true;
                    Ok(native)
                } else if native.is_empty() {
                    self.control.probe(stop)
                } else {
                    self.native_seen_at = Some(Instant::now());
                    self.native = true;
                    // Deliberately not merged with HeadsetControl's view:
                    // running its twenty-packet sequence against the same
                    // dongle every poll is what we are trying to get away from.
                    Ok(native)
                }
            }
        }
    }
}
