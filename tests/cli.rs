use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::tempdir;
use zsnap::config::Config;
use zsnap::lock::RunLock;

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
        printf 'tank\t-\ntank/data\t-\nbackup\t-\nbackup/data\t-\n'
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
  *filesystem,volume*) printf 'tank\t-\ntank/data\t-\ntank/data/vm\t-\n' ;;
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

    // A refresh only reads ZFS, so it must not contend with the run lock.
    let run_lock = RunLock::acquire(&lock).unwrap();
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
    drop(run_lock);
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
      *filesystem,volume*) printf 'tank\t-\ntank/data\t-\n' ;;
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

/// Writes an executable script through a child process, so this test process never
/// holds a writable descriptor that a concurrently forking test could inherit (which
/// makes executing the script fail with ETXTBSY).
fn write_executable(path: &std::path::Path, contents: &str) {
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

/// A config whose fake `zfs` lists `tank` and `tank/data` and records mutations.
fn fake_host(directory: &std::path::Path, zfs_script: &str, extra: &str) -> std::path::PathBuf {
    let zfs = directory.join("zfs");
    let zpool = directory.join("zpool");
    let config = directory.join("zsnap.toml");
    write_executable(&zfs, zfs_script);
    write_executable(&zpool, "#!/bin/sh\nprintf 'tank\\t50\\n'\n");
    fs::write(
        &config,
        format!(
            "version = 1\n[settings]\nzfs_command = \"{}\"\nzpool_command = \"{}\"\nlock_file = \"{}\"\ncache_file = \"{}\"\n{extra}",
            zfs.display(),
            zpool.display(),
            directory.join("zsnap.lock").display(),
            directory.join("zsnap.cache").display(),
        ),
    )
    .unwrap();
    config
}

const LISTING_ZFS: &str = r#"#!/bin/sh
case "$1" in
  list) case "$*" in *filesystem,volume*) printf 'tank\t-\ntank/data\t-\n' ;; esac ;;
esac
"#;

#[test]
fn nested_sections_are_not_rescanned_and_missing_ones_are_reported() {
    let directory = tempdir().unwrap();
    let calls = directory.path().join("list-args.log");
    let zfs = format!(
        r#"#!/bin/sh
case "$1" in
  list)
    printf '%s\n' "$*" >> '{}'
    case "$*" in *filesystem,volume*) printf 'tank\t-\ntank/data\t-\ntank/data/db\t-\n' ;; esac ;;
esac
"#,
        calls.display()
    );
    let config = fake_host(
        directory.path(),
        &zfs,
        "[tank]\nrecursive = true\n[tank/data/db]\nhourly = 1\n",
    );

    let plan = Command::new(env!("CARGO_BIN_EXE_zsnap"))
        .args(["--config", config.to_str().unwrap(), "plan"])
        .output()
        .unwrap();
    assert!(
        plan.status.success(),
        "{}",
        String::from_utf8_lossy(&plan.stderr)
    );
    let lists = fs::read_to_string(&calls).unwrap();
    assert_eq!(lists.lines().count(), 2, "{lists}");
    for line in lists.lines() {
        // Only the topmost root is listed recursively; tank/data/db is covered by it.
        assert!(line.ends_with(" tank"), "{line}");
    }

    let config = fake_host(directory.path(), &zfs, "[tank]\n[tank/missing]\n");
    let check = Command::new(env!("CARGO_BIN_EXE_zsnap"))
        .args(["--config", config.to_str().unwrap(), "check", "--probe"])
        .output()
        .unwrap();
    assert!(!check.status.success());
    let stderr = String::from_utf8_lossy(&check.stderr);
    assert!(
        stderr.contains("reference missing names:\n  - tank/missing\n"),
        "{stderr}"
    );
}

#[test]
fn deleted_datasets_are_skipped_with_a_warning() {
    let directory = tempdir().unwrap();
    // Like real ZFS: a missing root fails the whole listing, so zsnap must retry without it.
    let zfs = r#"#!/bin/sh
case "$*" in
  *tank/gone*) echo "cannot open 'tank/gone': dataset does not exist" >&2; exit 1 ;;
  list*filesystem,volume*) printf 'tank/data\t-\n' ;;
esac
"#;
    let config = fake_host(
        directory.path(),
        zfs,
        "[tank/data]\nhourly = 1\n[tank/gone]\nhourly = 1\n",
    );

    let plan = Command::new(env!("CARGO_BIN_EXE_zsnap"))
        .args(["--config", config.to_str().unwrap(), "--json", "plan"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&plan.stderr);
    assert!(plan.status.success(), "{stderr}");
    assert!(
        stderr.contains("WARNING: configured dataset \"tank/gone\" does not exist"),
        "{stderr}"
    );
    let plan: Value = serde_json::from_slice(&plan.stdout).unwrap();
    let datasets: Vec<_> = plan["snapshots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|action| action["dataset"].as_str().unwrap())
        .collect();
    assert_eq!(datasets, ["tank/data"]);

    // Only deleted datasets configured: nothing to do, and ZFS is never asked to list everything.
    let config = fake_host(directory.path(), zfs, "[tank/gone]\n");
    let plan = Command::new(env!("CARGO_BIN_EXE_zsnap"))
        .args(["--config", config.to_str().unwrap(), "plan"])
        .output()
        .unwrap();
    assert!(
        plan.status.success(),
        "{}",
        String::from_utf8_lossy(&plan.stderr)
    );

    let check = Command::new(env!("CARGO_BIN_EXE_zsnap"))
        .args(["--config", config.to_str().unwrap(), "check", "--probe"])
        .output()
        .unwrap();
    assert!(!check.status.success());
}

#[test]
fn notify_test_works_before_any_dataset_is_configured() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let output = Command::new(env!("CARGO_BIN_EXE_zsnap"))
        .args([
            "--config",
            root.join("contrib/zsnap.toml.example").to_str().unwrap(),
            "notify-test",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    // The starter config has no webhooks; it must not fail on the missing datasets.
    assert!(
        stderr.contains("no enabled notification webhooks are configured"),
        "{stderr}"
    );
}

#[test]
fn hook_output_does_not_corrupt_json_reports() {
    let directory = tempdir().unwrap();
    let hook = directory.path().join("hook");
    write_executable(&hook, "#!/bin/sh\necho 'database frozen'\n");
    let config = fake_host(
        directory.path(),
        LISTING_ZFS,
        &format!(
            "[tank/data]\nhourly = 1\ndaily = 0\nmonthly = 0\npre_snapshot_script = [\"{}\"]\n",
            hook.display()
        ),
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
    let report: Value = serde_json::from_slice(&run.stdout).expect("stdout is only JSON");
    assert_eq!(report["snapshots_created"], 1);
    assert!(String::from_utf8_lossy(&run.stderr).contains("database frozen"));
}

#[test]
fn a_hung_zfs_command_times_out_and_reports_the_failure() {
    let directory = tempdir().unwrap();
    let config = fake_host(
        directory.path(),
        "#!/bin/sh\nexec sleep 60\n",
        "zfs_timeout = 1\n[notifications]\nmax_attempts = 1\ntimeout_seconds = 1\n[[notifications.webhooks]]\nname = \"ops\"\nkind = \"slack\"\nurl = \"https://127.0.0.1:1/hook\"\n[tank/data]\n",
    );

    let started = Instant::now();
    let run = Command::new(env!("CARGO_BIN_EXE_zsnap"))
        .args(["--config", config.to_str().unwrap(), "run"])
        .output()
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(30));
    assert!(!run.status.success());
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(stderr.contains("timed out after 1s"), "{stderr}");
    // The run returned, so the failure webhook was attempted instead of hanging silently.
    assert!(stderr.contains("NOTIFICATION ERROR [slack:ops"), "{stderr}");
}

#[test]
fn refuses_to_run_when_the_ownership_marker_is_set_on_a_dataset() {
    let directory = tempdir().unwrap();
    let calls = directory.path().join("calls.log");
    // tank/data carries the marker itself, so its unmarked snapshots would inherit it.
    let zfs = format!(
        r#"#!/bin/sh
case "$1" in
  list)
    case "$*" in
      *filesystem,volume*) printf 'tank\t-\ntank/data\tyes\n' ;;
      *snapshot*) printf 'tank/data@autosnap_2020-01-01_00:00:00_hourly\t1577836800\tyes\t0\t-\n' ;;
    esac ;;
  snapshot|destroy) printf '%s\n' "$*" >> '{}' ;;
esac
"#,
        calls.display()
    );
    let config = fake_host(
        directory.path(),
        &zfs,
        "[tank/data]\nhourly = 0\ndaily = 0\nmonthly = 0\n",
    );

    let run = Command::new(env!("CARGO_BIN_EXE_zsnap"))
        .args(["--config", config.to_str().unwrap(), "run"])
        .output()
        .unwrap();
    assert!(!run.status.success());
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(stderr.contains("org.zsnap:managed is set on"), "{stderr}");
    assert!(stderr.contains("  - tank/data"), "{stderr}");
    assert!(!calls.exists(), "zsnap mutated ZFS despite the marker");
}

fn calls_for(path: &std::path::Path, command: &str) -> usize {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|line| line.starts_with(command))
        .count()
}
