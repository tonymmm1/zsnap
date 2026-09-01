use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rayon::prelude::*;
use serde::Serialize;

use crate::config::Settings;
use crate::planner::{Plan, PruneAction, SnapshotAction};
use crate::zfs::MANAGED_PROPERTY;

#[derive(Debug, Clone, Default, Serialize)]
pub struct ExecutionReport {
    pub dry_run: bool,
    pub snapshots_created: usize,
    pub snapshots_pruned: usize,
    pub prunes_skipped_after_snapshot_failure: usize,
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
    prunes_skipped: usize,
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
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(thread_count.max(1))
        .thread_name(|index| format!("zsnap-pool-{index}"))
        .build()
        .context("failed to create the pool execution thread set")?;

    let mut results = pool.install(|| {
        work.into_par_iter()
            .map(|pool_work| execute_pool(pool_work, settings, dry_run, verbose))
            .collect::<Vec<_>>()
    });
    results.sort_by(|left, right| left.pool.cmp(&right.pool));

    let mut report = ExecutionReport {
        dry_run,
        ..ExecutionReport::default()
    };
    for mut result in results {
        report.snapshots_created += result.snapshots_created;
        report.snapshots_pruned += result.snapshots_pruned;
        report.prunes_skipped_after_snapshot_failure += result.prunes_skipped;
        report.errors.append(&mut result.errors);
        report.logs.append(&mut result.logs);
    }
    Ok(report)
}

fn execute_pool(work: PoolWork, settings: &Settings, dry_run: bool, verbose: bool) -> PoolResult {
    let mut result = PoolResult {
        pool: work.pool.clone(),
        ..PoolResult::default()
    };
    if verbose {
        result
            .logs
            .push(format!("[{}] starting pool work", work.pool));
    }
    let mut snapshot_failed = false;
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

    snapshot_failed |=
        !execute_snapshot_batches(&batchable_regular, false, settings, dry_run, &mut result);
    snapshot_failed |=
        !execute_snapshot_batches(&batchable_recursive, true, settings, dry_run, &mut result);
    for action in hooked {
        if !execute_hooked_snapshot(&action, settings, dry_run, &mut result) {
            snapshot_failed = true;
        }
    }

    if snapshot_failed && !work.prunes.is_empty() {
        result.prunes_skipped = work.prunes.iter().map(|action| action.names.len()).sum();
        result.logs.push(format!(
            "[{}] skipped {} prune(s) because snapshot creation or a snapshot hook failed",
            work.pool, result.prunes_skipped
        ));
        return result;
    }

    for action in work.prunes {
        if action.policy.has_prune_hooks() {
            execute_hooked_prunes(&action, settings, dry_run, &mut result);
        } else {
            execute_prune_batches(&action, settings, dry_run, &mut result);
        }
    }
    result
}

fn execute_snapshot_batches(
    actions: &[SnapshotAction],
    recursive: bool,
    settings: &Settings,
    dry_run: bool,
    result: &mut PoolResult,
) -> bool {
    let mut succeeded = true;
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
            let mut args: Vec<OsString> = vec![OsString::from("snapshot")];
            if recursive {
                args.push(OsString::from("-r"));
            }
            args.push(OsString::from("-o"));
            args.push(OsString::from(format!("{MANAGED_PROPERTY}=yes")));
            args.extend(chunk.iter().map(OsString::from));
            if invoke_zfs(settings, &args, dry_run, result) {
                result.snapshots_created += chunk.len();
            } else {
                succeeded = false;
            }
        }
    }
    succeeded
}

fn execute_hooked_snapshot(
    action: &SnapshotAction,
    settings: &Settings,
    dry_run: bool,
    result: &mut PoolResult,
) -> bool {
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
        if action.policy.force_post_snapshot_script
            && !action.policy.post_snapshot_script.is_empty()
        {
            let environment = HookEnvironment {
                script: "post",
                pre_failure: true,
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
        return false;
    }

    let mut snapshot_succeeded = true;
    for name in &action.names {
        let mut args: Vec<OsString> = vec![OsString::from("snapshot")];
        if action.recursive {
            args.push(OsString::from("-r"));
        }
        args.push(OsString::from("-o"));
        args.push(OsString::from(format!("{MANAGED_PROPERTY}=yes")));
        args.push(OsString::from(format!("{}@{name}", action.dataset)));
        if invoke_zfs(settings, &args, dry_run, result) {
            result.snapshots_created += 1;
        } else {
            snapshot_succeeded = false;
        }
    }

    let mut post_succeeded = true;
    if !action.policy.post_snapshot_script.is_empty()
        && (pre_succeeded || action.policy.force_post_snapshot_script)
    {
        let environment = HookEnvironment {
            script: "post",
            pre_failure: !pre_succeeded,
            ..environment
        };
        post_succeeded = run_hook(
            &action.policy.post_snapshot_script,
            action.policy.script_timeout,
            &environment,
            dry_run,
            result,
        );
    }
    pre_succeeded && snapshot_succeeded && post_succeeded
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

fn invoke_zfs(
    settings: &Settings,
    args: &[OsString],
    dry_run: bool,
    result: &mut PoolResult,
) -> bool {
    let command = render_command(settings.zfs_command.as_os_str(), args);
    if args.first().is_some_and(|argument| argument == "destroy") {
        if let Err(reason) = validate_snapshot_destroy_command(args) {
            result.errors.push(format!(
                "[{}] refused unsafe ZFS destroy command {command}: {reason}",
                result.pool
            ));
            return false;
        }
    }
    if dry_run {
        result
            .logs
            .push(format!("[{}] WOULD RUN {command}", result.pool));
        return true;
    }
    result.logs.push(format!("[{}] RUN {command}", result.pool));
    match Command::new(&settings.zfs_command).args(args).output() {
        Ok(output) if output.status.success() => true,
        Ok(output) => {
            result.errors.push(format!(
                "[{}] {command} failed with {}: {}",
                result.pool,
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
            false
        }
        Err(error) => {
            result.errors.push(format!(
                "[{}] failed to execute {command}: {error}",
                result.pool
            ));
            false
        }
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
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
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
    let deadline =
        (timeout_seconds > 0).then(|| Instant::now() + Duration::from_secs(timeout_seconds));
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return true,
            Ok(Some(status)) => {
                result.errors.push(format!(
                    "[{}] hook {rendered} failed with {status}",
                    result.pool
                ));
                return false;
            }
            Ok(None) => {}
            Err(error) => {
                result.errors.push(format!(
                    "[{}] failed while waiting for hook {rendered}: {error}",
                    result.pool
                ));
                return false;
            }
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            let _ = child.kill();
            let _ = child.wait();
            result.errors.push(format!(
                "[{}] hook {rendered} timed out after {timeout_seconds}s",
                result.pool
            ));
            return false;
        }
        thread::sleep(Duration::from_millis(25));
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
    use std::os::unix::fs::PermissionsExt;

    use tempfile::tempdir;

    use super::*;
    use crate::config::Policy;

    #[test]
    fn batches_snapshot_targets_into_one_command() {
        let directory = tempdir().unwrap();
        let script = directory.path().join("fake-zfs");
        let log = directory.path().join("calls.log");
        fs::write(
            &script,
            format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n", log.display()),
        )
        .unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();

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
    fn separates_multiple_snapshot_names_for_each_dataset_into_rounds() {
        let directory = tempdir().unwrap();
        let script = directory.path().join("fake-zfs");
        let log = directory.path().join("calls.log");
        fs::write(
            &script,
            format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n", log.display()),
        )
        .unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();

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
        fs::write(
            &script,
            format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n", log.display()),
        )
        .unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();

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
        fs::write(&script, format!("#!/bin/sh\n: > '{}'\n", marker.display())).unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();

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
        fs::write(
            &script,
            format!("#!/bin/sh\nprintf '%s\\n' \"$1\" >> '{}'\n", log.display()),
        )
        .unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();

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
    fn snapshot_failure_stops_pruning_for_that_pool() {
        let settings = Settings {
            zfs_command: "false".into(),
            ..Settings::default()
        };
        let policy = Policy::default();
        let plan = Plan {
            snapshots: vec![SnapshotAction {
                pool: "tank".to_owned(),
                dataset: "tank/data".to_owned(),
                recursive: false,
                names: vec!["autosnap_2026-08-27_12:00:00_hourly".to_owned()],
                kinds: vec![crate::model::SnapshotKind::Hourly],
                policy: policy.clone(),
            }],
            prunes: vec![PruneAction {
                pool: "tank".to_owned(),
                dataset: "tank/data".to_owned(),
                names: vec!["autosnap_2026-08-20_12:00:00_hourly".to_owned()],
                policy,
            }],
            ..Plan::default()
        };
        let report = execute(&plan, &settings, false, false).unwrap();
        assert!(!report.succeeded());
        assert_eq!(report.prunes_skipped_after_snapshot_failure, 1);
        assert_eq!(report.snapshots_pruned, 0);
        let commands = report
            .logs
            .iter()
            .filter(|log| log.contains("] RUN "))
            .collect::<Vec<_>>();
        assert_eq!(commands.len(), 1);
        assert!(commands[0].contains(" RUN \"false\" \"snapshot\" "));
        assert!(
            report
                .logs
                .iter()
                .any(|log| log.contains("skipped 1 prune"))
        );
        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].contains("failed with"));
    }

    #[test]
    fn independent_pools_reach_the_executor_at_the_same_time() {
        let directory = tempdir().unwrap();
        let script = directory.path().join("fake-zfs");
        let state = directory.path().join("state");
        fs::create_dir(&state).unwrap();
        fs::write(
            &script,
            format!(
                "#!/bin/sh\ncase \"$*\" in\n  *tank/*) own=tank; other=backup ;;\n  *backup/*) own=backup; other=tank ;;\n  *) exit 64 ;;\nesac\n: > '{state}/'$own\nattempt=0\nwhile [ ! -e '{state}/'$other ]; do\n  attempt=$((attempt + 1))\n  [ \"$attempt\" -lt 100 ] || exit 70\n  sleep 0.01\ndone\n",
                state = state.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();

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
