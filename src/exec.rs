//! `run`: execute a shell command as the user would in their own terminal.
//!
//! The command runs via `$SHELL -c` with the user's full captured environment
//! (so `PATH` matches their interactive shell — nix, cargo, … — even over HTTP
//! or a proxy) and their home as the default cwd. `sudo` anywhere in the
//! command transparently uses the OS password dialog via the PATH-injected
//! wrapper (see [`crate::askpass`]); the password never reaches the model.

use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};

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
    cmd.current_dir(cwd.map(str::to_string).unwrap_or_else(userenv::home));
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = cmd.spawn().context("spawn shell")?;
    let used_sudo = command.split_whitespace().any(|t| t == "sudo");

    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(res) => {
            let out = res.context("wait for command")?;
            Ok(ExecOutput {
                exit_code: out.status.code(),
                stdout: cap(&out.stdout),
                stderr: cap(&out.stderr),
                timed_out: false,
                used_sudo,
            })
        }
        Err(_) => Ok(ExecOutput {
            exit_code: None,
            stdout: String::new(),
            stderr: format!("timed out after {}s", timeout.as_secs()),
            timed_out: true,
            used_sudo,
        }),
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
