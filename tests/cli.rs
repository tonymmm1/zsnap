use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::thread;
use std::time::Duration;

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
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let webhook_address = listener.local_addr().unwrap();
    let webhook_server = thread::spawn(move || receive_webhook(listener));

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

[notifications]
allow_insecure_http = true
max_attempts = 1

[[notifications.webhooks]]
name = "success-receiver"
kind = "slack"
url = "http://{webhook_address}/hook"
events = ["success"]

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
    let webhook_request = webhook_server.join().unwrap();
    assert!(webhook_request.contains("zsnap run SUCCESS"));
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
fn notify_test_posts_a_provider_specific_payload() {
    let directory = tempdir().unwrap();
    let config = directory.path().join("zsnap.toml");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || receive_webhook(listener));
    fs::write(
        &config,
        format!(
            r#"version = 1

[notifications]
allow_insecure_http = true
max_attempts = 1

[[notifications.webhooks]]
name = "local-discord"
kind = "discord"
url = "http://{address}/webhook"

[datasets."tank/data"]
"#
        ),
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
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["attempted"], 1);
    assert_eq!(report["delivered"], 1);

    let request = server.join().unwrap();
    let (_, body) = request.split_once("\r\n\r\n").unwrap();
    let payload: Value = serde_json::from_str(body).unwrap();
    assert!(
        payload["content"]
            .as_str()
            .unwrap()
            .contains("integration check")
    );
    assert_eq!(payload["allowed_mentions"]["parse"], serde_json::json!([]));
}

#[test]
fn failed_zfs_run_sends_a_failure_notification_and_stays_failed() {
    let directory = tempdir().unwrap();
    let zfs = directory.path().join("zfs");
    let zpool = directory.path().join("zpool");
    let config = directory.path().join("zsnap.toml");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || receive_webhook(listener));

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
allow_insecure_http = true
max_attempts = 1

[[notifications.webhooks]]
name = "failure-receiver"
kind = "flock"
url = "http://{address}/hook"

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
    let request = server.join().unwrap();
    assert!(request.contains("zsnap run FAILURE"));
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

fn receive_webhook(listener: TcpListener) -> String {
    let (mut stream, _) = listener.accept().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut request = Vec::new();
    let mut buffer = [0_u8; 2_048];
    loop {
        let read = stream.read(&mut buffer).unwrap();
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
        if let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.strip_prefix("content-length: ")
                        .or_else(|| line.strip_prefix("Content-Length: "))
                })
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(0);
            if request.len() >= header_end + 4 + content_length {
                break;
            }
        }
    }
    stream
        .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .unwrap();
    String::from_utf8(request).unwrap()
}
