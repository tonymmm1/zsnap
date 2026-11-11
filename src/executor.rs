use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::config::Settings;
use crate::parallel::map_bounded;
use crate::planner::{Plan, PruneAction, SnapshotAction};
use crate::process::{output_with_timeout, wait_with_timeout};
use crate::zfs::MANAGED_PROPERTY;

#[derive(Debug, Clone, Default, Serialize)]
pub struct ExecutionReport {
    pub dry_run: bool,
    pub snapshots_created: usize,
    pub snapshots_pruned: usize,
    pub errors: Vec<String>,
    pub logs: Vec<String>,
}

impl ExecutionReport {
    pub fn succeeded(&self) -> bool {
        self.errors.is_empty()
    }
}

#[derive(Debug, Clone)]
struct PoolWork {
    pool: String,
    snapshots: Vec<SnapshotAction>,
    prunes: Vec<PruneAction>,
}

#[derive(Debug, Default)]
struct PoolResult {
    pool: String,
    snapshots_created: usize,
    snapshots_pruned: usize,
    errors: Vec<String>,
    logs: Vec<String>,
}

pub fn execute(
    plan: &Plan,
    settings: &Settings,
    dry_run: bool,
    verbose: bool,
) -> Result<ExecutionReport> {
    let mut work: BTreeMap<String, PoolWork> = BTreeMap::new();
    for action in &plan.snapshots {
        work.entry(action.pool.clone())
            .or_insert_with(|| PoolWork {
                pool: action.pool.clone(),
                snapshots: Vec::new(),
                prunes: Vec::new(),
            })
            .snapshots
            .push(action.clone());
    }
    for action in &plan.prunes {
        work.entry(action.pool.clone())
            .or_insert_with(|| PoolWork {
                pool: action.pool.clone(),
                snapshots: Vec::new(),
                prunes: Vec::new(),
            })
            .prunes
            .push(action.clone());
    }

    if work.is_empty() {
        return Ok(ExecutionReport {
            dry_run,
            ..ExecutionReport::default()
        });
    }

    let work: Vec<_> = work.into_values().collect();
    let thread_count = if settings.max_parallel_pools == 0 {
        work.len()
    } else {
        settings.max_parallel_pools.min(work.len())
    };
    // Results come back in input order, which is already sorted by pool name.
    let results = map_bounded(work, thread_count, "zsnap-pool", |pool_work| {
        execute_pool(pool_work, settings, dry_run, verbose)
    })
    .context("failed to create the pool execution thread set")?;

    let mut report = ExecutionReport {
        dry_run,
        ..ExecutionReport::default()
    };
    for mut result in results {
        report.snapshots_created += result.snapshots_created;
        report.snapshots_pruned += result.snapshots_pruned;
        report.errors.append(&mut result.errors);
        report.logs.append(&mut result.logs);
    }
    Ok(report)
}

fn execute_pool(work: PoolWork, settings: &Settings, dry_run: bool, verbose: bool) -> PoolResult {
    let pool_started = Instant::now();
    let mut result = PoolResult {
        pool: work.pool.clone(),
        ..PoolResult::default()
    };
    if verbose {
        result
            .logs
            .push(format!("[{}] starting pool work", work.pool));
    }
    let snapshot_started = Instant::now();
    let mut batchable_regular = Vec::new();
    let mut batchable_recursive = Vec::new();
    let mut hooked = Vec::new();
    for action in work.snapshots {
        if action.policy.has_snapshot_hooks() {
            hooked.push(action);
        } else if action.recursive {
            batchable_recursive.push(action);
        } else {
            batchable_regular.push(action);
        }
    }

    execute_snapshot_batches(&batchable_regular, false, settings, dry_run, &mut result);
    execute_snapshot_batches(&batchable_recursive, true, settings, dry_run, &mut result);
    for action in hooked {
        execute_hooked_snapshot(&action, settings, dry_run, &mut result);
    }
    let snapshot_elapsed = snapshot_started.elapsed();

    // Pruning runs even when a snapshot failed. Retention never drops below its
    // configured count, and on a full pool pruning is what lets snapshots succeed again.
    let prune_started = Instant::now();
    for action in work.prunes {
        if action.policy.has_prune_hooks() {
            execute_hooked_prunes(&action, settings, dry_run, &mut result);
        } else {
            execute_prune_batches(&action, settings, dry_run, &mut result);
        }
    }
    let prune_elapsed = prune_started.elapsed();
    if verbose {
        result.logs.push(format!(
            "[{}] timing: snapshots {:.3} ms, pruning {:.3} ms, pool total {:.3} ms",
            work.pool,
            snapshot_elapsed.as_secs_f64() * 1_000.0,
            prune_elapsed.as_secs_f64() * 1_000.0,
            pool_started.elapsed().as_secs_f64() * 1_000.0
        ));
    }
    result
}

fn snapshot_args(recursive: bool, targets: &[String]) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![OsString::from("snapshot")];
    if recursive {
        args.push(OsString::from("-r"));
    }
    args.push(OsString::from("-o"));
    args.push(OsString::from(format!("{MANAGED_PROPERTY}=yes")));
    args.extend(targets.iter().map(OsString::from));
    args
}

fn execute_snapshot_batches(
    actions: &[SnapshotAction],
    recursive: bool,
    settings: &Settings,
    dry_run: bool,
    result: &mut PoolResult,
) {
    let round_count = actions
        .iter()
        .map(|action| action.names.len())
        .max()
        .unwrap_or(0);

    // OpenZFS accepts snapshots for multiple filesystems in one invocation, but
    // rejects multiple snapshot names for the same filesystem. Build one target
    // per dataset in each round, then apply the configured command-size limit.
    for name_index in 0..round_count {
        let targets = actions
            .iter()
            .filter_map(|action| {
                action
                    .names
                    .get(name_index)
                    .map(|name| format!("{}@{name}", action.dataset))
            })
            .collect::<Vec<_>>();
        for chunk in targets.chunks(settings.snapshot_batch_size) {
            match run_zfs(settings, &snapshot_args(recursive, chunk), dry_run, result) {
                Ok(()) => result.snapshots_created += chunk.len(),
                // A multi-target snapshot is all-or-nothing, so one dataset that cannot
                // be snapshotted fails the whole batch. Retry each target on its own so
                // only that dataset is affected.
                Err(failure) if failure.exited && chunk.len() > 1 => {
                    result.logs.push(format!(
                        "[{}] snapshot batch failed; retrying its {} target(s) individually: {}",
                        result.pool,
                        chunk.len(),
                        failure.message
                    ));
                    for target in chunk {
                        let args = snapshot_args(recursive, std::slice::from_ref(target));
                        match run_zfs(settings, &args, dry_run, result) {
                            Ok(()) => result.snapshots_created += 1,
                            Err(failure) => result.errors.push(failure.message),
                        }
                    }
                }
                Err(failure) => result.errors.push(failure.message),
            }
        }
    }
}

fn execute_hooked_snapshot(
    action: &SnapshotAction,
    settings: &Settings,
    dry_run: bool,
    result: &mut PoolResult,
) {
    let types = action
        .kinds
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let names = action.names.join(",");
    let environment = HookEnvironment {
        script: "pre",
        target: &action.dataset,
        names: &names,
        types: &types,
        pre_failure: false,
    };

    let pre_succeeded = if action.policy.pre_snapshot_script.is_empty() {
        true
    } else {
        run_hook(
            &action.policy.pre_snapshot_script,
            action.policy.script_timeout,
            &environment,
            dry_run,
            result,
        )
    };
    if !pre_succeeded && action.policy.no_inconsistent_snapshot {
        result.errors.push(format!(
            "[{}] pre-snapshot hook failed for {}; snapshot skipped",
            action.pool, action.dataset
        ));
    } else {
        for name in &action.names {
            let target = format!("{}@{name}", action.dataset);
            let args = snapshot_args(action.recursive, std::slice::from_ref(&target));
            if invoke_zfs(settings, &args, dry_run, result) {
                result.snapshots_created += 1;
            }
        }
    }

    if !action.policy.post_snapshot_script.is_empty()
        && (pre_succeeded || action.policy.force_post_snapshot_script)
    {
        let environment = HookEnvironment {
            script: "post",
            pre_failure: !pre_succeeded,
            ..environment
        };
        run_hook(
            &action.policy.post_snapshot_script,
            action.policy.script_timeout,
            &environment,
            dry_run,
            result,
        );
    }
}

fn execute_prune_batches(
    action: &PruneAction,
    settings: &Settings,
    dry_run: bool,
    result: &mut PoolResult,
) {
    for names in action.names.chunks(settings.prune_batch_size) {
        let target = format!("{}@{}", action.dataset, names.join(","));
        let args = [OsString::from("destroy"), OsString::from(target)];
        if invoke_zfs(settings, &args, dry_run, result) {
            result.snapshots_pruned += names.len();
        }
    }
}

fn execute_hooked_prunes(
    action: &PruneAction,
    settings: &Settings,
    dry_run: bool,
    result: &mut PoolResult,
) {
    for name in &action.names {
        let environment = HookEnvironment {
            script: "prune",
            target: &action.dataset,
            names: name,
            types: "",
            pre_failure: false,
        };
        if !action.policy.pre_pruning_script.is_empty()
            && !run_hook(
                &action.policy.pre_pruning_script,
                action.policy.script_timeout,
                &environment,
                dry_run,
                result,
            )
        {
            result.errors.push(format!(
                "[{}] pre-pruning hook failed for {}@{}; prune skipped",
                action.pool, action.dataset, name
            ));
            continue;
        }
        let args = [
            OsString::from("destroy"),
            OsString::from(format!("{}@{name}", action.dataset)),
        ];
        if !invoke_zfs(settings, &args, dry_run, result) {
            continue;
        }
        result.snapshots_pruned += 1;
        if !action.policy.pruning_script.is_empty() {
            run_hook(
                &action.policy.pruning_script,
                action.policy.script_timeout,
                &environment,
                dry_run,
                result,
            );
        }
    }
}

/// Why a ZFS command did not succeed. `exited` is true only when the command ran to
/// completion and reported failure itself, which is the one case worth retrying.
struct ZfsFailure {
    message: String,
    exited: bool,
}

fn invoke_zfs(
    settings: &Settings,
    args: &[OsString],
    dry_run: bool,
    result: &mut PoolResult,
) -> bool {
    match run_zfs(settings, args, dry_run, result) {
        Ok(()) => true,
        Err(failure) => {
            result.errors.push(failure.message);
            false
        }
    }
}

fn run_zfs(
    settings: &Settings,
    args: &[OsString],
    dry_run: bool,
    result: &mut PoolResult,
) -> Result<(), ZfsFailure> {
    let command = render_command(settings.zfs_command.as_os_str(), args);
    let failure = |message: String, exited: bool| ZfsFailure { message, exited };
    if args.first().is_some_and(|argument| argument == "destroy")
        && let Err(reason) = validate_snapshot_destroy_command(args)
    {
        return Err(failure(
            format!(
                "[{}] refused unsafe ZFS destroy command {command}: {reason}",
                result.pool
            ),
            false,
        ));
    }
    if dry_run {
        result
            .logs
            .push(format!("[{}] WOULD RUN {command}", result.pool));
        return Ok(());
    }
    result.logs.push(format!("[{}] RUN {command}", result.pool));
    let timeout = settings.zfs_timeout();
    match output_with_timeout(Command::new(&settings.zfs_command).args(args), timeout) {
        Ok(Some(output)) if output.status.success() => Ok(()),
        Ok(Some(output)) => Err(failure(
            format!(
                "[{}] {command} failed with {}: {}",
                result.pool,
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            true,
        )),
        Ok(None) => Err(failure(
            format!(
                "[{}] {command} timed out after {}s (settings.zfs_timeout)",
                result.pool,
                timeout.unwrap_or_default().as_secs()
            ),
            false,
        )),
        Err(error) => Err(failure(
            format!("[{}] failed to execute {command}: {error}", result.pool),
            false,
        )),
    }
}

fn validate_snapshot_destroy_command(args: &[OsString]) -> std::result::Result<(), &'static str> {
    if args.first().map(OsString::as_os_str) != Some(OsStr::new("destroy")) {
        return Err("command is not a snapshot deletion");
    }
    if args.len() != 2 {
        return Err("snapshot deletion requires exactly one target and no flags");
    }
    let Some(target) = args[1].to_str() else {
        return Err("snapshot deletion target is not valid UTF-8");
    };
    let Some((dataset, names)) = target.split_once('@') else {
        return Err("target is not a dataset@snapshot expression");
    };
    if dataset.is_empty()
        || dataset.starts_with('-')
        || dataset.chars().any(char::is_whitespace)
        || dataset.contains(['@', '#', ','])
    {
        return Err("dataset portion is invalid");
    }
    if names.split(',').any(|name| {
        name.is_empty()
            || name.starts_with('-')
            || name.chars().any(char::is_whitespace)
            || name.contains(['@', '#', '%', '/'])
    }) {
        return Err("snapshot name list is invalid");
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct HookEnvironment<'a> {
    script: &'a str,
    target: &'a str,
    names: &'a str,
    types: &'a str,
    pre_failure: bool,
}

fn run_hook(
    command: &[String],
    timeout_seconds: u64,
    environment: &HookEnvironment<'_>,
    dry_run: bool,
    result: &mut PoolResult,
) -> bool {
    let rendered = render_command(OsStr::new(&command[0]), &command[1..]);
    if dry_run {
        result
            .logs
            .push(format!("[{}] WOULD RUN HOOK {rendered}", result.pool));
        return true;
    }
    result
        .logs
        .push(format!("[{}] RUN HOOK {rendered}", result.pool));
    let mut process = Command::new(&command[0]);
    process
        .args(&command[1..])
        .env("ZSNAP_SCRIPT", environment.script)
        .env("ZSNAP_TARGET", environment.target)
        .env("ZSNAP_TARGETS", environment.target)
        .env(
            "ZSNAP_SNAPNAME",
            environment.names.split(',').next().unwrap_or(""),
        )
        .env("ZSNAP_SNAPNAMES", environment.names)
        .env("ZSNAP_TYPES", environment.types)
        .env(
            "ZSNAP_PRE_FAILURE",
            if environment.pre_failure { "1" } else { "0" },
        )
        // Sanoid-compatible aliases make migration of existing hooks straightforward.
        .env("SANOID_SCRIPT", environment.script)
        .env("SANOID_TARGET", environment.target)
        .env("SANOID_TARGETS", environment.target)
        .env(
            "SANOID_SNAPNAME",
            environment.names.split(',').next().unwrap_or(""),
        )
        .env("SANOID_SNAPNAMES", environment.names)
        .env("SANOID_TYPES", environment.types)
        .env(
            "SANOID_PRE_FAILURE",
            if environment.pre_failure { "1" } else { "0" },
        )
        .stdin(Stdio::null())
        // stdout is reserved for zsnap's own report, which may be JSON.
        .stdout(io::stderr())
        .stderr(Stdio::inherit())
        // Own process group, so a timeout also kills whatever the hook started.
        .process_group(0);
    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(error) => {
            result.errors.push(format!(
                "[{}] failed to start hook {rendered}: {error}",
                result.pool
            ));
            return false;
        }
    };
    let timeout = (timeout_seconds > 0).then(|| Duration::from_secs(timeout_seconds));
    match wait_with_timeout(&mut child, timeout) {
        Ok(Some(status)) if status.success() => true,
        Ok(Some(status)) => {
            result.errors.push(format!(
                "[{}] hook {rendered} failed with {status}",
                result.pool
            ));
            false
        }
        Ok(None) => {
            result.errors.push(format!(
                "[{}] hook {rendered} timed out after {timeout_seconds}s",
                result.pool
            ));
            false
        }
        Err(error) => {
            result.errors.push(format!(
                "[{}] failed while waiting for hook {rendered}: {error}",
                result.pool
            ));
            false
        }
    }
}

fn render_command<I, S>(program: &OsStr, args: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut parts = vec![format!("{:?}", program.to_string_lossy())];
    parts.extend(
        args.into_iter()
            .map(|part| format!("{:?}", part.as_ref().to_string_lossy())),
    );
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::path::Path;

    use tempfile::tempdir;

    use super::*;
    use crate::config::Policy;

    /// Writes an executable script through a child process. Writing it from the test
    /// process would briefly hold a writable descriptor that a concurrently forking
    /// test can inherit, making the script fail to execute with ETXTBSY.
    fn write_script(path: &Path, contents: &str) {
        let mut writer = Command::new("/bin/sh")
            .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
            .arg(path)
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = writer.stdin.take().unwrap();
        stdin.write_all(contents.as_bytes()).unwrap();
        drop(stdin);
        assert!(writer.wait().unwrap().success());
    }

    fn snapshot_action(dataset: &str, name: &str, policy: &Policy) -> SnapshotAction {
        SnapshotAction {
            pool: "tank".to_owned(),
            dataset: dataset.to_owned(),
            recursive: false,
            names: vec![name.to_owned()],
            kinds: vec![],
            policy: policy.clone(),
        }
    }

    #[test]
    fn batches_snapshot_targets_into_one_command() {
        let directory = tempdir().unwrap();
        let script = directory.path().join("fake-zfs");
        let log = directory.path().join("calls.log");
        write_script(
            &script,
            &format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n", log.display()),
        );

        let settings = Settings {
            zfs_command: script,
            ..Settings::default()
        };
        let policy = Policy::default();
        let plan = Plan {
            snapshots: vec![
                SnapshotAction {
                    pool: "tank".to_owned(),
                    dataset: "tank/a".to_owned(),
                    recursive: false,
                    names: vec!["autosnap_a_hourly".to_owned()],
                    kinds: vec![],
                    policy: policy.clone(),
                },
                SnapshotAction {
                    pool: "tank".to_owned(),
                    dataset: "tank/b".to_owned(),
                    recursive: false,
                    names: vec!["autosnap_b_hourly".to_owned()],
                    kinds: vec![],
                    policy,
                },
            ],
            ..Plan::default()
        };
        let report = execute(&plan, &settings, false, false).unwrap();
        assert!(report.succeeded());
        assert_eq!(report.snapshots_created, 2);
        let calls = fs::read_to_string(log).unwrap();
        assert_eq!(calls.lines().count(), 1);
        assert!(calls.contains("tank/a@autosnap_a_hourly"));
        assert!(calls.contains("tank/b@autosnap_b_hourly"));
    }

    #[test]
    fn verbose_execution_reports_snapshot_prune_and_pool_timings() {
        let settings = Settings {
            zfs_command: "/bin/true".into(),
            ..Settings::default()
        };
        let policy = Policy::default();
        let plan = Plan {
            snapshots: vec![SnapshotAction {
                pool: "tank".to_owned(),
                dataset: "tank/data".to_owned(),
                recursive: false,
                names: vec!["autosnap_new_hourly".to_owned()],
                kinds: vec![],
                policy: policy.clone(),
            }],
            prunes: vec![PruneAction {
                pool: "tank".to_owned(),
                dataset: "tank/data".to_owned(),
                names: vec!["autosnap_old_hourly".to_owned()],
                policy,
            }],
            ..Plan::default()
        };

        let report = execute(&plan, &settings, false, true).unwrap();
        assert!(report.succeeded(), "{:?}", report.errors);
        let timing = report
            .logs
            .iter()
            .find(|line| line.starts_with("[tank] timing:"))
            .expect("missing verbose pool timing");
        assert!(timing.contains("snapshots "));
        assert!(timing.contains("pruning "));
        assert!(timing.contains("pool total "));
    }

    #[test]
    fn separates_multiple_snapshot_names_for_each_dataset_into_rounds() {
        let directory = tempdir().unwrap();
        let script = directory.path().join("fake-zfs");
        let log = directory.path().join("calls.log");
        write_script(
            &script,
            &format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n", log.display()),
        );

        let settings = Settings {
            zfs_command: script,
            ..Settings::default()
        };
        let policy = Policy::default();
        let plan = Plan {
            snapshots: vec![
                SnapshotAction {
                    pool: "tank".to_owned(),
                    dataset: "tank/a".to_owned(),
                    recursive: false,
                    names: vec![
                        "autosnap_a_daily".to_owned(),
                        "autosnap_a_weekly".to_owned(),
                        "autosnap_a_monthly".to_owned(),
                    ],
                    kinds: vec![],
                    policy: policy.clone(),
                },
                SnapshotAction {
                    pool: "tank".to_owned(),
                    dataset: "tank/b".to_owned(),
                    recursive: false,
                    names: vec![
                        "autosnap_b_daily".to_owned(),
                        "autosnap_b_weekly".to_owned(),
                    ],
                    kinds: vec![],
                    policy,
                },
            ],
            ..Plan::default()
        };

        let report = execute(&plan, &settings, false, false).unwrap();
        assert!(report.succeeded(), "{:?}", report.errors);
        assert_eq!(report.snapshots_created, 5);
        let calls = fs::read_to_string(log).unwrap();
        assert_eq!(
            calls.lines().collect::<Vec<_>>(),
            [
                "snapshot -o org.zsnap:managed=yes tank/a@autosnap_a_daily tank/b@autosnap_b_daily",
                "snapshot -o org.zsnap:managed=yes tank/a@autosnap_a_weekly tank/b@autosnap_b_weekly",
                "snapshot -o org.zsnap:managed=yes tank/a@autosnap_a_monthly",
            ]
        );
    }

    #[test]
    fn hooked_snapshot_names_use_separate_zfs_commands() {
        let directory = tempdir().unwrap();
        let script = directory.path().join("fake-zfs");
        let log = directory.path().join("calls.log");
        write_script(
            &script,
            &format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n", log.display()),
        );

        let settings = Settings {
            zfs_command: script,
            ..Settings::default()
        };
        let policy = Policy {
            pre_snapshot_script: vec!["/bin/true".to_owned()],
            ..Policy::default()
        };
        let plan = Plan {
            snapshots: vec![SnapshotAction {
                pool: "tank".to_owned(),
                dataset: "tank/data".to_owned(),
                recursive: false,
                names: vec![
                    "autosnap_data_daily".to_owned(),
                    "autosnap_data_weekly".to_owned(),
                ],
                kinds: vec![],
                policy,
            }],
            ..Plan::default()
        };

        let report = execute(&plan, &settings, false, false).unwrap();
        assert!(report.succeeded(), "{:?}", report.errors);
        assert_eq!(report.snapshots_created, 2);
        let calls = fs::read_to_string(log).unwrap();
        assert_eq!(
            calls.lines().collect::<Vec<_>>(),
            [
                "snapshot -o org.zsnap:managed=yes tank/data@autosnap_data_daily",
                "snapshot -o org.zsnap:managed=yes tank/data@autosnap_data_weekly",
            ]
        );
    }

    #[test]
    fn destroy_safety_gate_accepts_only_snapshot_targets() {
        let safe = [
            OsString::from("destroy"),
            OsString::from("tank/data@autosnap_daily,autosnap_weekly"),
        ];
        assert!(validate_snapshot_destroy_command(&safe).is_ok());

        let unsafe_commands = [
            vec![OsString::from("destroy"), OsString::from("tank/data")],
            vec![
                OsString::from("destroy"),
                OsString::from("-r"),
                OsString::from("tank/data@autosnap_daily"),
            ],
            vec![
                OsString::from("destroy"),
                OsString::from("tank/data#bookmark"),
            ],
            vec![
                OsString::from("destroy"),
                OsString::from("tank/data@old%new"),
            ],
            vec![OsString::from("destroy"), OsString::from("tank/data@")],
            vec![
                OsString::from("destroy"),
                OsString::from("tank/data@autosnap/child"),
            ],
        ];
        for command in unsafe_commands {
            assert!(
                validate_snapshot_destroy_command(&command).is_err(),
                "accepted unsafe command: {command:?}"
            );
        }
    }

    #[test]
    fn unsafe_destroy_never_reaches_the_zfs_executable() {
        let directory = tempdir().unwrap();
        let script = directory.path().join("fake-zfs");
        let marker = directory.path().join("executed");
        write_script(&script, &format!("#!/bin/sh\n: > '{}'\n", marker.display()));

        let settings = Settings {
            zfs_command: script,
            ..Settings::default()
        };
        let mut result = PoolResult {
            pool: "tank".to_owned(),
            ..PoolResult::default()
        };
        let args = [OsString::from("destroy"), OsString::from("tank/data")];

        assert!(!invoke_zfs(&settings, &args, false, &mut result));
        assert!(!marker.exists());
        assert_eq!(result.errors.len(), 1);
        assert!(result.errors[0].contains("refused unsafe ZFS destroy command"));
    }

    #[test]
    fn configured_batch_limits_split_and_execute_long_argument_sets() {
        let directory = tempdir().unwrap();
        let script = directory.path().join("fake-zfs");
        let log = directory.path().join("calls.log");
        write_script(
            &script,
            &format!("#!/bin/sh\nprintf '%s\\n' \"$1\" >> '{}'\n", log.display()),
        );

        let long_suffix = "x".repeat(200);
        let snapshot_names = (0..3)
            .map(|index| format!("autosnap_{index:03}_{long_suffix}_hourly"))
            .collect::<Vec<_>>();
        let prune_names = (0..3)
            .map(|index| format!("autosnap_{index:03}_{long_suffix}_daily"))
            .collect::<Vec<_>>();
        let policy = Policy::default();
        let plan = Plan {
            snapshots: vec![SnapshotAction {
                pool: "tank".to_owned(),
                dataset: "tank/data".to_owned(),
                recursive: false,
                names: snapshot_names,
                kinds: vec![],
                policy: policy.clone(),
            }],
            prunes: vec![PruneAction {
                pool: "tank".to_owned(),
                dataset: "tank/data".to_owned(),
                names: prune_names,
                policy,
            }],
            ..Plan::default()
        };
        let settings = Settings {
            zfs_command: script,
            snapshot_batch_size: 2,
            prune_batch_size: 2,
            ..Settings::default()
        };

        let report = execute(&plan, &settings, false, false).unwrap();
        assert!(report.succeeded(), "{:?}", report.errors);
        assert_eq!(report.snapshots_created, 3);
        assert_eq!(report.snapshots_pruned, 3);
        let calls = fs::read_to_string(log).unwrap();
        assert_eq!(
            calls.lines().collect::<Vec<_>>(),
            ["snapshot", "snapshot", "snapshot", "destroy", "destroy"]
        );
    }

    #[test]
    fn snapshot_failure_does_not_block_pruning() {
        let directory = tempdir().unwrap();
        let script = directory.path().join("fake-zfs");
        let log = directory.path().join("calls.log");
        write_script(
            &script,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$1\" >> '{}'\nif [ \"$1\" = snapshot ]; then echo 'out of space' >&2; exit 1; fi\n",
                log.display()
            ),
        );
        let settings = Settings {
            zfs_command: script,
            ..Settings::default()
        };
        let policy = Policy::default();
        let plan = Plan {
            snapshots: vec![snapshot_action("tank/data", "autosnap_new_hourly", &policy)],
            prunes: vec![PruneAction {
                pool: "tank".to_owned(),
                dataset: "tank/data".to_owned(),
                names: vec!["autosnap_old_hourly".to_owned()],
                policy,
            }],
            ..Plan::default()
        };
        let report = execute(&plan, &settings, false, false).unwrap();
        assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
        assert!(report.errors[0].contains("out of space"));
        assert_eq!(report.snapshots_created, 0);
        assert_eq!(report.snapshots_pruned, 1);
        let calls = fs::read_to_string(log).unwrap();
        assert_eq!(calls.lines().collect::<Vec<_>>(), ["snapshot", "destroy"]);
    }

    #[test]
    fn a_failed_snapshot_batch_is_retried_one_dataset_at_a_time() {
        let directory = tempdir().unwrap();
        let script = directory.path().join("fake-zfs");
        let log = directory.path().join("calls.log");
        // Like ZFS, fail the whole command when any one target cannot be snapshotted.
        write_script(
            &script,
            &format!(
                "#!/bin/sh\ncase \"$*\" in *tank/bad@*) echo 'out of space' >&2; exit 1 ;; esac\nshift 3\nprintf '%s\\n' \"$@\" >> '{}'\n",
                log.display()
            ),
        );
        let settings = Settings {
            zfs_command: script,
            ..Settings::default()
        };
        let policy = Policy::default();
        let plan = Plan {
            snapshots: vec![
                snapshot_action("tank/a", "autosnap_a_hourly", &policy),
                snapshot_action("tank/bad", "autosnap_bad_hourly", &policy),
                snapshot_action("tank/c", "autosnap_c_hourly", &policy),
            ],
            ..Plan::default()
        };
        let report = execute(&plan, &settings, false, false).unwrap();
        assert_eq!(report.snapshots_created, 2);
        assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
        assert!(report.errors[0].contains("tank/bad@autosnap_bad_hourly"));
        assert!(!report.errors[0].contains("tank/a@"));
        let created = fs::read_to_string(log).unwrap();
        assert_eq!(
            created.lines().collect::<Vec<_>>(),
            ["tank/a@autosnap_a_hourly", "tank/c@autosnap_c_hourly"]
        );
    }

    #[test]
    fn a_zfs_command_that_never_returns_times_out() {
        let directory = tempdir().unwrap();
        let script = directory.path().join("fake-zfs");
        write_script(&script, "#!/bin/sh\nexec sleep 60\n");
        let settings = Settings {
            zfs_command: script,
            zfs_timeout: 1,
            ..Settings::default()
        };
        let policy = Policy::default();
        let plan = Plan {
            snapshots: vec![
                snapshot_action("tank/a", "autosnap_a_hourly", &policy),
                snapshot_action("tank/b", "autosnap_b_hourly", &policy),
            ],
            ..Plan::default()
        };
        let started = Instant::now();
        let report = execute(&plan, &settings, false, false).unwrap();
        // One timeout for the batch; a timed-out batch is not retried per dataset.
        assert!(started.elapsed() < Duration::from_secs(20));
        assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
        assert!(report.errors[0].contains("timed out after 1s"));
        assert_eq!(report.snapshots_created, 0);
    }

    fn shell_hook(script: String) -> Vec<String> {
        vec!["/bin/sh".to_owned(), "-c".to_owned(), script]
    }

    #[test]
    fn a_failed_pre_snapshot_hook_follows_the_consistency_settings() {
        // (no_inconsistent_snapshot, force_post_snapshot_script) -> (snapshot taken, post ran)
        for (no_inconsistent, force_post, snapshot_taken, post_ran) in [
            (true, false, false, false),
            (true, true, false, true),
            (false, false, true, false),
        ] {
            let directory = tempdir().unwrap();
            let script = directory.path().join("fake-zfs");
            let zfs_log = directory.path().join("zfs.log");
            let hook_log = directory.path().join("hook.log");
            write_script(
                &script,
                &format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$1\" >> '{}'\n",
                    zfs_log.display()
                ),
            );
            let settings = Settings {
                zfs_command: script,
                ..Settings::default()
            };
            let policy = Policy {
                pre_snapshot_script: shell_hook("exit 1".to_owned()),
                post_snapshot_script: shell_hook(format!(
                    "echo \"post pre_failure=$ZSNAP_PRE_FAILURE\" >> '{}'",
                    hook_log.display()
                )),
                no_inconsistent_snapshot: no_inconsistent,
                force_post_snapshot_script: force_post,
                ..Policy::default()
            };
            let plan = Plan {
                snapshots: vec![snapshot_action("tank/data", "autosnap_new_hourly", &policy)],
                ..Plan::default()
            };
            let report = execute(&plan, &settings, false, false).unwrap();
            let case = format!("no_inconsistent={no_inconsistent} force_post={force_post}");
            assert!(!report.succeeded(), "{case}");
            assert_eq!(
                report.snapshots_created,
                usize::from(snapshot_taken),
                "{case}"
            );
            assert_eq!(zfs_log.exists(), snapshot_taken, "{case}");
            assert_eq!(
                fs::read_to_string(&hook_log).unwrap_or_default(),
                if post_ran { "post pre_failure=1\n" } else { "" },
                "{case}"
            );
        }
    }

    #[test]
    fn prune_hooks_gate_and_follow_each_destroy() {
        let directory = tempdir().unwrap();
        let script = directory.path().join("fake-zfs");
        let zfs_log = directory.path().join("zfs.log");
        let hook_log = directory.path().join("hook.log");
        write_script(
            &script,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n",
                zfs_log.display()
            ),
        );
        let settings = Settings {
            zfs_command: script,
            ..Settings::default()
        };
        let policy = Policy {
            // Refuse to prune "a"; allow everything else.
            pre_pruning_script: shell_hook(
                "[ \"$ZSNAP_SNAPNAME\" != autosnap_a_hourly ]".to_owned(),
            ),
            pruning_script: shell_hook(format!(
                "echo \"$ZSNAP_SCRIPT $ZSNAP_TARGET@$ZSNAP_SNAPNAME\" >> '{}'",
                hook_log.display()
            )),
            ..Policy::default()
        };
        let plan = Plan {
            prunes: vec![PruneAction {
                pool: "tank".to_owned(),
                dataset: "tank/data".to_owned(),
                names: vec![
                    "autosnap_a_hourly".to_owned(),
                    "autosnap_b_hourly".to_owned(),
                ],
                policy,
            }],
            ..Plan::default()
        };
        let report = execute(&plan, &settings, false, false).unwrap();
        assert_eq!(report.snapshots_pruned, 1);
        assert_eq!(report.errors.len(), 2, "{:?}", report.errors);
        assert!(report.errors.iter().any(|error| {
            error.contains("pre-pruning hook failed for tank/data@autosnap_a_hourly; prune skipped")
        }));
        assert_eq!(
            fs::read_to_string(zfs_log).unwrap(),
            "destroy tank/data@autosnap_b_hourly\n"
        );
        assert_eq!(
            fs::read_to_string(hook_log).unwrap(),
            "prune tank/data@autosnap_b_hourly\n"
        );
    }

    #[test]
    fn independent_pools_reach_the_executor_at_the_same_time() {
        let directory = tempdir().unwrap();
        let script = directory.path().join("fake-zfs");
        let state = directory.path().join("state");
        fs::create_dir(&state).unwrap();
        write_script(
            &script,
            &format!(
                "#!/bin/sh\ncase \"$*\" in\n  *tank/*) own=tank; other=backup ;;\n  *backup/*) own=backup; other=tank ;;\n  *) exit 64 ;;\nesac\n: > '{state}/'$own\nattempt=0\nwhile [ ! -e '{state}/'$other ]; do\n  attempt=$((attempt + 1))\n  [ \"$attempt\" -lt 100 ] || exit 70\n  sleep 0.01\ndone\n",
                state = state.display()
            ),
        );

        let settings = Settings {
            zfs_command: script,
            max_parallel_pools: 2,
            ..Settings::default()
        };
        let policy = Policy::default();
        let plan = Plan {
            snapshots: vec![
                SnapshotAction {
                    pool: "tank".to_owned(),
                    dataset: "tank/data".to_owned(),
                    recursive: false,
                    names: vec!["autosnap_tank_hourly".to_owned()],
                    kinds: vec![],
                    policy: policy.clone(),
                },
                SnapshotAction {
                    pool: "backup".to_owned(),
                    dataset: "backup/data".to_owned(),
                    recursive: false,
                    names: vec!["autosnap_backup_hourly".to_owned()],
                    kinds: vec![],
                    policy,
                },
            ],
            ..Plan::default()
        };
        let report = execute(&plan, &settings, false, false).unwrap();
        assert!(report.succeeded(), "{:?}", report.errors);
        assert_eq!(report.snapshots_created, 2);
    }
}
