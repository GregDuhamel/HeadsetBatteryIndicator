//! Where battery readings come from.
//!
//! HeadsetControl knows a few hundred headsets and is the general answer. For
//! the hardware this crate can talk to itself, the native reader is both far
//! more reliable and about forty times faster, so it goes first.

use std::cell::Cell;

use anyhow::Result;

use crate::headset::Headset;
use crate::headsetcontrol::HeadsetControl;
use crate::maxwell;

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

/// A configured source of battery readings.
#[derive(Debug, Clone)]
pub struct Source {
    backend: Backend,
    control: HeadsetControl,
    /// Whether the last probe was answered by the native reader.
    native: Cell<bool>,
}

impl Source {
    /// Builds a source.
    #[must_use]
    pub fn new(backend: Backend, control: HeadsetControl) -> Self {
        Self {
            backend,
            control,
            native: Cell::new(false),
        }
    }

    /// A short description, for the startup log line.
    #[must_use]
    pub fn describe(&self) -> String {
        let binary = self.control.binary().display();
        match self.backend {
            Backend::Auto => format!("the native reader, falling back to {binary}"),
            Backend::Native => "the native reader".to_owned(),
            Backend::HeadsetControl => binary.to_string(),
        }
    }

    /// Whether the last [`Source::probe`] was answered by the native reader.
    ///
    /// It matters for pacing: a native read is one HID request and takes about
    /// 70 ms, where HeadsetControl replays twenty packets over 2.7 s, so only
    /// the former can be afforded every few seconds.
    #[must_use]
    pub fn last_probe_was_native(&self) -> bool {
        self.native.get()
    }

    /// Reads the battery state of every headset this source can see.
    ///
    /// # Errors
    ///
    /// Only HeadsetControl can fail; the native reader reports a dongle it
    /// cannot query as an unavailable battery instead.
    pub fn probe(&self) -> Result<Vec<Headset>> {
        self.native.set(self.backend == Backend::Native);
        match self.backend {
            Backend::Native => Ok(maxwell::probe()),
            Backend::HeadsetControl => self.control.probe(),
            Backend::Auto => {
                let native = maxwell::probe();
                if native.is_empty() {
                    self.control.probe()
                } else {
                    self.native.set(true);
                    // Deliberately not merged with HeadsetControl's view:
                    // running its twenty-packet sequence against the same
                    // dongle every poll is what we are trying to get away from.
                    Ok(native)
                }
            }
        }
    }
}
