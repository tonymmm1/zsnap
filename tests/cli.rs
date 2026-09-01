use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use serde_json::Value;
use tempfile::tempdir;

#[test]
fn plans_and_executes_against_fake_zfs_tools() {
    let directory = tempdir().unwrap();
    let zfs = directory.path().join("zfs");
    let zpool = directory.path().join("zpool");
    let calls = directory.path().join("calls.log");
    let lock = directory.path().join("zsnap.lock");
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
max_parallel_pools = 0

[templates.test]
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
            lock.display()
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
}

#[test]
fn distributed_example_configuration_is_valid() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let output = Command::new(env!("CARGO_BIN_EXE_zsnap"))
        .args([
            "--config",
            root.join("config.example.toml").to_str().unwrap(),
            "check",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
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
