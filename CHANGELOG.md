# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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

[0.3.0]: https://github.com/GregDuhamel/HeadsetBatteryIndicator/compare/v0.2.2...v0.3.0
[0.2.2]: https://github.com/GregDuhamel/HeadsetBatteryIndicator/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/GregDuhamel/HeadsetBatteryIndicator/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/GregDuhamel/HeadsetBatteryIndicator/compare/v0.1.3...v0.2.0
