use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{Local, Utc};
use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;
use zsnap::config::{Config, ScheduleTimezone};
use zsnap::executor::{ExecutionReport, execute};
use zsnap::lock::RunLock;
use zsnap::migration::convert_sanoid;
use zsnap::notification::{self, DeliveryEvent, Notice, NoticeField, NotificationReport};
use zsnap::planner::{Plan, build_plan, resolve_datasets};
use zsnap::status::StatusCache;
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
    /// Show cached pool, dataset, and snapshot inventory statistics.
    #[command(alias = "info")]
    Status {
        /// Query ZFS now and atomically replace the reporting cache.
        #[arg(long)]
        refresh: bool,
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
    snapshots_protected: usize,
    pools: usize,
}

/// Execution finished but some ZFS commands or hooks failed.
///
/// Carries the individual errors so failure notifications can show them, not just a count.
#[derive(Debug)]
struct OperationsFailed {
    errors: Vec<String>,
    snapshots_created: usize,
    snapshots_planned: usize,
    snapshots_pruned: usize,
    prunes_planned: usize,
    prunes_skipped: usize,
    pools: usize,
}

impl std::fmt::Display for OperationsFailed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} operation(s) failed after creating {} and pruning {} snapshot(s) across {} pool(s)",
            self.errors.len(),
            self.snapshots_created,
            self.snapshots_pruned,
            self.pools
        )
    }
}

impl std::error::Error for OperationsFailed {}

#[derive(Serialize)]
struct StatusOutput<'a> {
    source: &'a str,
    cache_file: String,
    status: &'a StatusCache,
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
    let webhook_environment_file = notification::environment_file_for_config(&cli.config);
    if let Commands::Check { probe } = &cli.command {
        return check(&config, *probe, cli.json);
    }
    if let Commands::NotifyTest { message } = &cli.command {
        return notify_test(&config, message, cli.json, &webhook_environment_file);
    }
    if let Commands::Status { refresh } = &cli.command {
        return show_status(&config, *refresh, cli.json, cli.verbose);
    }

    let notification_command = mutating_command(&cli.command);
    let started_at = Local::now();
    let started = Instant::now();
    let result = run_zfs_command(&cli, &config);
    let mut notification_error = None;
    if let Some(command) = notification_command {
        let (event, notice) = run_notice(
            command,
            &config,
            &cli.config,
            started_at,
            started.elapsed().as_secs_f64(),
            &result,
        );
        match notification::deliver_with_environment_file(
            &config.notifications,
            event,
            &notice,
            &webhook_environment_file,
        ) {
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
        Commands::Check { .. }
        | Commands::Status { .. }
        | Commands::MigrateSanoid { .. }
        | Commands::NotifyTest { .. } => unreachable!(),
    };

    // Hold the lock across discovery, planning, and mutation so two invocations cannot race.
    let _lock = if execute_actions && !dry_run {
        Some(RunLock::acquire(&config.settings.lock_file)?)
    } else {
        None
    };
    let inventory_started = Instant::now();
    let inventory =
        Inventory::discover(&config.settings, config.datasets.keys().map(String::as_str))?;
    let inventory_elapsed = inventory_started.elapsed();
    let resolved = resolve_datasets(config, &inventory)?;
    let mut plan = build_plan(config, &inventory, &resolved, Utc::now())?;
    filter_plan(&mut plan, scope);

    if !execute_actions {
        print_plan(&plan, cli.json)?;
        return Ok(RunSummary {
            snapshots_created: 0,
            snapshots_pruned: 0,
            snapshots_protected: plan.protected_snapshot_count(),
            pools: plan.pools().len(),
        });
    }

    let mut report = execute(&plan, &config.settings, dry_run, cli.verbose)?;
    let core_elapsed = overall_started.elapsed();
    if !dry_run && report.succeeded() {
        let cache = StatusCache::from_inventory(
            &inventory,
            config.datasets.keys().cloned(),
            inventory_elapsed,
            Some(&plan),
        );
        match cache.write_atomic(&config.settings.cache_file) {
            Ok(()) if cli.verbose => report.logs.push(format!(
                "[status] updated reporting cache {}",
                config.settings.cache_file.display()
            )),
            Ok(()) => {}
            Err(error) => eprintln!(
                "CACHE WARNING: failed to update {}: {error:#}",
                config.settings.cache_file.display()
            ),
        }
    }
    if cli.verbose {
        report.logs.push(format!(
            "[overall] timing: core run {:.3} ms (discovery, planning, and pool execution)",
            core_elapsed.as_secs_f64() * 1_000.0
        ));
    }
    print_report(&plan, &report, cli.json, cli.verbose)?;
    if !report.succeeded() {
        return Err(OperationsFailed {
            errors: report.errors,
            snapshots_created: report.snapshots_created,
            snapshots_planned: plan.snapshot_count(),
            snapshots_pruned: report.snapshots_pruned,
            prunes_planned: plan.prune_count(),
            prunes_skipped: report.prunes_skipped_after_snapshot_failure,
            pools: plan.pools().len(),
        }
        .into());
    }
    Ok(RunSummary {
        snapshots_created: report.snapshots_created,
        snapshots_pruned: report.snapshots_pruned,
        snapshots_protected: plan.protected_snapshot_count(),
        pools: plan.pools().len(),
    })
}

/// Build the success or failure notification for a mutating command.
fn run_notice(
    command: &str,
    config: &Config,
    config_path: &Path,
    started_at: chrono::DateTime<Local>,
    elapsed_seconds: f64,
    result: &Result<RunSummary>,
) -> (DeliveryEvent, Notice) {
    let mut notice = Notice {
        host: notification::hostname(&config.notifications),
        ..Notice::default()
    };
    let timing = format!(
        "{} (took {elapsed_seconds:.2}s)",
        started_at.format("%Y-%m-%d %H:%M:%S %:z")
    );
    let event = match result {
        Ok(summary) => {
            notice.headline = format!("zsnap {command} succeeded");
            notice.fields = vec![
                NoticeField::text("Started", timing),
                NoticeField::text(
                    "Snapshots",
                    format!(
                        "{} created, {} pruned, {} protected across {} pool(s)",
                        summary.snapshots_created,
                        summary.snapshots_pruned,
                        summary.snapshots_protected,
                        summary.pools
                    ),
                ),
            ];
            DeliveryEvent::Success
        }
        Err(error) => {
            notice.headline = format!("zsnap {command} failed");
            notice.fields.push(NoticeField::text("Started", timing));
            if let Some(failed) = error.downcast_ref::<OperationsFailed>() {
                let mut snapshots = format!(
                    "created {} of {}, pruned {} of {} across {} pool(s)",
                    failed.snapshots_created,
                    failed.snapshots_planned,
                    failed.snapshots_pruned,
                    failed.prunes_planned,
                    failed.pools
                );
                if failed.prunes_skipped > 0 {
                    snapshots.push_str(&format!(
                        "; {} prune(s) skipped after a snapshot failure",
                        failed.prunes_skipped
                    ));
                }
                notice
                    .fields
                    .push(NoticeField::text("Snapshots", snapshots));
                notice.details_heading = format!("Errors ({})", failed.errors.len());
                notice.details = failed.errors.clone();
            } else {
                // The run stopped before execution (lock, discovery, planning); show the
                // whole cause chain, one link per line.
                notice.details_heading = "Error".to_owned();
                notice.details = error
                    .chain()
                    .enumerate()
                    .map(|(index, cause)| {
                        if index == 0 {
                            cause.to_string()
                        } else {
                            format!("caused by: {cause}")
                        }
                    })
                    .collect();
            }
            DeliveryEvent::Failure
        }
    };
    notice.fields.push(NoticeField::code(
        "Config",
        config_path.display().to_string(),
    ));
    notice.fields.push(NoticeField::text(
        "Version",
        format!("zsnap {}", env!("CARGO_PKG_VERSION")),
    ));
    if let Some(logs) = journal_command() {
        notice.fields.push(NoticeField::code("Logs", logs));
    }
    (event, notice)
}

/// Return a journalctl command for this exact run when started by systemd.
fn journal_command() -> Option<String> {
    let invocation = std::env::var("INVOCATION_ID").ok()?;
    let valid = !invocation.is_empty() && invocation.chars().all(|c| c.is_ascii_hexdigit());
    valid.then(|| format!("journalctl _SYSTEMD_INVOCATION_ID={invocation}"))
}

fn mutating_command(command: &Commands) -> Option<&'static str> {
    match command {
        Commands::Run { dry_run: false } => Some("run"),
        Commands::Snapshot { dry_run: false } => Some("snapshot"),
        Commands::Prune { dry_run: false } => Some("prune"),
        _ => None,
    }
}

fn show_status(config: &Config, refresh: bool, json: bool, verbose: bool) -> Result<()> {
    let cache_path = &config.settings.cache_file;
    let (cache, source) = if refresh || !cache_path.exists() {
        let _lock = RunLock::acquire(&config.settings.lock_file)?;
        let inventory_started = Instant::now();
        let inventory =
            Inventory::discover(&config.settings, config.datasets.keys().map(String::as_str))?;
        let cache = StatusCache::from_inventory(
            &inventory,
            config.datasets.keys().cloned(),
            inventory_started.elapsed(),
            None,
        );
        cache.write_atomic(cache_path)?;
        (cache, if refresh { "refreshed" } else { "created" })
    } else {
        (
            StatusCache::load(cache_path).with_context(|| {
                format!(
                    "rerun `zsnap status --refresh` to rebuild {}",
                    cache_path.display()
                )
            })?,
            "cache",
        )
    };

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&StatusOutput {
                source,
                cache_file: cache_path.display().to_string(),
                status: &cache,
            })?
        );
        return Ok(());
    }

    let generated_at = match config.settings.timezone {
        ScheduleTimezone::Local => cache.generated_at.with_timezone(&Local).to_rfc3339(),
        ScheduleTimezone::Utc => cache.generated_at.to_rfc3339(),
    };
    let other_snapshots = cache
        .totals
        .snapshots
        .saturating_sub(cache.totals.managed_snapshots);
    let pool_names = cache
        .pools
        .iter()
        .map(|pool| pool.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    println!("status source: {source} ({})", cache_path.display());
    println!(
        "updated: {generated_at} ({})",
        format_cache_age(cache.generated_at)
    );
    println!(
        "scope: {} configured root(s), inventory scan {} ms",
        cache.scope_roots.len(),
        cache.inventory_scan_milliseconds
    );
    if pool_names.is_empty() {
        println!("zpools: 0");
    } else {
        println!("zpools: {} ({pool_names})", cache.totals.pools);
    }
    println!("datasets: {}", cache.totals.datasets);
    println!(
        "snapshots: {} ({} zsnap-managed, {other_snapshots} other)",
        cache.totals.snapshots, cache.totals.managed_snapshots
    );

    if verbose {
        println!("pool details:");
        for pool in &cache.pools {
            let capacity = pool
                .capacity_percent
                .map(|capacity| format!("{capacity}% capacity"))
                .unwrap_or_else(|| "capacity unavailable".to_owned());
            println!(
                "  {}: {capacity}, {} dataset(s), {} snapshot(s) ({} managed)",
                pool.name, pool.datasets, pool.snapshots, pool.managed_snapshots
            );
        }
        println!("dataset details:");
        for dataset in &cache.datasets {
            println!(
                "  {}: {} snapshot(s) ({} managed)",
                dataset.name, dataset.snapshots, dataset.managed_snapshots
            );
        }
    }
    Ok(())
}

fn format_cache_age(generated_at: chrono::DateTime<Utc>) -> String {
    let seconds = Utc::now().signed_duration_since(generated_at).num_seconds();
    if seconds < 0 {
        return format!("{} s in the future", seconds.unsigned_abs());
    }
    match seconds {
        0..=59 => format!("{seconds} s ago"),
        60..=3_599 => format!("{} min ago", seconds / 60),
        3_600..=86_399 => format!("{} h ago", seconds / 3_600),
        _ => format!("{} d ago", seconds / 86_400),
    }
}

fn notify_test(config: &Config, text: &str, json: bool, environment_file: &Path) -> Result<()> {
    let notice = Notice {
        headline: "zsnap webhook test".to_owned(),
        host: notification::hostname(&config.notifications),
        fields: vec![
            NoticeField::text("Message", text),
            NoticeField::text("Version", format!("zsnap {}", env!("CARGO_PKG_VERSION"))),
        ],
        ..Notice::default()
    };
    let report = notification::deliver_with_environment_file(
        &config.notifications,
        DeliveryEvent::Test,
        &notice,
        environment_file,
    )?;
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
        "plan: create {} snapshot(s), prune {} snapshot(s), protect {} snapshot(s), {} pool(s)",
        plan.snapshot_count(),
        plan.prune_count(),
        plan.protected_snapshot_count(),
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
    for snapshot in &plan.protected_snapshots {
        let reason = match (snapshot.user_holds > 0, snapshot.has_clones) {
            (true, true) => format!(
                "{} user hold(s) and one or more dependent clones",
                snapshot.user_holds
            ),
            (true, false) => format!("{} user hold(s)", snapshot.user_holds),
            (false, true) => "one or more dependent clones".to_owned(),
            (false, false) => unreachable!(),
        };
        println!(
            "  PROTECT [{}] {}@{}: {reason}",
            snapshot.pool, snapshot.dataset, snapshot.name
        );
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
    if !plan.protected_snapshots.is_empty() {
        println!(
            "protected {} managed snapshot(s) from pruning because of user holds or dependent clones",
            plan.protected_snapshot_count()
        );
        if verbose || report.dry_run {
            for snapshot in &plan.protected_snapshots {
                println!(
                    "  PROTECT [{}] {}@{}",
                    snapshot.pool, snapshot.dataset, snapshot.name
                );
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn notice_for(result: &Result<RunSummary>) -> (DeliveryEvent, Notice) {
        let config = Config::parse("version = 1").unwrap();
        run_notice(
            "run",
            &config,
            Path::new("/etc/zsnap/zsnap.toml"),
            Local::now(),
            1.5,
            result,
        )
    }

    fn field<'a>(notice: &'a Notice, label: &str) -> &'a str {
        &notice
            .fields
            .iter()
            .find(|field| field.label == label)
            .unwrap_or_else(|| panic!("missing {label} field"))
            .value
    }

    #[test]
    fn operation_failures_carry_every_error_and_progress() {
        let result = Err(OperationsFailed {
            errors: vec!["[tank] first".to_owned(), "[backup] second".to_owned()],
            snapshots_created: 1,
            snapshots_planned: 2,
            snapshots_pruned: 0,
            prunes_planned: 3,
            prunes_skipped: 3,
            pools: 2,
        }
        .into());
        let (event, notice) = notice_for(&result);
        assert_eq!(event, DeliveryEvent::Failure);
        assert_eq!(notice.headline, "zsnap run failed");
        assert_eq!(notice.details_heading, "Errors (2)");
        assert_eq!(notice.details, ["[tank] first", "[backup] second"]);
        assert_eq!(
            field(&notice, "Snapshots"),
            "created 1 of 2, pruned 0 of 3 across 2 pool(s); 3 prune(s) skipped after a snapshot failure"
        );
        assert_eq!(field(&notice, "Config"), "/etc/zsnap/zsnap.toml");
    }

    #[test]
    fn early_failures_show_the_whole_cause_chain() {
        let result = Err(anyhow!("Resource temporarily unavailable")
            .context("failed to lock /run/zsnap/zsnap.lock"));
        let (_, notice) = notice_for(&result);
        assert_eq!(notice.details_heading, "Error");
        assert_eq!(
            notice.details,
            [
                "failed to lock /run/zsnap/zsnap.lock",
                "caused by: Resource temporarily unavailable"
            ]
        );
        assert!(notice.fields.iter().all(|field| field.label != "Snapshots"));
    }
}
