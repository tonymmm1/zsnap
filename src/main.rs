use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;
use zsnap::config::Config;
use zsnap::executor::{ExecutionReport, execute};
use zsnap::lock::RunLock;
use zsnap::migration::convert_sanoid;
use zsnap::notification::{self, DeliveryEvent, NotificationReport};
use zsnap::planner::{Plan, build_plan, resolve_datasets};
use zsnap::zfs::Inventory;

#[derive(Debug, Parser)]
#[command(name = "zsnap", version, about = "Fast, policy-driven ZFS snapshots")]
struct Cli {
    /// Path to the TOML configuration file.
    #[arg(short, long, default_value = "/etc/zsnap/zsnap.toml", global = true)]
    config: PathBuf,

    /// Emit machine-readable JSON.
    #[arg(long, global = true)]
    json: bool,

    /// Include command-level execution details.
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Take due snapshots, then prune expired snapshots.
    Run {
        /// Show commands without changing ZFS state or running hooks.
        #[arg(long)]
        dry_run: bool,
    },
    /// Take due snapshots without pruning.
    Snapshot {
        /// Show commands without changing ZFS state or running hooks.
        #[arg(long)]
        dry_run: bool,
    },
    /// Prune expired snapshots without taking new snapshots.
    Prune {
        /// Show commands without changing ZFS state or running hooks.
        #[arg(long)]
        dry_run: bool,
    },
    /// Inspect current state and print the actions that would be performed.
    Plan {
        /// Restrict the plan to one action type.
        #[arg(long, value_enum, default_value_t = PlanScope::All)]
        scope: PlanScope,
    },
    /// Validate the configuration, optionally probing ZFS as well.
    Check {
        /// Also discover ZFS state and resolve recursive datasets.
        #[arg(long)]
        probe: bool,
    },
    /// Convert a Sanoid sanoid.conf into validated zsnap TOML without touching ZFS.
    MigrateSanoid {
        /// Sanoid configuration to read; it is never modified.
        #[arg(short, long, default_value = "/etc/sanoid/sanoid.conf")]
        input: PathBuf,

        /// Optional sanoid.defaults.conf; the sibling file is detected automatically.
        #[arg(long)]
        defaults: Option<PathBuf>,

        /// Create this file with mode 0600; existing paths are never overwritten.
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Send a test message to every enabled webhook.
    NotifyTest {
        /// Text appended to the test notification.
        #[arg(long, default_value = "webhook delivery is working")]
        message: String,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum PlanScope {
    All,
    Snapshot,
    Prune,
}

#[derive(Serialize)]
struct CheckOutput {
    status: &'static str,
    configured_datasets: usize,
    resolved_datasets: Option<usize>,
    pools: Option<usize>,
}

#[derive(Debug)]
struct RunSummary {
    snapshots_created: usize,
    snapshots_pruned: usize,
    pools: usize,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Commands::MigrateSanoid {
        input,
        defaults,
        output,
    } = &cli.command
    {
        return migrate_sanoid(input, defaults.as_deref(), output.as_deref(), cli.json);
    }

    let config = Config::load(&cli.config)?;
    if let Commands::Check { probe } = &cli.command {
        return check(&config, *probe, cli.json);
    }
    if let Commands::NotifyTest { message } = &cli.command {
        return notify_test(&config, message, cli.json);
    }

    let notification_command = mutating_command(&cli.command);
    let started = Instant::now();
    let result = run_zfs_command(&cli, &config);
    let mut notification_error = None;
    if let Some(command) = notification_command {
        let (event, detail) = match &result {
            Ok(summary) => (
                DeliveryEvent::Success,
                format!(
                    "Created {} snapshot(s), pruned {} snapshot(s), {} pool(s).",
                    summary.snapshots_created, summary.snapshots_pruned, summary.pools
                ),
            ),
            Err(error) => (DeliveryEvent::Failure, format!("Error: {error:#}")),
        };
        let status = if event == DeliveryEvent::Success {
            "SUCCESS"
        } else {
            "FAILURE"
        };
        let message = format!(
            "zsnap {command} {status} on {}\nDuration: {:.3}s\n{detail}",
            notification::hostname(&config.notifications),
            started.elapsed().as_secs_f64(),
        );
        match notification::deliver(&config.notifications, event, &message) {
            Ok(report) => {
                print_notification_errors(&report);
                if !report.succeeded() {
                    notification_error = Some(format!(
                        "{} of {} notification delivery attempt(s) failed",
                        report.failed(),
                        report.attempted
                    ));
                }
            }
            Err(error) => {
                eprintln!("NOTIFICATION ERROR: {error:#}");
                notification_error = Some(error.to_string());
            }
        }
    }

    match result {
        Err(error) => Err(error),
        Ok(_) if config.notifications.fail_on_error && notification_error.is_some() => {
            Err(anyhow!(notification_error.unwrap()))
        }
        Ok(_) => Ok(()),
    }
}

fn run_zfs_command(cli: &Cli, config: &Config) -> Result<RunSummary> {
    let overall_started = Instant::now();
    let (scope, dry_run, execute_actions) = match &cli.command {
        Commands::Run { dry_run } => (PlanScope::All, *dry_run, true),
        Commands::Snapshot { dry_run } => (PlanScope::Snapshot, *dry_run, true),
        Commands::Prune { dry_run } => (PlanScope::Prune, *dry_run, true),
        Commands::Plan { scope } => (*scope, true, false),
        Commands::Check { .. } | Commands::MigrateSanoid { .. } | Commands::NotifyTest { .. } => {
            unreachable!()
        }
    };

    // Hold the lock across discovery, planning, and mutation so two invocations cannot race.
    let _lock = if execute_actions && !dry_run {
        Some(RunLock::acquire(&config.settings.lock_file)?)
    } else {
        None
    };
    let inventory =
        Inventory::discover(&config.settings, config.datasets.keys().map(String::as_str))?;
    let resolved = resolve_datasets(config, &inventory)?;
    let mut plan = build_plan(config, &inventory, &resolved, Utc::now())?;
    filter_plan(&mut plan, scope);

    if !execute_actions {
        print_plan(&plan, cli.json)?;
        return Ok(RunSummary {
            snapshots_created: 0,
            snapshots_pruned: 0,
            pools: plan.pools().len(),
        });
    }

    let mut report = execute(&plan, &config.settings, dry_run, cli.verbose)?;
    if cli.verbose {
        report.logs.push(format!(
            "[overall] timing: core run {:.3} ms (discovery, planning, and pool execution)",
            overall_started.elapsed().as_secs_f64() * 1_000.0
        ));
    }
    print_report(&plan, &report, cli.json, cli.verbose)?;
    if !report.succeeded() {
        bail!(
            "{} operation(s) failed after creating {} and pruning {} snapshot(s) across {} pool(s)",
            report.errors.len(),
            report.snapshots_created,
            report.snapshots_pruned,
            plan.pools().len()
        );
    }
    Ok(RunSummary {
        snapshots_created: report.snapshots_created,
        snapshots_pruned: report.snapshots_pruned,
        pools: plan.pools().len(),
    })
}

fn mutating_command(command: &Commands) -> Option<&'static str> {
    match command {
        Commands::Run { dry_run: false } => Some("run"),
        Commands::Snapshot { dry_run: false } => Some("snapshot"),
        Commands::Prune { dry_run: false } => Some("prune"),
        _ => None,
    }
}

fn notify_test(config: &Config, text: &str, json: bool) -> Result<()> {
    let message = format!(
        "zsnap TEST on {}\n{}",
        notification::hostname(&config.notifications),
        text
    );
    let report = notification::deliver(&config.notifications, DeliveryEvent::Test, &message)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "webhook test: delivered {} of {} target(s); {} skipped",
            report.delivered, report.attempted, report.skipped
        );
    }
    print_notification_errors(&report);
    if report.attempted == 0 {
        bail!("no enabled notification webhooks are configured");
    }
    if !report.succeeded() {
        bail!(
            "{} webhook test delivery attempt(s) failed",
            report.failed()
        );
    }
    Ok(())
}

fn print_notification_errors(report: &NotificationReport) {
    for delivery in &report.deliveries {
        if let Some(error) = &delivery.error {
            eprintln!(
                "NOTIFICATION ERROR [{}:{} after {} attempt(s)]: {error}",
                delivery.kind, delivery.target, delivery.attempts
            );
        }
    }
}

fn migrate_sanoid(
    input: &Path,
    requested_defaults: Option<&Path>,
    output: Option<&Path>,
    json: bool,
) -> Result<()> {
    let source = fs::read_to_string(input)
        .with_context(|| format!("failed to read Sanoid configuration {}", input.display()))?;
    let sibling_defaults = input.with_file_name("sanoid.defaults.conf");
    let defaults_path = requested_defaults
        .map(Path::to_path_buf)
        .or_else(|| sibling_defaults.is_file().then_some(sibling_defaults));
    let defaults = defaults_path
        .as_ref()
        .map(|path| {
            fs::read_to_string(path)
                .with_context(|| format!("failed to read Sanoid defaults {}", path.display()))
        })
        .transpose()?;
    let mut migration = convert_sanoid(&source, defaults.as_deref())?;
    if defaults_path.is_none() {
        migration.warnings.insert(
            0,
            "sanoid.defaults.conf was not supplied or found beside the input; used embedded Sanoid 2.x defaults"
                .to_owned(),
        );
    }

    if let Some(path) = output {
        write_new_config(path, &migration.config_toml)?;
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&migration)?);
        return Ok(());
    }
    for warning in &migration.warnings {
        eprintln!("MIGRATION WARNING: {warning}");
    }
    if let Some(path) = output {
        println!(
            "wrote validated zsnap configuration to {} ({} dataset(s), {} template(s), {} warning(s)); source unchanged; no ZFS commands run",
            path.display(),
            migration.datasets,
            migration.templates,
            migration.warnings.len()
        );
    } else {
        print!("{}", migration.config_toml);
    }
    Ok(())
}

fn write_new_config(path: &Path, contents: &str) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            bail!(
                "refusing to overwrite existing migration output {}; choose a new path",
                path.display()
            )
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to create migration output {}", path.display()));
        }
    };
    if let Err(error) = file
        .write_all(contents.as_bytes())
        .and_then(|()| file.sync_all())
    {
        return Err(error).with_context(|| {
            format!(
                "failed to write migration output {}; the partial create-new file was left in place for inspection",
                path.display()
            )
        });
    }
    Ok(())
}

fn check(config: &Config, probe: bool, json: bool) -> Result<()> {
    let (resolved_datasets, pools) = if probe {
        let inventory =
            Inventory::discover(&config.settings, config.datasets.keys().map(String::as_str))?;
        let resolved = resolve_datasets(config, &inventory)?;
        (Some(resolved.len()), Some(inventory.pools.len()))
    } else {
        (None, None)
    };
    let output = CheckOutput {
        status: "ok",
        configured_datasets: config.datasets.len(),
        resolved_datasets,
        pools,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else if probe {
        println!(
            "configuration is valid; {} configured dataset(s) resolve to {} dataset(s) across {} pool(s)",
            config.datasets.len(),
            resolved_datasets.unwrap_or_default(),
            pools.unwrap_or_default()
        );
    } else {
        println!(
            "configuration is valid ({} configured dataset(s))",
            config.datasets.len()
        );
    }
    Ok(())
}

fn filter_plan(plan: &mut Plan, scope: PlanScope) {
    match scope {
        PlanScope::All => {}
        PlanScope::Snapshot => plan.retain_snapshots_only(),
        PlanScope::Prune => plan.retain_prunes_only(),
    }
}

fn print_plan(plan: &Plan, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(plan)?);
        return Ok(());
    }
    println!(
        "plan: create {} snapshot(s), prune {} snapshot(s), {} pool(s)",
        plan.snapshot_count(),
        plan.prune_count(),
        plan.pools().len()
    );
    for action in &plan.snapshots {
        let recursion = if action.recursive { " recursively" } else { "" };
        println!(
            "  CREATE [{}] {}{} @ {}",
            action.pool,
            action.dataset,
            recursion,
            action.names.join(", ")
        );
    }
    for action in &plan.prunes {
        println!(
            "  PRUNE  [{}] {} @ {}",
            action.pool,
            action.dataset,
            action.names.join(", ")
        );
    }
    for dataset in &plan.deferred_prune_datasets {
        println!("  DEFER  pruning {dataset}: pool capacity is below prune_defer");
    }
    Ok(())
}

fn print_report(plan: &Plan, report: &ExecutionReport, json: bool, verbose: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(report)?);
        return Ok(());
    }
    if verbose || report.dry_run {
        for line in &report.logs {
            println!("{line}");
        }
    }
    for error in &report.errors {
        eprintln!("ERROR: {error}");
    }
    if report.dry_run {
        println!(
            "dry run: would create {} snapshot(s) and prune {} snapshot(s) across {} pool(s)",
            plan.snapshot_count(),
            plan.prune_count(),
            plan.pools().len()
        );
    } else {
        println!(
            "created {} snapshot(s), pruned {} snapshot(s) across {} pool(s)",
            report.snapshots_created,
            report.snapshots_pruned,
            plan.pools().len()
        );
    }
    if report.prunes_skipped_after_snapshot_failure > 0 {
        eprintln!(
            "safety stop: skipped {} prune(s) after a snapshot failure",
            report.prunes_skipped_after_snapshot_failure
        );
    }
    Ok(())
}
