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

[datasets."tank/data"]
use_templates = ["test"]

[datasets."backup/data"]
use_templates = ["test"]
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
