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
    /// The reader's own hardware has failed and needs a hand - a wedged
    /// dongle, which only replugging revives. No level is known. The reader
    /// reports it under a name that says what to do, and the bridge publishes
    /// it as an empty battery: on a desktop that only shows batteries, that
    /// is the one place where it gets seen.
    Fault,
}

/// One headset, as reported by whichever reader found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Headset {
    /// Model name, for example `Audeze Maxwell`.
    pub name: String,
    /// Product string of the endpoint the reader talks to, for example
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
    /// Which reading `battery` comes from, for a reader that reports the same
    /// reading on several polls in a row; `None` for one that reads afresh each
    /// time. It lets a consumer tell a second opinion from the first one
    /// repeated.
    pub sample: Option<u64>,
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

    /// The name to give the virtual device, which is the label desktops show.
    ///
    /// `name` comes from whatever the reader was told - HeadsetControl's JSON,
    /// in the general case - and the kernel's name field is 128 bytes, NUL
    /// included. uhid-battery refuses a name that would not fit rather than
    /// let the kernel truncate it, so make it fit here: control characters go,
    /// and the rest is cut on a `char` boundary.
    #[must_use]
    pub fn display_name(&self) -> String {
        const MAX_BYTES: usize = 127;

        let mut name: String = self.name.chars().filter(|c| !c.is_control()).collect();
        let mut end = name.len().min(MAX_BYTES);
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        name.truncate(end);
        if name.trim().is_empty() {
            "Headset".to_owned()
        } else {
            name
        }
    }

    /// A short, filesystem-safe identifier for the virtual device.
    ///
    /// The kernel builds the sysfs power supply name out of it
    /// (`hid-<uniq>-battery`), so it must not contain anything exotic.
    ///
    /// A [`BatteryState::Fault`] gets one of its own: UPower keeps a charge
    /// history per identifier, and the empty battery that stands for a fault
    /// has no business in the headset's.
    #[must_use]
    pub fn uniq(&self) -> String {
        let fault = if self.battery == BatteryState::Fault {
            "-fault"
        } else {
            ""
        };
        format!(
            "headset-{:04x}-{:04x}{fault}",
            self.vendor_id, self.product_id
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: &str) -> Headset {
        Headset {
            name: name.to_owned(),
            product: String::new(),
            vendor_id: 0x3329,
            product_id: 0x4b18,
            supports_battery: true,
            battery: BatteryState::Unavailable,
            sample: None,
        }
    }

    #[test]
    fn an_ordinary_name_is_left_alone() {
        assert_eq!(named("Audeze Maxwell").display_name(), "Audeze Maxwell");
    }

    #[test]
    fn a_name_the_kernel_would_truncate_is_made_to_fit() {
        // 100 two-byte characters: 200 bytes, cut on a boundary under 128.
        let name = named(&"é".repeat(100)).display_name();
        assert_eq!(name.len(), 126);
        assert!(name.chars().all(|c| c == 'é'));
    }

    #[test]
    fn a_fault_is_published_apart_from_the_headset() {
        let mut headset = named("Audeze Maxwell");
        assert_eq!(headset.uniq(), "headset-3329-4b18");
        headset.battery = BatteryState::Fault;
        assert_eq!(headset.uniq(), "headset-3329-4b18-fault");
    }

    #[test]
    fn control_characters_never_reach_the_kernel() {
        assert_eq!(named("Bad\0Name\n").display_name(), "BadName");
        assert_eq!(named("\0\n").display_name(), "Headset");
        assert_eq!(named("").display_name(), "Headset");
    }
}
