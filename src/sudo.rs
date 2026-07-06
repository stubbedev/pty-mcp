//! `sudo_run`: execute a command under sudo without the password ever touching
//! the model. Password entry is delegated to the OS via `SUDO_ASKPASS` (see
//! [`crate::askpass`]). The command is run directly (no shell), so there is no
//! shell-injection surface.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};

use crate::askpass;

/// Cap each captured stream so a runaway command can't blow up memory or the
/// model's context.
pub const MAX_STREAM: usize = 256 * 1024;

pub struct SudoOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

pub async fn run(argv: &[String], cwd: Option<&str>, timeout: Duration) -> Result<SudoOutput> {
    if argv.is_empty() {
        return Err(anyhow!("argv must not be empty"));
    }
    let script = askpass::ensure_script()?;

    let mut cmd = tokio::process::Command::new("sudo");
    cmd.arg("-A") // askpass
        .arg("--")
        .args(argv)
        .env("SUDO_ASKPASS", &script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    let child = cmd.spawn().context("spawn sudo")?;

    let wait = child.wait_with_output();
    match tokio::time::timeout(timeout, wait).await {
        Ok(res) => {
            let out = res.context("wait for sudo")?;
            Ok(SudoOutput {
                exit_code: out.status.code(),
                stdout: cap(&out.stdout),
                stderr: cap(&out.stderr),
                timed_out: false,
            })
        }
        Err(_) => Ok(SudoOutput {
            exit_code: None,
            stdout: String::new(),
            stderr: format!("timed out after {}s", timeout.as_secs()),
            timed_out: true,
        }),
    }
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
    async fn empty_argv_rejected() {
        assert!(run(&[], None, Duration::from_secs(1)).await.is_err());
    }
}
