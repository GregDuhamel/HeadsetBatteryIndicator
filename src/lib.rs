//! Publishes the battery level of a wireless headset to UPower.
//!
//! Desktop environments read peripheral battery levels from UPower, and UPower
//! only knows about devices the kernel exposes under
//! `/sys/class/power_supply`. There is no way for a third-party daemon to add
//! a device to UPower over D-Bus, so this crate takes the other route: it
//! creates a virtual HID device through `/dev/uhid` whose report descriptor
//! declares a battery. The kernel's `hid-input` driver then registers the
//! `power_supply` object itself, UPower picks it up like any other peripheral
//! battery, and KDE's Power & Battery applet lists the headset.
//!
//! The battery values come from the [HeadsetControl] binary, which knows the
//! vendor protocol of a few hundred headsets - or, for the Audeze Maxwell, from
//! a native reader that is markedly more reliable (see [`maxwell`]).
//!
//! [HeadsetControl]: https://github.com/Sapd/HeadsetControl

pub mod bridge;
pub mod headsetcontrol;
pub mod maxwell;
mod poll;
pub mod source;
pub mod systemd;
pub mod uhid;
