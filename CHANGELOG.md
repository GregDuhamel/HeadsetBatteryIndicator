# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.5.0] - 2026-10-09

The two large modules are split into directories, and the release binary is
static. Nothing changes at runtime: same readings, same log lines, same
command line, same 74 tests under the same names.

### Changed

- `src/maxwell.rs` is `src/maxwell/`: `mod.rs` keeps the public face
  (`Reader`, `supports`, the IDs, `discover`), `frame.rs` the dongle's report
  (the request, `fresh`, `messages`, `fetch`, and the tests on captured
  frames), `session.rs` one open dongle (`Session`, `Link`, `should_ask`, the
  `ASK_*` request budget, `AccessWatch` and the access-error lines), and
  `discover.rs` the dongle in sysfs (`Dongle`, `discover_in`,
  `headset_is_wired`). Every comment and every test moved with its code; the
  session tests borrow the captured frames from `frame.rs`.
- `src/bridge.rs` is `src/bridge/`: `mod.rs` keeps `Bridge`, `Config`, the
  `Uhid` seam and `DevicePool`; `batteries.rs` the virtual batteries and
  their bookkeeping (`Batteries`, `VirtualBattery`); `policy.rs` the pure
  decisions (`vet`, `second_opinion`, `withdrawal`, `next_delay`, `note`, the
  plausibility and retry constants) with their tests; and `tests.rs` the
  bridge driven over the scripted source and the fake kernel. The public API
  of both modules is unchanged, item for item (checked against rustdoc).
- The release workflow builds `x86_64-unknown-linux-musl`, statically linked,
  and refuses to attach a binary `file` does not report as static. Nothing in
  the dependency tree links C code, so the one file runs on any x86_64 Linux
  whatever its glibc. A `SHA256SUMS` is attached next to it; the README's
  *Releasing* section says how to check it, and *Install* how to feed the
  downloaded binary to `install.sh`.
- README: a *Source layout* section under *Development*.

## [0.4.0] - 2026-10-09

The daemon's loop is now tested end to end without hardware or `/dev/uhid`,
and the unit has a watchdog.

### Added

- A systemd watchdog: the unit is `Type=notify` with `WatchdogSec=60`, and
  the daemon speaks `sd_notify(3)` itself (`src/notify.rs`: `READY=1`,
  `WATCHDOG=1` on every turn of the loop, `STOPPING=1`; `std` only, no
  `unsafe`, abstract socket names handled). Without `NOTIFY_SOCKET` it is
  all a no-op. The README's new *The unit* section sizes the 60 s.
- Unit tests of the bridge itself, over a scripted source and uhid-battery's
  fake kernel (its new `fake` feature, from the dev-dependencies): attach on
  the first reading and republish every minute, the disconnect settle, the
  attach retry after a minute, charging without a level, the deferred
  reading and its second opinion, the offline and missing graces, the
  three-strikes destruction and the attach path after it, an identity the
  kernel can never accept, and the withdrawal of every battery at shutdown.
  The clock is passed in, so none of them sleeps. The README's *How the
  core is tested* section says what is and is not covered.
- Tests of the native reader's access-error reporting (said once, after
  5 s, and the recovery once too), of the headset going away as the dongle
  reports it, and of a re-enumerating dongle handing its link belief to the
  new session.

### Changed

- `Bridge` is generic over a `BatterySource` (implemented by `Source`) and
  a `Uhid` (implemented by `DevicePool`): the two seams the tests go
  through. `Bridge::run` takes the `Notifier`, and `tick` the instant of the
  poll.
- The native reader's access-error bookkeeping is an `AccessWatch` apart
  from the logging, so that it can be tested with instants of the test's
  choosing. Same lines in the journal.
- uhid-battery is pinned to the release that carries the `fake` feature.

### Fixed

- A headset that was no longer reported at all - the dongle unplugged, or a
  reader failing - was withdrawn after the *offline* grace (10 s), not the
  *missing* grace (30 s) the README promised: a missing headset is a silent
  one too, and the shorter grace was judged first. The missing grace is now
  judged on its own, and the offline grace only applies to a headset that
  is still listed.

## [0.3.0] - 2026-10-08

The native reader's transport moved to the shared
[hidraw](https://github.com/GregDuhamel/hidraw) crate. Nothing changes for the
dongle: same discovery, same request, same ioctl, same cadence.

### Changed

- The Audeze Maxwell dongle is found and driven through `hidraw` v0.1.0:
  `discover` with a USB and vendor filter (the virtual battery this daemon
  publishes carries the dongle's IDs on `BUS_VIRTUAL`, and is told apart by
  the bus as before), `Device::write` for the battery request, and
  `Device::get_input` for the answer the dongle never pushes.
- A session on the dongle is closed only when the dongle is gone
  (`hidraw::is_gone`: `ENODEV`, the node vanishing as the dongle
  re-enumerates). A transfer that merely failed - `EIO`, `EPIPE`, `ETIMEDOUT`,
  or a kernel without `HIDIOCGINPUT` - keeps the session, so the link belief
  and the questions already asked survive the hiccup; the level is withheld
  while it lasts, and the bridge's graces decide what becomes of the entry.
  Any error used to close the session, and its replacement listened, then
  asked again.
- README: the two shared crates in the architecture diagram and the text
  (`hidraw` reads the dongle, `uhid-battery` publishes the level); the native
  reader's Linux 5.11 requirement (`HIDIOCGINPUT`); the one `unsafe` spot is
  adopting systemd's descriptors in `main.rs`, not an ioctl; the journal line
  quoted under Troubleshooting is the one the daemon prints.

### Removed

- The crate's own sysfs walk (`/sys/class/hidraw`, `uevent` parsing) and its
  `HIDIOCGINPUT` ioctl, which was the native reader's `unsafe` block. `rustix`
  is no longer a direct dependency.

### Added

- This changelog.
- A test that a failed transfer keeps the session, and that a node sysfs no
  longer lists hands what it knew of the link to the next one.

## [0.2.2] - 2026-10-08

### Changed

- uhid-battery v0.4.0: `Bridge::wait` is `serve_all`, whose error names the
  battery that failed (`ServeError::index`) and feeds the existing
  three-strikes policy; the devices are kept contiguous next to their
  bookkeeping for it. Readings are published as `Reading`, identities are
  built with the 0.4 builders.
- An identity the kernel can never accept (`InvalidIdentity` from `create`)
  abandons that headset once, with one error line, instead of retrying every
  minute.

### Removed

- `poll.rs`: the daemon's own `poll(2)` wrapper, replaced by `serve_all`.

## [0.2.1] - 2026-10-08

### Changed

- uhid-battery v0.3.0: `Handle::inherited` is `unsafe` there now (it unsets
  `LISTEN_*` in the environment); the call lives in its own helper in `main.rs`
  with the safety argument - single-threaded at that point, the pipe-draining
  threads come later.

## [0.2.0] - 2026-10-08

Review fixes of the first phase.

### Fixed

- headsetcontrol: stdout and stderr are drained while waiting for the exit,
  so a talkative binary no longer deadlocks on a full pipe and reads as "did
  not answer"; what it said on stderr is quoted in the error; the stop flag
  is honoured during the wait.
- maxwell: a session that has heard nothing is asked again every ten minutes
  instead of never, so a headset that came back unannounced is still found;
  readings are numbered across sessions, so a second opinion is not rejected
  as a repeat after the dongle re-enumerates.
- bridge: after three consecutive uhid failures the virtual battery is
  destroyed and created again through the attach path and its retry, instead
  of logging the same error on every tick.

### Changed

- `run` is the default subcommand and the only home of its options:
  `--interval 30 status` is refused rather than silently ignored.
- `install.sh` keeps installing the unit when no headset is plugged in.
- The unit and the README document that `OpenFile=` needs systemd 253, and
  the fallback for an older one.
- `Source::probe` takes `&mut self`; no more `Cell`/`RefCell`.
- Tests use `tempfile` for the fake sysfs trees.

## Older releases

0.1.0 to 0.1.3 (2026-09-20 and 2026-09-21) predate this changelog; their
notes are on the
[GitHub releases page](https://github.com/GregDuhamel/HeadsetBatteryIndicator/releases).

[0.5.0]: https://github.com/GregDuhamel/HeadsetBatteryIndicator/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/GregDuhamel/HeadsetBatteryIndicator/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/GregDuhamel/HeadsetBatteryIndicator/compare/v0.2.2...v0.3.0
[0.2.2]: https://github.com/GregDuhamel/HeadsetBatteryIndicator/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/GregDuhamel/HeadsetBatteryIndicator/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/GregDuhamel/HeadsetBatteryIndicator/compare/v0.1.3...v0.2.0
