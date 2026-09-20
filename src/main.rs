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
};
use headset_battery_indicator::headset::BatteryState;
use headset_battery_indicator::headsetcontrol::HeadsetControl;
use headset_battery_indicator::source::{Backend, Source};
use headset_battery_indicator::systemd::{self, UHID_FD_NAME};
use headset_battery_indicator::uhid::{self, Uhid};
use log::{LevelFilter, error, warn};

/// Default group granted access to the headset's hidraw nodes.
const DEFAULT_GROUP: &str = "headset-battery";

#[derive(Debug, Parser)]
#[command(
    name = "headset-battery-indicator",
    version,
    about = "Publishes HeadsetControl battery levels to UPower",
    long_about = "Reads the battery level of a wireless headset with HeadsetControl and \
                  publishes it as a virtual HID battery, so UPower - and therefore KDE's \
                  Power & Battery applet - lists the headset like any other peripheral."
)]
struct Cli {
    #[command(flatten)]
    common: CommonArgs,

    #[command(flatten)]
    run: RunArgs,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the daemon (default).
    Run(RunArgs),
    /// Print what HeadsetControl currently reports, then exit.
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

#[derive(Debug, Clone, Args)]
struct RunArgs {
    /// Delay between two battery readings, in seconds.
    #[arg(short, long, value_name = "SECONDS", default_value_t = DEFAULT_INTERVAL_SECS)]
    interval: u64,

    /// How long a headset that is detected but no longer answering keeps its
    /// entry, in seconds. Headsets park their radio when idle; the last known
    /// level stays true meanwhile.
    #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_OFFLINE_GRACE_SECS)]
    offline_grace: u64,

    /// How long a headset that is no longer detected at all (dongle unplugged)
    /// keeps its entry, in seconds.
    #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_MISSING_GRACE_SECS)]
    missing_grace: u64,

    /// Path of the uhid character device.
    #[arg(long, value_name = "PATH", default_value = uhid::DEV_UHID)]
    uhid: PathBuf,
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
    let source = Source::new(cli.common.backend.into(), control);

    let result = match cli.command.unwrap_or(Command::Run(cli.run)) {
        Command::Run(args) => run(&args, source),
        Command::Status => status(&source),
        Command::UdevRules(args) => udev_rules(&args, &source),
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

fn run(args: &RunArgs, source: Source) -> Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
        signal_hook::flag::register(signal, Arc::clone(&stop))
            .with_context(|| format!("installing the handler for signal {signal}"))?;
    }

    let inherited = systemd::take_fds(UHID_FD_NAME)
        .into_iter()
        .filter_map(|fd| match Uhid::from_fd(fd) {
            Ok(handle) => Some(handle),
            Err(err) => {
                warn!("ignoring an inherited descriptor: {err}");
                None
            }
        })
        .collect::<Vec<_>>();

    let config = Config {
        interval: Duration::from_secs(args.interval.max(1)),
        offline_grace: Duration::from_secs(args.offline_grace),
        missing_grace: Duration::from_secs(args.missing_grace),
        uhid_path: args.uhid.clone(),
    };

    Bridge::new(config, source, inherited).run(&stop)
}

fn status(source: &Source) -> Result<()> {
    let headsets = source.probe()?;
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
                "unavailable (radio parked, or headset off)".to_owned()
            }
            BatteryState::Unavailable => "not supported by this headset".to_owned(),
        };
        let published = uhid::find_power_supply(&headset.uniq()).map_or_else(
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

fn udev_rules(args: &UdevRulesArgs, source: &Source) -> Result<()> {
    let headsets = source.probe()?;
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
    writeln!(
        out,
        "\n# Makes UPower (and the desktop) show a headset rather than a generic battery.\n\
         SUBSYSTEM==\"input\", KERNEL==\"input*\", ATTR{{phys}}==\"headset-battery-indicator/*\", \
         ENV{{SOUND_INITIALIZED}}=\"1\", ENV{{SOUND_FORM_FACTOR}}=\"headset\""
    )?;

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
