//! What every battery reader reports, whichever way it got it.

/// What a headset says about its battery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatteryState {
    /// Running on battery, at the given percentage.
    Discharging(u8),
    /// Plugged in. Some headsets stop reporting a level while charging.
    Charging(Option<u8>),
    /// No level could be read: the query failed, or nothing is known yet.
    Unavailable,
    /// Positively known to be switched off or out of range, as opposed to merely
    /// not answering: the reader was told so. There is nothing to wait for.
    Disconnected,
}

/// One headset, as reported by HeadsetControl.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Headset {
    /// Model name, for example `Audeze Maxwell`.
    pub name: String,
    /// Product string of the endpoint HeadsetControl talks to, for example
    /// `Audeze Maxwell XBOX Dongle`.
    pub product: String,
    /// USB vendor ID.
    pub vendor_id: u16,
    /// USB product ID.
    pub product_id: u16,
    /// Whether the headset advertises `CAP_BATTERY_STATUS`.
    pub supports_battery: bool,
    /// Last known battery state.
    pub battery: BatteryState,
}

impl Headset {
    /// A stable identifier, used to match a headset across polls.
    #[must_use]
    pub fn key(&self) -> String {
        format!(
            "{:04x}:{:04x}/{}",
            self.vendor_id, self.product_id, self.name
        )
    }

    /// A short, filesystem-safe identifier for the virtual device.
    ///
    /// The kernel builds the sysfs power supply name out of it
    /// (`hid-<uniq>-battery`), so it must not contain anything exotic.
    #[must_use]
    pub fn uniq(&self) -> String {
        format!("headset-{:04x}-{:04x}", self.vendor_id, self.product_id)
    }
}
