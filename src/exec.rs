//! `run`: execute a shell command as the user would in their own terminal.
//!
//! The command runs via `$SHELL -c` with the user's full captured environment
//! (so `PATH` matches their interactive shell — nix, cargo, … — even over HTTP
//! or a proxy) and the harness's cwd as the default. `sudo` anywhere in the
//! command transparently uses the OS password dialog via the PATH-injected
//! wrapper (see [`crate::askpass`]); the password never reaches the model.

use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::askpass;
use crate::userenv;

/// Cap each captured stream so a runaway command can't blow up memory or the
/// model's context.
pub const MAX_STREAM: usize = 256 * 1024;

pub struct ExecOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    /// Whether the command invoked sudo (used to arm the keepalive).
    pub used_sudo: bool,
}

pub async fn run(
    command: &str,
    cwd: Option<&str>,
    timeout: Duration,
    askpass_cmd: Option<&str>,
) -> Result<ExecOutput> {
    let shell = userenv::shell();
    let base_path = userenv::user_env().get("PATH").cloned().unwrap_or_default();

    let mut cmd = tokio::process::Command::new(&shell);
    cmd.arg("-c").arg(command);
    // Full user environment first, then the sudo-helper overrides (PATH prefix
    // + SUDO_ASKPASS) so they win.
    cmd.env_clear();
    cmd.envs(userenv::user_env());
    for (k, v) in askpass::apply_to_env(&base_path, askpass_cmd) {
        cmd.env(k, v);
    }
    cmd.current_dir(cwd.map(str::to_string).unwrap_or_else(userenv::harness_cwd));
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Own process group, so a timeout can kill the whole tree — the shell often
    // has children (pipelines, build tools) that killing the shell alone would
    // orphan and leave running forever.
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            libc::setpgid(0, 0);
            Ok(())
        });
    }

    let mut child = cmd.spawn().context("spawn shell")?;
    let pid = child.id();
    let used_sudo = mentions_sudo(command);

    // Stream both pipes concurrently, keeping at most MAX_STREAM and draining
    // the rest, so a huge-output command can neither blow up memory (the old
    // wait_with_output buffered everything) nor deadlock on a full pipe.
    let out_task = tokio::spawn(read_capped(child.stdout.take().context("take stdout")?));
    let err_task = tokio::spawn(read_capped(child.stderr.take().context("take stderr")?));

    let (status, timed_out) = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(res) => (res.context("wait for command")?, false),
        Err(_) => {
            // Kill the process group; the readers then hit EOF and finish, so
            // whatever the command printed before the deadline is still returned.
            #[cfg(unix)]
            if let Some(pid) = pid {
                unsafe { libc::killpg(pid as i32, libc::SIGKILL) };
            }
            let status = child.wait().await.context("reap after kill")?;
            (status, true)
        }
    };

    let stdout = cap(&out_task.await.unwrap_or_default());
    let mut stderr = cap(&err_task.await.unwrap_or_default());
    if timed_out {
        if !stderr.is_empty() {
            stderr.push('\n');
        }
        stderr.push_str(&format!(
            "[timed out after {}s — process group killed]",
            timeout.as_secs()
        ));
    }
    Ok(ExecOutput {
        exit_code: if timed_out { None } else { exit_code(&status) },
        stdout,
        stderr,
        timed_out,
        used_sudo,
    })
}

/// Does the command line invoke sudo? Split on whitespace AND shell operators,
/// so `foo|sudo tee x` or `a;sudo b` count too. False positives (e.g. `echo
/// sudo`) are harmless — the keepalive refresher just finds no timestamp and
/// stops.
fn mentions_sudo(command: &str) -> bool {
    command
        .split(|c: char| c.is_whitespace() || "|;&()<>".contains(c))
        .any(|t| t == "sudo")
}

/// Exit code, mapping death-by-signal to the shell convention `128 + signal`.
fn exit_code(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.code().or_else(|| status.signal().map(|s| 128 + s))
}

/// Read a pipe to EOF, keeping only the first ~[`MAX_STREAM`] bytes and
/// discarding the rest (still reading, so the child never blocks on a full pipe).
async fn read_capped(mut r: impl AsyncRead + Unpin) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => return kept,
            // Allow a little past the cap so `cap()` sees the overflow and
            // appends its truncation marker.
            Ok(n) if kept.len() <= MAX_STREAM => kept.extend_from_slice(&buf[..n]),
            Ok(_) => {}
        }
    }
}

/// Keep sudo's credential timestamp warm for the lifetime of this process, so a
/// single password entry covers the whole session. Started once, after the
/// first successful sudo auth; refreshes with `sudo -n -v` on an interval
/// shorter than any reasonable `timestamp_timeout`. If the timestamp is ever
/// lost it stops and re-arms on the next sudo command.
pub fn spawn_keepalive(started: std::sync::Arc<AtomicBool>) {
    if started.swap(true, Ordering::SeqCst) {
        return;
    }
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        tick.tick().await; // fires immediately; auth is already fresh
        loop {
            tick.tick().await;
            let ok = tokio::process::Command::new("sudo")
                .arg("-n")
                .arg("-v")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;
            if !matches!(ok, Ok(s) if s.success()) {
                tracing::info!("sudo keepalive: timestamp lost; stopping refresher");
                started.store(false, Ordering::SeqCst);
                break;
            }
        }
    });
}

/// UTF-8-lossy, truncated to [`MAX_STREAM`] bytes on a char boundary.
fn cap(bytes: &[u8]) -> String {
    if bytes.len() <= MAX_STREAM {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let mut end = MAX_STREAM;
    while end > 0 && (bytes[end] & 0xC0) == 0x80 {
        end -= 1;
    }
    let mut s = String::from_utf8_lossy(&bytes[..end]).into_owned();
    s.push_str("\n…[truncated]");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_under_limit_passthrough() {
        assert_eq!(cap(b"hello"), "hello");
    }

    #[test]
    fn sudo_detected_through_shell_operators() {
        assert!(mentions_sudo("sudo apt update"));
        assert!(mentions_sudo("echo hi|sudo tee /etc/x"));
        assert!(mentions_sudo("a && sudo b"));
        assert!(mentions_sudo("true;sudo reboot"));
        assert!(!mentions_sudo("echo sudoku"));
        assert!(!mentions_sudo("visudo"));
    }

    #[test]
    fn cap_truncates_large() {
        let big = vec![b'a'; MAX_STREAM + 100];
        let out = cap(&big);
        assert!(out.len() <= MAX_STREAM + 20);
        assert!(out.ends_with("[truncated]"));
    }

    #[tokio::test]
    async fn runs_in_user_env_with_path() {
        // A plain command resolves via the user's PATH and returns stdout.
        let out = run("printf hello", None, Duration::from_secs(10), None)
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(0));
        assert_eq!(out.stdout, "hello");
        assert!(!out.used_sudo);
    }

    #[tokio::test]
    async fn shell_features_work() {
        let out = run(
            "echo a && echo b | tr a-z A-Z",
            None,
            Duration::from_secs(10),
            None,
        )
        .await
        .unwrap();
        assert!(
            out.stdout.contains('B'),
            "pipe/&& should work: {:?}",
            out.stdout
        );
    }

    #[tokio::test]
    async fn nonzero_exit_reported() {
        let out = run("exit 3", None, Duration::from_secs(10), None)
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(3));
    }

    #[tokio::test]
    async fn timeout_kills_tree_and_returns_partial_output() {
        let t0 = std::time::Instant::now();
        // `exec` replaces the shell, so this also proves group-kill reaches the
        // real process, and the early echo proves partial output survives.
        let out = run(
            "echo started; exec sleep 30",
            None,
            Duration::from_secs(1),
            None,
        )
        .await
        .unwrap();
        assert!(out.timed_out);
        assert!(out.stdout.contains("started"), "partial stdout kept");
        assert!(out.stderr.contains("timed out"), "stderr: {:?}", out.stderr);
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "killed promptly, not after 30s"
        );
    }

    #[tokio::test]
    async fn huge_output_capped_without_deadlock() {
        // Way past MAX_STREAM: must truncate, not OOM, and not deadlock on a
        // full pipe once the cap is hit.
        let out = run(
            "yes abcdefgh | head -c 2000000",
            None,
            Duration::from_secs(30),
            None,
        )
        .await
        .unwrap();
        assert!(!out.timed_out);
        assert!(out.stdout.ends_with("[truncated]"), "truncation marker");
        assert!(out.stdout.len() <= MAX_STREAM + 8192 + 20);
    }

    #[tokio::test]
    async fn signal_death_reported_as_128_plus() {
        let out = run("kill -TERM $$", None, Duration::from_secs(10), None)
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(128 + 15));
    }

    #[tokio::test]
    async fn cwd_override() {
        // Use the OS temp dir and compare canonical paths — macOS resolves
        // /tmp to /private/tmp, so a literal string compare is wrong there.
        let dir = std::env::temp_dir();
        let out = run("pwd", dir.to_str(), Duration::from_secs(10), None)
            .await
            .unwrap();
        let got = std::fs::canonicalize(out.stdout.trim()).unwrap();
        let want = std::fs::canonicalize(&dir).unwrap();
        assert_eq!(got, want);
    }
}
