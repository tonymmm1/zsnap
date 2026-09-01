use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use serde_json::Value;
use tempfile::tempdir;
use zsnap::config::Config;

#[test]
fn plans_and_executes_against_fake_zfs_tools() {
    let directory = tempdir().unwrap();
    let zfs = directory.path().join("zfs");
    let zpool = directory.path().join("zpool");
    let calls = directory.path().join("calls.log");
    let lock = directory.path().join("zsnap.lock");
    let cache = directory.path().join("zsnap.cache");
    let config = directory.path().join("zsnap.toml");

    write_executable(
        &zfs,
        &format!(
            r#"#!/bin/sh
case "$1" in
  list)
    case "$*" in
      *filesystem,volume*)
        printf 'tank\ntank/data\nbackup\nbackup/data\n'
        ;;
      *snapshot*)
        :
        ;;
    esac
    ;;
  snapshot|destroy)
    printf '%s\n' "$*" >> '{}'
    ;;
  *)
    exit 64
    ;;
esac
"#,
            calls.display()
        ),
    );
    write_executable(
        &zpool,
        "#!/bin/sh\nprintf 'tank\\t80%%\\nbackup\\t50%%\\n'\n",
    );
    fs::write(
        &config,
        format!(
            r#"version = 1

[settings]
zfs_command = "{}"
zpool_command = "{}"
lock_file = "{}"
cache_file = "{}"
max_parallel_pools = 0

[template_test]
autosnap = true
autoprune = true
frequently = 0
hourly = 1
daily = 0
weekly = 0
monthly = 0
yearly = 0

[tank/data]
use_templates = [test]

[backup/data]
use_templates = [test]
"#,
            zfs.display(),
            zpool.display(),
            lock.display(),
            cache.display()
        ),
    )
    .unwrap();

    let plan = Command::new(env!("CARGO_BIN_EXE_zsnap"))
        .args(["--config", config.to_str().unwrap(), "--json", "plan"])
        .output()
        .unwrap();
    assert!(
        plan.status.success(),
        "{}",
        String::from_utf8_lossy(&plan.stderr)
    );
    let plan: Value = serde_json::from_slice(&plan.stdout).unwrap();
    assert_eq!(plan["snapshots"].as_array().unwrap().len(), 2);
    assert_eq!(plan["prunes"].as_array().unwrap().len(), 0);

    let dry_run = Command::new(env!("CARGO_BIN_EXE_zsnap"))
        .args([
            "--config",
            config.to_str().unwrap(),
            "--json",
            "--verbose",
            "run",
            "--dry-run",
        ])
        .output()
        .unwrap();
    assert!(
        dry_run.status.success(),
        "{}",
        String::from_utf8_lossy(&dry_run.stderr)
    );
    let dry_run_report: Value = serde_json::from_slice(&dry_run.stdout).unwrap();
    let dry_run_logs = dry_run_report["logs"].as_array().unwrap();
    assert!(dry_run_logs.iter().any(|line| {
        line.as_str()
            .is_some_and(|line| line.starts_with("[tank] timing:"))
    }));
    assert!(dry_run_logs.iter().any(|line| {
        line.as_str()
            .is_some_and(|line| line.starts_with("[overall] timing: core run "))
    }));
    assert!(
        !calls.exists(),
        "dry-run unexpectedly invoked a mutating fake-ZFS command"
    );

    let run = Command::new(env!("CARGO_BIN_EXE_zsnap"))
        .args(["--config", config.to_str().unwrap(), "--json", "run"])
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let report: Value = serde_json::from_slice(&run.stdout).unwrap();
    assert_eq!(report["snapshots_created"], 2);
    assert_eq!(calls_for(&calls, "snapshot"), 2); // One batched call per independent pool.
    assert!(cache.exists(), "successful run did not update status cache");

    let status = Command::new(env!("CARGO_BIN_EXE_zsnap"))
        .args(["--config", config.to_str().unwrap(), "--json", "status"])
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["source"], "cache");
    assert_eq!(status["status"]["totals"]["pools"], 2);
    assert_eq!(status["status"]["totals"]["datasets"], 4);
    assert_eq!(status["status"]["totals"]["snapshots"], 2);
    assert_eq!(status["status"]["totals"]["managed_snapshots"], 2);

    let info = Command::new(env!("CARGO_BIN_EXE_zsnap"))
        .args(["--config", config.to_str().unwrap(), "info", "-v"])
        .output()
        .unwrap();
    assert!(
        info.status.success(),
        "{}",
        String::from_utf8_lossy(&info.stderr)
    );
    let info = String::from_utf8_lossy(&info.stdout);
    assert!(info.contains("zpools: 2 (backup, tank)"), "{info}");
    assert!(info.contains("pool details:"), "{info}");
    assert!(info.contains("dataset details:"), "{info}");
}

#[test]
fn distributed_example_is_inert_until_a_dataset_is_uncommented() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let raw = fs::read_to_string(root.join("config.example.toml")).unwrap();
    let config = Config::parse(&raw).unwrap();
    assert!(config.datasets.is_empty());

    let output = Command::new(env!("CARGO_BIN_EXE_zsnap"))
        .args([
            "--config",
            root.join("config.example.toml").to_str().unwrap(),
            "check",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("at least one dataset"));
}

#[test]
fn installed_starter_configuration_never_assumes_a_dataset() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let raw = fs::read_to_string(root.join("contrib/zsnap.toml.example")).unwrap();
    let config = Config::parse(&raw).unwrap();
    assert!(config.datasets.is_empty());
    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("at least one dataset")
    );
}

#[test]
fn status_refreshes_atomically_then_reads_without_querying_zfs() {
    let directory = tempdir().unwrap();
    let zfs = directory.path().join("zfs");
    let zpool = directory.path().join("zpool");
    let config = directory.path().join("zsnap.toml");
    let cache = directory.path().join("zsnap.cache");
    let lock = directory.path().join("zsnap.lock");

    write_executable(
        &zfs,
        r#"#!/bin/sh
case "$*" in
  *filesystem,volume*) printf 'tank\ntank/data\ntank/data/vm\n' ;;
  *snapshot*) printf 'tank/data@autosnap_hourly\t100\tyes\t0\t-\ntank/data@manual\t90\t-\t0\t-\n' ;;
  *) exit 64 ;;
esac
"#,
    );
    write_executable(&zpool, "#!/bin/sh\nprintf 'tank\\t63%%\\n'\n");
    fs::write(
        &config,
        format!(
            r#"version = 1
[settings]
zfs_command = "{}"
zpool_command = "{}"
lock_file = "{}"
cache_file = "{}"
[tank]
recursive = true
"#,
            zfs.display(),
            zpool.display(),
            lock.display(),
            cache.display()
        ),
    )
    .unwrap();

    let refreshed = Command::new(env!("CARGO_BIN_EXE_zsnap"))
        .args([
            "--config",
            config.to_str().unwrap(),
            "--json",
            "status",
            "--refresh",
        ])
        .output()
        .unwrap();
    assert!(
        refreshed.status.success(),
        "{}",
        String::from_utf8_lossy(&refreshed.stderr)
    );
    let refreshed: Value = serde_json::from_slice(&refreshed.stdout).unwrap();
    assert_eq!(refreshed["source"], "refreshed");
    assert_eq!(refreshed["status"]["totals"]["pools"], 1);
    assert_eq!(refreshed["status"]["totals"]["datasets"], 3);
    assert_eq!(refreshed["status"]["totals"]["snapshots"], 2);
    assert_eq!(refreshed["status"]["totals"]["managed_snapshots"], 1);
    assert_eq!(
        fs::metadata(&cache).unwrap().permissions().mode() & 0o777,
        0o600
    );

    write_executable(&zfs, "#!/bin/sh\nexit 99\n");
    write_executable(&zpool, "#!/bin/sh\nexit 99\n");
    let cached = Command::new(env!("CARGO_BIN_EXE_zsnap"))
        .args(["--config", config.to_str().unwrap(), "--json", "status"])
        .output()
        .unwrap();
    assert!(
        cached.status.success(),
        "{}",
        String::from_utf8_lossy(&cached.stderr)
    );
    let cached: Value = serde_json::from_slice(&cached.stdout).unwrap();
    assert_eq!(cached["source"], "cache");
    assert_eq!(cached["status"]["totals"], refreshed["status"]["totals"]);
}

#[test]
fn probe_reports_all_missing_datasets_without_raw_zfs_command_noise() {
    let directory = tempdir().unwrap();
    let zfs = directory.path().join("zfs");
    let zpool = directory.path().join("zpool");
    let config = directory.path().join("zsnap.toml");

    write_executable(
        &zfs,
        "#!/bin/sh\necho \"cannot open 'pool/missing-b': dataset does not exist\" >&2\necho \"cannot open 'pool/missing-a': dataset does not exist\" >&2\nexit 1\n",
    );
    write_executable(&zpool, "#!/bin/sh\nexit 99\n");
    fs::write(
        &config,
        format!(
            r#"version = 1

[settings]
zfs_command = "{}"
zpool_command = "{}"

[pool/missing-a]

[pool/missing-b]
"#,
            zfs.display(),
            zpool.display()
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_zsnap"))
        .args(["--config", config.to_str().unwrap(), "check", "--probe"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("configured ZFS dataset sections reference missing names"));
    assert!(stderr.contains("  - pool/missing-a"));
    assert!(stderr.contains("  - pool/missing-b"));
    assert!(stderr.contains("correct or remove these sections"));
    assert!(!stderr.contains("zfs list -H"), "{stderr}");
}

#[test]
fn notify_test_requires_an_enabled_target() {
    let directory = tempdir().unwrap();
    let config = directory.path().join("zsnap.toml");
    fs::write(
        &config,
        r#"version = 1
[tank/data]
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_zsnap"))
        .args([
            "--config",
            config.to_str().unwrap(),
            "--json",
            "notify-test",
            "--message",
            "integration check",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("no enabled notification webhooks are configured")
    );
}

#[test]
fn notify_test_loads_the_environment_file_next_to_the_config() {
    let directory = tempdir().unwrap();
    let config = directory.path().join("zsnap.toml");
    let environment = directory.path().join("webhooks.env");
    fs::write(
        &config,
        r#"version = 1

[notifications]
max_attempts = 1

[[notifications.webhooks]]
name = "storage-discord"
kind = "discord"
url_env = "ZSNAP_CLI_TEST_DISCORD_7D5ED06D"

[tank]
"#,
    )
    .unwrap();
    fs::write(
        environment,
        "ZSNAP_CLI_TEST_DISCORD_7D5ED06D='discord://loaded-from-file'\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_zsnap"))
        .args(["--config", config.to_str().unwrap(), "notify-test"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("webhook URL must use HTTPS"), "{stderr}");
    assert!(!stderr.contains("environment variable ZSNAP_CLI_TEST_DISCORD_7D5ED06D is not set"));
}

#[test]
fn failed_zfs_run_attempts_a_failure_notification_and_stays_failed() {
    let directory = tempdir().unwrap();
    let zfs = directory.path().join("zfs");
    let zpool = directory.path().join("zpool");
    let config = directory.path().join("zsnap.toml");

    write_executable(
        &zfs,
        r#"#!/bin/sh
case "$1" in
  list)
    case "$*" in
      *filesystem,volume*) printf 'tank\ntank/data\n' ;;
      *snapshot*) : ;;
    esac
    ;;
  snapshot)
    echo 'deliberate snapshot failure' >&2
    exit 1
    ;;
esac
"#,
    );
    write_executable(&zpool, "#!/bin/sh\nprintf 'tank\\t50%%\\n'\n");
    fs::write(
        &config,
        format!(
            r#"version = 1

[settings]
zfs_command = "{}"
zpool_command = "{}"
lock_file = "{}"

[notifications]
max_attempts = 1
timeout_seconds = 1

[[notifications.webhooks]]
name = "failure-receiver"
kind = "flock"
url = "https://127.0.0.1:1/hook"

[templates.test]
frequently = 0
hourly = 1
daily = 0
weekly = 0
monthly = 0
yearly = 0

[datasets."tank/data"]
use_templates = ["test"]
"#,
            zfs.display(),
            zpool.display(),
            directory.path().join("zsnap.lock").display(),
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_zsnap"))
        .args(["--config", config.to_str().unwrap(), "run"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("operation(s) failed"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("NOTIFICATION ERROR [flock:failure-receiver")
    );
}

#[test]
fn migrates_sanoid_to_a_new_valid_file_without_loading_zfs_config() {
    let directory = tempdir().unwrap();
    let input = directory.path().join("sanoid.conf");
    let defaults = directory.path().join("sanoid.defaults.conf");
    let output = directory.path().join("zsnap.toml");
    let source = r#"
[tank/data]
use_template = production
recursive = yes

[template_production]
autosnap = yes
autoprune = yes
hourly = 24
daily = 7
weekly = 4
monthly = 3
yearly = 0
"#;
    fs::write(&input, source).unwrap();
    fs::write(
        &defaults,
        "[version]\nversion = 2\n[template_default]\nhourly = 48\ndaily = 90\n",
    )
    .unwrap();

    let migration = Command::new(env!("CARGO_BIN_EXE_zsnap"))
        .args([
            "--config",
            directory
                .path()
                .join("does-not-exist.toml")
                .to_str()
                .unwrap(),
            "migrate-sanoid",
            "--input",
            input.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        migration.status.success(),
        "{}",
        String::from_utf8_lossy(&migration.stderr)
    );
    assert_eq!(fs::read_to_string(&input).unwrap(), source);
    assert_eq!(
        fs::metadata(&output).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let generated = fs::read_to_string(&output).unwrap();
    assert!(generated.contains("Existing unmarked Sanoid snapshots are never pruned"));
    assert!(generated.contains("timezone = \"local\""));
    assert!(generated.contains("[tank/data]"));
    assert!(generated.contains("use_templates = [sanoid_defaults, production]"));
    assert!(!generated.contains("[datasets.\"tank/data\"]"));

    let check = Command::new(env!("CARGO_BIN_EXE_zsnap"))
        .args(["--config", output.to_str().unwrap(), "check"])
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );

    let original_output = fs::read(&output).unwrap();
    let overwrite = Command::new(env!("CARGO_BIN_EXE_zsnap"))
        .args([
            "migrate-sanoid",
            "--input",
            input.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!overwrite.status.success());
    assert!(String::from_utf8_lossy(&overwrite.stderr).contains("refusing to overwrite"));
    assert_eq!(fs::read(&output).unwrap(), original_output);
}

#[test]
fn invalid_config_never_reaches_zfs_for_snapshot_prune_or_run() {
    let directory = tempdir().unwrap();
    let zfs = directory.path().join("zfs");
    let zpool = directory.path().join("zpool");
    let calls = directory.path().join("unexpected-zfs-call");
    let config = directory.path().join("invalid.toml");
    let marker_script = format!("#!/bin/sh\nprintf called > '{}'\n", calls.display());
    write_executable(&zfs, &marker_script);
    write_executable(&zpool, &marker_script);
    fs::write(
        &config,
        format!(
            r#"version = 1
[settings]
snapshot_batch_size = 0
zfs_command = "{}"
zpool_command = "{}"
[datasets."tank/data"]
"#,
            zfs.display(),
            zpool.display()
        ),
    )
    .unwrap();

    for command in ["snapshot", "prune", "run"] {
        let output = Command::new(env!("CARGO_BIN_EXE_zsnap"))
            .args(["--config", config.to_str().unwrap(), command, "--dry-run"])
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "invalid {command} unexpectedly passed"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("snapshot_batch_size"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let check = Command::new(env!("CARGO_BIN_EXE_zsnap"))
        .args(["--config", config.to_str().unwrap(), "check"])
        .output()
        .unwrap();
    assert!(!check.status.success());
    assert!(
        !calls.exists(),
        "invalid configuration invoked a ZFS command"
    );
}

fn write_executable(path: &std::path::Path, contents: &str) {
    fs::write(path, contents).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

fn calls_for(path: &std::path::Path, command: &str) -> usize {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|line| line.starts_with(command))
        .count()
}
