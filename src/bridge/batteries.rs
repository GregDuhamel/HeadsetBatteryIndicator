//! The virtual batteries: the devices the kernel knows, and what the daemon
//! knows about each.

use std::ops::{Index, IndexMut};
use std::time::Instant;

use uhid_battery::Battery;

/// What the daemon knows about one virtual battery, besides the device itself
/// - which lives in [`Batteries`], at the same index.
#[derive(Debug)]
pub(super) struct VirtualBattery {
    pub(super) key: String,
    pub(super) name: String,
    /// When the headset last gave us a usable level.
    pub(super) last_reading: Instant,
    /// When the headset was last reported at all, answering or not.
    pub(super) last_seen: Instant,
    /// Since when the headset has been silent, if it is: the poll on which it
    /// first failed to answer. The grace runs from there and not from the last
    /// answer - which is a whole polling interval older, so that with a grace
    /// no longer than the interval the first lost reading would already have
    /// outlived it, and the retries would never get their chance.
    pub(super) silent_since: Option<Instant>,
    /// Since when the headset has been missing from the polls, likewise.
    pub(super) missing_since: Option<Instant>,
    /// Since when the reader has been saying the headset is gone.
    pub(super) gone_since: Option<Instant>,
    /// A reading that was too far off to publish, waiting for confirmation,
    /// and which reading that was.
    pub(super) deferred: Option<u8>,
    pub(super) deferred_sample: Option<u64>,
    /// The level the last INFO line mentioned.
    pub(super) logged_percent: u8,
    /// When a reading was last pushed to the kernel.
    pub(super) published_at: Instant,
    /// How many times in a row the device could not be talked to.
    pub(super) failures: u32,
}

/// The virtual batteries: the devices, and what the daemon knows about each.
///
/// Two vectors rather than one of pairs, because [`serve_all`] wants the
/// [`Battery`] values contiguous (`&mut [Battery]`) and nothing else about
/// them. An index means the same in both; every change goes through here so
/// that they cannot drift apart. Indexing yields the bookkeeping, which is
/// what most of the daemon reads; the device is asked for by name.
///
/// [`serve_all`]: uhid_battery::serve_all
#[derive(Debug, Default)]
pub(super) struct Batteries {
    devices: Vec<Battery>,
    states: Vec<VirtualBattery>,
}

impl Batteries {
    pub(super) fn len(&self) -> usize {
        self.states.len()
    }

    /// The index of the battery mirroring the headset with this key.
    pub(super) fn position(&self, key: &str) -> Option<usize> {
        self.states.iter().position(|state| state.key == key)
    }

    pub(super) fn push(&mut self, device: Battery, state: VirtualBattery) {
        self.devices.push(device);
        self.states.push(state);
    }

    /// Takes a battery out, closing the gap: every index past it moves down
    /// by one, as with `Vec::remove`.
    pub(super) fn remove(&mut self, index: usize) -> (Battery, VirtualBattery) {
        (self.devices.remove(index), self.states.remove(index))
    }

    pub(super) fn drain(&mut self) -> impl Iterator<Item = (Battery, VirtualBattery)> + '_ {
        self.devices.drain(..).zip(self.states.drain(..))
    }

    pub(super) fn device(&self, index: usize) -> &Battery {
        &self.devices[index]
    }

    /// The device and its bookkeeping, borrowed together: they are separate
    /// fields, so both can be held mutably at once.
    pub(super) fn pair_mut(&mut self, index: usize) -> (&mut Battery, &mut VirtualBattery) {
        (&mut self.devices[index], &mut self.states[index])
    }

    /// Every device, the way [`serve_all`] wants them.
    pub(super) fn devices_mut(&mut self) -> &mut [Battery] {
        &mut self.devices
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = &VirtualBattery> {
        self.states.iter()
    }

    pub(super) fn iter_mut(&mut self) -> impl Iterator<Item = &mut VirtualBattery> {
        self.states.iter_mut()
    }
}

impl Index<usize> for Batteries {
    type Output = VirtualBattery;

    fn index(&self, index: usize) -> &Self::Output {
        &self.states[index]
    }
}

impl IndexMut<usize> for Batteries {
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        &mut self.states[index]
    }
}
