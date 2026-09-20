# Headset Battery Indicator

[![CI](https://github.com/GregDuhamel/HeadsetBatteryIndicator/actions/workflows/ci.yml/badge.svg)](https://github.com/GregDuhamel/HeadsetBatteryIndicator/actions/workflows/ci.yml)
[![Lint](https://github.com/GregDuhamel/HeadsetBatteryIndicator/actions/workflows/lint.yml/badge.svg)](https://github.com/GregDuhamel/HeadsetBatteryIndicator/actions/workflows/lint.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Makes a wireless gaming headset show up in **KDE's Power & Battery applet**, next
to the mouse and the keyboard. The battery level comes from
[HeadsetControl](https://github.com/Sapd/HeadsetControl), or — for the Audeze
Maxwell — from a native reader that is far more reliable.

No tray icon, no applet to install: the headset becomes a real UPower device, so
anything that reads UPower — Plasma, GNOME, `upower -d`, a status bar — sees it.

## How it works

UPower does not accept battery devices over D-Bus; it only reports what the
kernel publishes under `/sys/class/power_supply`. So instead of talking to
UPower, this daemon talks to the kernel:

```
 headsetcontrol -b -o json          /dev/uhid                    /sys/class/power_supply
 ┌───────────────────────┐   poll   ┌──────────────────────┐    ┌─────────────────────────┐
 │  Audeze Maxwell 73 %  │ ───────▶ │ headset-battery-     │──▶ │ hid-headset-3329-4b18-  │
 │  (vendor HID report)  │   60 s   │ indicator            │    │ battery  (capacity=73)  │
 └───────────────────────┘          └──────────────────────┘    └────────────┬────────────┘
                                     creates a virtual HID                   │ udev
                                     device whose descriptor                 ▼
                                     declares a battery              ┌───────────────┐
                                                                     │    UPower     │
                                                                     └───────┬───────┘
                                                                             ▼
                                                                  KDE Power & Battery
```

The virtual device's report descriptor declares *Battery Strength* (Generic
Device Controls page, usage `0x20`) and *Charging* (Battery System page, usage
`0x44`). That is all `drivers/hid/hid-input.c` needs to register a
`power_supply` object with `scope=Device`, which UPower exposes as a peripheral
battery.

Three details are worth knowing, all checked against the kernel source:

* The descriptor's top-level collection must be an **input application**
  (`IS_INPUT_APPLICATION`). With a vendor-defined collection,
  `hidinput_connect()` returns before looking at any field, and the device gets
  a hidraw node and nothing else — no battery. It is declared as *Consumer
  Control*, which is what the real dongle declares too.
* A HID device that declares **only** a battery is torn down again —
  `hidinput_connect()` bails out with *"No inputs registered, leaving"* and
  removes the battery with it. The descriptor therefore ends with a single
  vendor-defined input bit, which the kernel maps to `BTN_MISC`. That bit is
  never set, and it is outside every range systemd's `input_id` builtin looks
  at, so udev does not tag the node as a keyboard or a pointer and UPower keeps
  reporting a plain battery rather than mislabelling the headset.
* The device is created on the **virtual bus** (`BUS_VIRTUAL`) with the real
  vendor and product IDs. Device-specific kernel HID drivers all match on a
  physical bus, so only `hid-generic` binds to it.

## Why it shows up as a headset

UPower types a HID battery after its *sibling* nodes: a mouse if one of them is
tagged `ID_INPUT_MOUSE`, and so on. There is no input class for headsets; the
only way to the `headset` kind is a sibling carrying `SOUND_INITIALIZED=1` and
`SOUND_FORM_FACTOR=headset`, the properties systemd puts on sound cards. UPower
accepts an `input` node as that sibling, and the virtual device has one, so the
generated udev rule tags it. Without the rule everything still works, but the
desktop draws a generic battery - which reads as a laptop battery.

## Backends

`--backend auto` (the default) uses the native reader when it recognises the
hardware and falls back to HeadsetControl otherwise. `--backend native` and
`--backend headsetcontrol` force one or the other.

| Backend | Hardware | Notes |
| --- | --- | --- |
| `native` | Audeze Maxwell (`3329:4b18`, `3329:4b19`) | One request, about 70 ms per read. HeadsetControl does not need to be installed. |
| `headsetcontrol` | [everything HeadsetControl supports](https://github.com/Sapd/HeadsetControl#supported-headsets) | Shells out to `headsetcontrol --battery --output json`. |

### Why a native reader for the Maxwell

HeadsetControl reads the Maxwell's battery unreliably — it reports
`BATTERY_UNAVAILABLE` while music is playing on the headset, and the odd `0%`
or `44%` in between. Its driver replays a twenty-packet sequence and looks for
the battery answer in the buffer of a *different* request, one frame later, so
a dongle that is a few milliseconds late is read as "unavailable". It also
matches `d6 0c 00 00` anywhere in the frame, which hits the dongle's
acknowledgement as well as its answer.

The dongle's input report is really a stream of small messages,
`05 <type> <len> 00 <payload>`:

```
05 5b 03 00  d6 0c 00          acknowledgement of request d6 0c
05 5d 05 00  d6 0c 00 00 5b    answer: 0x5b = 91 %
```

The native reader sends that single request, polls the input report until the
*answer* message (type `5d`) shows up, and rejects any level above 100. Measured
side by side on a Maxwell Xbox dongle: 60 reads out of 60 in 0.06 s each, against
intermittent failures and 2.7 s per read for HeadsetControl.

Two more things the frames taught us. The report is a buffer the dongle fills
from the start, and its second byte counts the bytes written since it was last
fetched; everything past that is left over from earlier exchanges, old answers
included. Reading without that count returns a stale level for ever once the
headset is switched off.

And charging is not in there at all: every register the dongle answers a read
for was compared plugged and unplugged, and only the level moved. What changes
is the USB bus - on a cable to the computer the headset enumerates as a device
of its own (`3329:4b1e`, *Audeze Maxwell XBOX Headset*), and that presence is
what gets reported as charging. A headset charging from a wall adapter is
therefore invisible, and keeps reading as discharging.

The answer is only available through a `GET_REPORT` control transfer
(`HIDIOCGINPUT`); the dongle never pushes it on the interrupt endpoint, so a
plain `read()` on the hidraw node sees nothing.

## Requirements

* Linux with `CONFIG_UHID` and `CONFIG_HID_BATTERY_STRENGTH` (Fedora, Arch,
  Ubuntu and friends all ship both).
* An Audeze Maxwell, **or** [HeadsetControl](https://github.com/Sapd/HeadsetControl)
  4.x on `PATH` with a
  [supported headset](https://github.com/Sapd/HeadsetControl#supported-headsets)
  that has the `battery` capability.
* UPower (any desktop that shows peripheral batteries).
* Rust 1.85 or newer to build.

## Install

```sh
cargo build --release
sudo ./install.sh
```

`install.sh` installs the binary to `/usr/local/bin`, creates the
`headset-battery` system group, generates the udev rule for the headsets it
currently detects, installs the systemd unit and starts it.

Remove everything with `sudo ./install.sh --uninstall`.

Then check the result:

```sh
systemctl status headset-battery-indicator.service
upower -d | grep -B2 -A8 -i headset
```

## Usage

```
headset-battery-indicator [OPTIONS] [COMMAND]

Commands:
  run         Run the daemon (default)
  status      Print what HeadsetControl currently reports, then exit
  udev-rules  Print a udev rule granting a group access to the detected headsets

Options:
      --backend <BACKEND>      auto, native or headsetcontrol [default: auto]
      --headsetcontrol <PATH>  Path to the headsetcontrol binary [default: headsetcontrol]
      --timeout <SECONDS>      How long to wait for headsetcontrol [default: 10]
  -i, --interval <SECONDS>     Delay between two battery readings [default: 60]
      --offline-grace <SECONDS>
                               How long a detected but silent headset keeps its entry [default: 900]
      --missing-grace <SECONDS>
                               How long an undetected headset keeps its entry [default: 180]
      --uhid <PATH>            Path of the uhid character device [default: /dev/uhid]
  -v, --verbose...             -v for debug, -vv for trace
```

`RUST_LOG` is honoured too, if you want finer filtering than `-v`.

## When the headset is not there

Wireless headsets park their radio after a short idle period — an Audeze
Maxwell does it within a couple of minutes of silence — and the dongle then
answers `BATTERY_UNAVAILABLE` even though the headset is switched on.
Neither backend can tell that apart from a headset that is off (HeadsetControl's
`--connected` flag is derived from the very same battery query), so the daemon
runs two clocks instead:

| Situation | What happens |
| --- | --- |
| Headset detected but not answering (radio parked, or switched off) | The entry **stays**, showing the last known level — a parked headset is not draining, so that level is still true. It is withdrawn after `--offline-grace` (15 min). |
| Headset no longer reported at all (dongle unplugged, `headsetcontrol` failing) | The entry is withdrawn after `--missing-grace` (3 min): a level on screen would be fiction. A single warning per failure streak, so an unplugged dongle does not fill the journal. |
| Headset never answered since the daemon started | Nothing is published until a first level is read. |
| Service stopped or restarted | Every virtual battery is destroyed, so no stale entry is left behind. |

The journal says which state the daemon is in, once per change:
`… is detected but not answering battery queries`, or
`no supported headset found`.

## Noisy readings

Wireless dongles hand out the occasional bogus frame. An Audeze Maxwell will
answer `0%` or `44%` between two `92%` readings, and a spurious `0%` is enough
to make the desktop announce a critical battery.

So a reading more than 15 points away from the last published level is held
back, and only published if the next poll confirms it (within 5 points). No
headset moves that far in one interval, so such a jump is either noise — which
never reaches UPower — or a real change, such as a machine coming back from a
night of sleep, which costs one extra poll before it shows up.

Small moves are logged at debug level rather than info, so a headset hovering
between 91% and 92% does not fill the journal; `journalctl -u
headset-battery-indicator -f` with `-v` in `ExecStart=` shows everything.

## Security

`/dev/uhid` is what this daemon needs, and a process holding it can create
arbitrary input devices — including a keyboard. The unit is built so that
nobody is granted access to the node:

* the node keeps its `root:root 0600` permissions — no udev rule widens it;
* systemd opens it (`OpenFile=/dev/uhid:uhid`) and passes the descriptor to the
  service, which adopts it through the `sd_listen_fds()` protocol;
* the daemon runs as a `DynamicUser=`, with an empty capability bounding set,
  no network, a read-only view of the filesystem, and `DevicePolicy=closed`
  except for hidraw;
* the only other device it reaches is the headset's own `hidraw` node, through
  the `headset-battery` group set by the generated udev rule.

The crate itself denies `unsafe` code outside three documented spots: adopting
the descriptor systemd passes, clearing the `LISTEN_*` environment variables,
and the `HIDIOCGINPUT` ioctl of the native reader. Those descriptors also get `FD_CLOEXEC`, so the `headsetcontrol`
child never inherits `/dev/uhid`.

## Development

```sh
cargo test                 # unit + CLI tests, no hardware needed
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
```

The CLI tests drive the binary against a stub `headsetcontrol`, so they run
anywhere. One acceptance test does talk to the real `/dev/uhid` — it creates a
virtual battery and reads it back from sysfs — and is ignored unless you run it
as root:

```sh
cargo build --tests
sudo -E cargo test --test uhid_live -- --ignored --nocapture
```

CI runs the test suite on stable and on the MSRV, verifies the systemd unit with
`systemd-analyze`, and the lint workflow covers rustfmt, clippy, rustdoc,
shellcheck and `cargo audit`.

## Troubleshooting

**Nothing shows up in the applet.** Check that the kernel created the power
supply: `ls /sys/class/power_supply/` should contain `hid-headset-<vid>-<pid>-battery`
(recent kernels append the report ID: `…-battery-1`) while the headset is on. If it is there but UPower does not list it, restart
`upower.service`.

**`could not run headsetcontrol`.** The binary is not on the daemon's `PATH`;
point at it explicitly with `--headsetcontrol /usr/local/bin/headsetcontrol` in
the unit's `ExecStart=`.

**The journal says `no supported headset found`.** The daemon is
running but cannot see the dongle, which is a different problem from a headset
that is merely switched off — that one is reported as *"connected but not
answering battery queries"*. Either the dongle is unplugged, or the udev rule
does not cover it and the service's unprivileged user cannot open its hidraw
node. Regenerate the rule while the dongle is plugged in:

```sh
headset-battery-indicator udev-rules | sudo tee /etc/udev/rules.d/70-headset-battery-indicator.rules
sudo udevadm control --reload && sudo udevadm trigger --subsystem-match=hidraw
```

**`out of inherited /dev/uhid descriptors`.** You have more than one headset;
add another `OpenFile=/dev/uhid:uhid` line to the unit — the daemon takes every
descriptor named `uhid`.

## Credits

* [HeadsetControl](https://github.com/Sapd/HeadsetControl) does the hard part:
  speaking each vendor's protocol.
* [ruflas/headset-battery-indicator](https://github.com/ruflas/headset-battery-indicator)
  for the idea of a small dedicated indicator.

## License

MIT — see [LICENSE](LICENSE).
