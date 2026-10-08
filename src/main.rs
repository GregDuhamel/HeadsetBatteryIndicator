//! Command line entry point.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use headset_battery_indicator::bridge::{
    Bridge, Config, DEFAULT_INTERVAL_SECS, DEFAULT_MISSING_GRACE_SECS, DEFAULT_OFFLINE_GRACE_SECS,
    PHYS_PREFIX,
};
use headset_battery_indicator::headset::BatteryState;
use headset_battery_indicator::headsetcontrol::HeadsetControl;
use headset_battery_indicator::source::{Backend, DEFAULT_NATIVE_INTERVAL_SECS, Source};
use log::{LevelFilter, error};
use uhid_battery::{DEV_UHID, Handle, Kind, find_power_supply};

/// Name prefix of the `/dev/uhid` descriptors the unit file passes down.
const UHID_FD_PREFIX: &str = "uhid";

/// Default group granted access to the headset's hidraw nodes.
const DEFAULT_GROUP: &str = "headset-battery";

#[derive(Debug, Parser)]
#[command(
    name = "headset-battery-indicator",
    version,
    about = "Publishes a wireless headset's battery level to UPower",
    long_about = "Reads the battery level of a wireless headset - natively for the Audeze \
                  Maxwell, through HeadsetControl for everything else - and publishes it as \
                  a virtual HID battery, so UPower - and therefore KDE's Power & Battery \
                  applet - lists the headset like any other peripheral.",
    after_help = "Without a command, the daemon runs with the defaults of `run`."
)]
struct Cli {
    #[command(flatten)]
    common: CommonArgs,

    /// The daemon's own options live under `run` only, rather than also being
    /// flattened here: accepted at the root, `--interval 30 status` parsed and
    /// was silently ignored.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the daemon (the default, with its default options).
    Run(RunArgs),
    /// Print what the readers currently report, then exit.
    Status,
    /// Print a udev rule granting a group access to the detected headsets.
    UdevRules(UdevRulesArgs),
}

/// Command line spelling of [`Backend`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum BackendArg {
    /// Native reader when the hardware is recognised, headsetcontrol otherwise.
    Auto,
    /// Native reader only (Audeze Maxwell); headsetcontrol is not needed.
    Native,
    /// headsetcontrol only.
    Headsetcontrol,
}

impl From<BackendArg> for Backend {
    fn from(arg: BackendArg) -> Self {
        match arg {
            BackendArg::Auto => Self::Auto,
            BackendArg::Native => Self::Native,
            BackendArg::Headsetcontrol => Self::HeadsetControl,
        }
    }
}

#[derive(Debug, Clone, Args)]
struct CommonArgs {
    /// Where battery readings come from.
    #[arg(
        long,
        value_enum,
        value_name = "BACKEND",
        default_value = "auto",
        global = true
    )]
    backend: BackendArg,

    /// Path to the headsetcontrol binary.
    #[arg(
        long,
        value_name = "PATH",
        default_value = "headsetcontrol",
        global = true
    )]
    headsetcontrol: PathBuf,

    /// How long to wait for headsetcontrol before giving up, in seconds.
    #[arg(long, value_name = "SECONDS", default_value_t = 10, global = true)]
    timeout: u64,

    /// Increase logging: -v for debug, -vv for trace.
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
struct RunArgs {
    /// Delay between two readings through headsetcontrol, in seconds.
    #[arg(short, long, value_name = "SECONDS", default_value_t = DEFAULT_INTERVAL_SECS)]
    interval: u64,

    /// How often the native reader asks a linked headset for its level, in
    /// seconds. It never asks a headset that is switched off.
    #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_NATIVE_INTERVAL_SECS)]
    native_interval: u64,

    /// How long a headset that is detected but no longer answering (switched
    /// off, typically) keeps its entry, in seconds.
    #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_OFFLINE_GRACE_SECS)]
    offline_grace: u64,

    /// How long a headset that is no longer detected at all (dongle unplugged)
    /// keeps its entry, in seconds.
    #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_MISSING_GRACE_SECS)]
    missing_grace: u64,

    /// Path of the uhid character device.
    #[arg(long, value_name = "PATH", default_value = DEV_UHID)]
    uhid: PathBuf,
}

/// What `run` gets when it is not spelled out: the same defaults as the
/// attributes above, which a test holds the two to.
impl Default for RunArgs {
    fn default() -> Self {
        Self {
            interval: DEFAULT_INTERVAL_SECS,
            native_interval: DEFAULT_NATIVE_INTERVAL_SECS,
            offline_grace: DEFAULT_OFFLINE_GRACE_SECS,
            missing_grace: DEFAULT_MISSING_GRACE_SECS,
            uhid: PathBuf::from(DEV_UHID),
        }
    }
}

#[derive(Debug, Clone, Args)]
struct UdevRulesArgs {
    /// Group the rule grants access to.
    #[arg(long, value_name = "GROUP", default_value = DEFAULT_GROUP)]
    group: String,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(cli.common.verbose);

    let control = HeadsetControl::new(
        cli.common.headsetcontrol.clone(),
        Duration::from_secs(cli.common.timeout.max(1)),
    );
    let command = cli
        .command
        .unwrap_or_else(|| Command::Run(RunArgs::default()));
    let native_interval = match &command {
        Command::Run(args) => args.native_interval,
        _ => DEFAULT_NATIVE_INTERVAL_SECS,
    };
    let mut source = Source::new(
        cli.common.backend.into(),
        control,
        Duration::from_secs(native_interval.max(1)),
    );

    let result = match command {
        Command::Run(args) => run(&args, source),
        Command::Status => status(&mut source),
        Command::UdevRules(args) => udev_rules(&args, &mut source),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            error!("{err:#}");
            ExitCode::FAILURE
        }
    }
}

fn init_logging(verbose: u8) {
    let level = match verbose {
        0 => LevelFilter::Info,
        1 => LevelFilter::Debug,
        _ => LevelFilter::Trace,
    };

    let mut builder = env_logger::Builder::new();
    builder.filter_level(level);
    // Under systemd the journal timestamps every line already.
    if std::env::var_os("JOURNAL_STREAM").is_some() {
        builder.format_timestamp(None);
    }
    builder.parse_default_env();
    builder.init();
}

/// The `/dev/uhid` descriptors systemd handed us, if any.
///
/// `OpenFile=/dev/uhid:uhid` in the unit: systemd opens the node and passes
/// it down, so the daemon never needs permission to open it itself.
// `unsafe_code = "deny"` crate-wide; this is the one place that needs it.
#[allow(unsafe_code)]
fn inherited_uhid_handles() -> Vec<Handle> {
    // SAFETY: `inherited` removes `LISTEN_PID`/`LISTEN_FDS`/`LISTEN_FDNAMES`
    // from the environment, which is only sound while no other thread can be
    // reading it. It is called from `run` before the bridge starts, and the
    // only threads this daemon ever spawns (draining headsetcontrol's pipes)
    // come later.
    unsafe { Handle::inherited(UHID_FD_PREFIX) }
}

fn run(args: &RunArgs, source: Source) -> Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
        signal_hook::flag::register(signal, Arc::clone(&stop))
            .with_context(|| format!("installing the handler for signal {signal}"))?;
    }

    let inherited = inherited_uhid_handles();

    let config = Config {
        interval: Duration::from_secs(args.interval.max(1)),
        offline_grace: Duration::from_secs(args.offline_grace),
        missing_grace: Duration::from_secs(args.missing_grace),
        uhid_path: args.uhid.clone(),
    };

    Bridge::new(config, source, inherited).run(&stop)
}

fn status(source: &mut Source) -> Result<()> {
    let headsets = source.probe_once()?;
    let mut out = std::io::stdout().lock();

    if headsets.is_empty() {
        writeln!(out, "No headset detected.")?;
        return Ok(());
    }

    for headset in &headsets {
        let battery = match headset.battery {
            BatteryState::Discharging(percent) => format!("{percent}%"),
            BatteryState::Charging(Some(percent)) => format!("{percent}% (charging)"),
            BatteryState::Charging(None) => "charging".to_owned(),
            BatteryState::Unavailable if headset.supports_battery => {
                "unavailable (headset off?)".to_owned()
            }
            BatteryState::Unavailable => "not supported by this headset".to_owned(),
            BatteryState::Disconnected => "headset switched off (the dongle says so)".to_owned(),
        };
        let published = find_power_supply(&headset.uniq()).map_or_else(
            || "not published (is the daemon running?)".to_owned(),
            |path| path.display().to_string(),
        );
        writeln!(
            out,
            "{} [{:04x}:{:04x}] via {}\n  battery: {battery}\n  sysfs:   {published}",
            headset.name, headset.vendor_id, headset.product_id, headset.product,
        )?;
    }
    Ok(())
}

fn udev_rules(args: &UdevRulesArgs, source: &mut Source) -> Result<()> {
    let headsets = source.probe_once()?;
    let mut out = std::io::stdout().lock();

    writeln!(
        out,
        "# Generated by headset-battery-indicator udev-rules.\n\
         # Lets the daemon's unprivileged service user talk to the headset.\n\
         # Install as /etc/udev/rules.d/70-headset-battery-indicator.rules. The name\n\
         # must sort before 73-seat-late.rules: once uaccess has put an ACL on the\n\
         # node, MODE= only moves the ACL mask and the group never gets access."
    )?;

    // UPower types a HID battery after its sibling nodes, and promotes it to
    // "headset" when one of them carries the properties systemd puts on a sound
    // card. It accepts an input node as that sibling, and the virtual device
    // has one, so tagging it is all it takes to get the headset icon instead of
    // a laptop battery.
    if let Some(rule) = Kind::Headset.udev_rule(&format!("{PHYS_PREFIX}/*")) {
        writeln!(
            out,
            "\n# Makes UPower (and the desktop) show a headset rather than a generic battery.\n{rule}"
        )?;
    }

    if headsets.is_empty() {
        writeln!(out, "# No headset detected; plug the dongle in and re-run.")?;
        return Ok(());
    }

    for headset in &headsets {
        writeln!(
            out,
            "\n# {} ({})\nKERNEL==\"hidraw*\", ATTRS{{idVendor}}==\"{:04x}\", \
             ATTRS{{idProduct}}==\"{:04x}\", GROUP=\"{}\", MODE=\"0660\"",
            headset.name, headset.product, headset.vendor_id, headset.product_id, args.group,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_line_is_well_formed() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn running_without_a_command_is_running_with_the_defaults() {
        // `RunArgs::default()` stands in for `run` when no command is given,
        // so it has to say what `run` says.
        let Some(Command::Run(parsed)) = Cli::parse_from(["x", "run"]).command else {
            panic!("`run` did not parse as itself");
        };
        assert_eq!(parsed, RunArgs::default());
        assert!(Cli::parse_from(["x"]).command.is_none());
    }

    #[test]
    fn run_options_belong_to_run() {
        // The bug this guards against: with the options also accepted at the
        // root, `--interval 30 status` was valid and `30` went nowhere.
        assert!(Cli::try_parse_from(["x", "--interval", "30", "status"]).is_err());
        assert!(Cli::try_parse_from(["x", "status", "--interval", "30"]).is_err());
        assert!(Cli::try_parse_from(["x", "--interval", "30"]).is_err());
        assert!(Cli::try_parse_from(["x", "run", "--interval", "30"]).is_ok());
        // The global ones go anywhere.
        assert!(Cli::try_parse_from(["x", "--timeout", "3", "status"]).is_ok());
        assert!(Cli::try_parse_from(["x", "status", "--timeout", "3"]).is_ok());
    }
}
