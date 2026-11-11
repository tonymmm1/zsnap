use std::io::{self, Read};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// How long to wait for a killed child to disappear before abandoning it.
const KILL_GRACE: Duration = Duration::from_secs(2);

/// Waits for `child`. Once `timeout` elapses the child (and its process group, when
/// it was spawned as a group leader) is killed and `None` is returned.
pub(crate) fn wait_with_timeout(
    child: &mut Child,
    timeout: Option<Duration>,
) -> io::Result<Option<ExitStatus>> {
    let Some(timeout) = timeout else {
        return child.wait().map(Some);
    };
    let deadline = Instant::now() + timeout;
    let mut pause = Duration::from_millis(1);
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        // Back off so short commands add about a millisecond, not a fixed poll interval.
        thread::sleep(pause.min(remaining));
        pause = (pause * 2).min(Duration::from_millis(50));
    }

    kill_tree(child);
    // A process blocked inside the kernel (for example on a suspended pool) ignores
    // SIGKILL until its system call returns, so stop waiting after a short grace
    // period instead of hanging with it.
    let grace = Instant::now() + KILL_GRACE;
    while Instant::now() < grace && child.try_wait()?.is_none() {
        thread::sleep(Duration::from_millis(10));
    }
    Ok(None)
}

/// Runs `command` capturing stdout and stderr; returns `None` when it timed out.
pub(crate) fn output_with_timeout(
    command: &mut Command,
    timeout: Option<Duration>,
) -> io::Result<Option<Output>> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // Detached readers: after a timeout an unkillable child still holds the pipes
    // open, so these threads must not be joined in that case.
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let Some(status) = wait_with_timeout(&mut child, timeout)? else {
        return Ok(None);
    };
    Ok(Some(Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    }))
}

fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buffer = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buffer);
        }
        buffer
    })
}

fn kill_tree(child: &mut Child) {
    // Hooks are spawned as process-group leaders so that anything they started dies
    // with them. For a child that is not a group leader no such group exists and
    // this is a harmless ESRCH.
    // SAFETY: kill(2) has no memory-safety preconditions. The child has not been
    // reaped, so its PID (and therefore the group ID) cannot have been reused.
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    let _ = child.kill();
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::CommandExt;
    use std::path::Path;

    use super::*;

    #[test]
    fn captures_output_and_status_of_a_finished_command() {
        let output = output_with_timeout(
            Command::new("/bin/sh").args(["-c", "echo out; echo err >&2; exit 3"]),
            Some(Duration::from_secs(30)),
        )
        .unwrap()
        .expect("command finished");
        assert_eq!(output.status.code(), Some(3));
        assert_eq!(output.stdout, b"out\n");
        assert_eq!(output.stderr, b"err\n");
    }

    #[test]
    fn a_timeout_kills_the_whole_process_group() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("grandchild.pid");
        let started = Instant::now();
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(
                "sleep 60 & echo $! > '{}'; wait",
                pid_file.display()
            ))
            .process_group(0)
            .spawn()
            .unwrap();
        let status = wait_with_timeout(&mut child, Some(Duration::from_millis(300))).unwrap();
        assert!(status.is_none(), "expected a timeout");
        assert!(started.elapsed() < Duration::from_secs(10));

        let grandchild = std::fs::read_to_string(&pid_file).unwrap();
        let process = format!("/proc/{}", grandchild.trim());
        let gone = (0..200).any(|_| {
            thread::sleep(Duration::from_millis(10));
            !Path::new(&process).exists()
        });
        assert!(gone, "grandchild {process} survived the timeout");
    }
}
