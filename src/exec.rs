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

/// Returned-output budget per stream: the first `HEAD_KEEP` bytes plus the last
/// `TAIL_KEEP`, with the middle elided. Build/test failures print the error at
/// the END — a head-only cap (what harness bash tools do) loses exactly the
/// part that matters. Total ≈ 32 KB per stream keeps the model's context sane.
pub const HEAD_KEEP: usize = 4 * 1024;
pub const TAIL_KEEP: usize = 28 * 1024;

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

    // New session: (a) own process group, so a timeout can kill the whole tree
    // — the shell often has children (pipelines, build tools) that killing the
    // shell alone would orphan; (b) no controlling terminal, so programs that
    // prompt via /dev/tty (git credentials, ssh passwords) fail immediately
    // with a readable error instead of hanging until the timeout — or worse,
    // writing their prompt into the user's terminal under the harness TUI.
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
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

    let stdout = out_task.await.map(|c| c.render()).unwrap_or_default();
    let mut stderr = err_task.await.map(|c| c.render()).unwrap_or_default();
    if timed_out {
        if !stderr.is_empty() {
            stderr.push('\n');
        }
        stderr.push_str(&format!(
            "[timed out after {}s — process group killed. If the command was \
             waiting for input, re-run it via pty_open + pty_write so you can \
             answer its prompt; otherwise retry with a larger timeout_seconds.]",
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

/// Head + tail of a stream, with the total byte count. Memory stays bounded at
/// `HEAD_KEEP + ~TAIL_KEEP` no matter how much the command prints.
struct Capped {
    /// First HEAD_KEEP bytes.
    head: Vec<u8>,
    /// Rolling window of the bytes after the head (kept ≤ TAIL_KEEP).
    tail: Vec<u8>,
    total: usize,
}

impl Capped {
    fn render(&self) -> String {
        let elided = self.total - self.head.len() - self.tail.len();
        if elided == 0 {
            // head and tail are disjoint and adjacent — this is everything.
            let mut all = self.head.clone();
            all.extend_from_slice(&self.tail);
            return String::from_utf8_lossy(&all).into_owned();
        }
        // Lossy conversion puts replacement chars at the ragged cut points;
        // trim them so the marker line stays clean.
        format!(
            "{}\n…[{elided} bytes truncated]…\n{}",
            String::from_utf8_lossy(&self.head).trim_end_matches('\u{FFFD}'),
            String::from_utf8_lossy(&self.tail).trim_start_matches('\u{FFFD}'),
        )
    }
}

/// Read a pipe to EOF, keeping the first [`HEAD_KEEP`] bytes and a rolling
/// window of the last [`TAIL_KEEP`] — always draining, so the child never
/// blocks on a full pipe and a failing build's final error lines survive.
async fn read_capped(mut r: impl AsyncRead + Unpin) -> Capped {
    let mut c = Capped {
        head: Vec::new(),
        tail: Vec::new(),
        total: 0,
    };
    let mut buf = [0u8; 8192];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => return c,
            Ok(n) => {
                c.total += n;
                let mut chunk = &buf[..n];
                if c.head.len() < HEAD_KEEP {
                    let take = (HEAD_KEEP - c.head.len()).min(chunk.len());
                    c.head.extend_from_slice(&chunk[..take]);
                    chunk = &chunk[take..];
                }
                c.tail.extend_from_slice(chunk);
                if c.tail.len() > TAIL_KEEP {
                    let excess = c.tail.len() - TAIL_KEEP;
                    c.tail.drain(..excess);
                }
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn capped(head: &[u8], tail: &[u8], total: usize) -> Capped {
        Capped {
            head: head.to_vec(),
            tail: tail.to_vec(),
            total,
        }
    }

    #[test]
    fn capped_render_passthrough() {
        assert_eq!(capped(b"hel", b"lo", 5).render(), "hello");
        assert_eq!(capped(b"hello", b"", 5).render(), "hello");
    }

    #[test]
    fn capped_render_elides_middle() {
        let out = capped(b"start", b"end", 100).render();
        assert!(out.starts_with("start\n"));
        assert!(out.ends_with("\nend"));
        assert!(out.contains("[92 bytes truncated]"));
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

    #[tokio::test]
    async fn read_capped_keeps_head_and_tail() {
        // 100 KB through an in-memory reader: head intact, tail is the LAST
        // bytes (where a build failure's error lives), middle elided.
        let mut data = b"FIRST-LINE\n".to_vec();
        data.extend(vec![b'x'; 100 * 1024]);
        data.extend_from_slice(b"\nLAST-ERROR-LINE");
        let c = read_capped(&data[..]).await;
        assert_eq!(c.total, data.len());
        let out = c.render();
        assert!(out.starts_with("FIRST-LINE"), "head preserved");
        assert!(out.ends_with("LAST-ERROR-LINE"), "tail preserved");
        assert!(out.contains("bytes truncated"));
        assert!(out.len() <= HEAD_KEEP + TAIL_KEEP + 64);
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
        // 2 MB of output: must truncate the middle (not OOM, not deadlock on a
        // full pipe) while the final line — where errors live — survives.
        let out = run(
            "yes abcdefgh | head -c 2000000; echo FINAL-LINE",
            None,
            Duration::from_secs(30),
            None,
        )
        .await
        .unwrap();
        assert!(!out.timed_out);
        assert!(out.stdout.contains("bytes truncated"), "middle elided");
        assert!(out.stdout.trim_end().ends_with("FINAL-LINE"), "tail kept");
        assert!(out.stdout.len() <= HEAD_KEEP + TAIL_KEEP + 64);
    }

    #[tokio::test]
    async fn tty_prompts_fail_fast_instead_of_hanging() {
        // setsid detaches the controlling terminal, so a /dev/tty prompt (git
        // credentials, ssh password) errors immediately — it must not sit
        // there until the timeout.
        let t0 = std::time::Instant::now();
        let out = run(
            "read -r x < /dev/tty && echo got:$x",
            None,
            Duration::from_secs(10),
            None,
        )
        .await
        .unwrap();
        assert!(!out.timed_out, "must fail fast, not hang: {:?}", out.stderr);
        assert_ne!(out.exit_code, Some(0));
        assert!(t0.elapsed() < Duration::from_secs(5));
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
