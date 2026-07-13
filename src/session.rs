//! PTY session lifecycle: spawn, background reader → emulator, wait/notify,
//! idle reaping.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::Notify;

use crate::screen::Emulator;
use crate::{askpass, userenv};

/// Scrollback window `pty_wait`/`wait_for` matches against, so output that
/// scrolled off the visible screen between wakeups is still seen.
const WAIT_WINDOW: usize = 400;

/// Shared, thread-crossing state: the reader thread writes it, async tool
/// handlers read it.
struct Shared {
    emu: Mutex<Emulator>,
    /// Woken on every chunk of new output and on child exit.
    notify: Notify,
    last_activity: Mutex<Instant>,
    /// `Some(code)` once the child has exited (EOF on the PTY).
    exited: Mutex<Option<i32>>,
    /// Attached human viewers (`pty-mcp attach`): raw PTY output is teed to
    /// each. Slow/broken taps are dropped, never allowed to stall the reader.
    taps: Mutex<Vec<std::os::unix::net::UnixStream>>,
}

pub struct Session {
    pub id: String,
    pub shell: String,
    pub cwd: String,
    pub created: Instant,
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    shared: Arc<Shared>,
    /// Hash of the screen last returned by `pty_read`, so an unchanged screen
    /// can be reported as such instead of re-sent (polling TUIs bloats context).
    last_read: Mutex<Option<u64>>,
}

pub struct OpenParams {
    pub shell: Option<String>,
    pub cwd: Option<String>,
    pub cols: u16,
    pub rows: u16,
    pub env: Vec<(String, String)>,
    pub scrollback: usize,
}

/// Snapshot of a session's rendered state for `pty_read`.
pub struct Snapshot {
    pub screen: String,
    pub cursor_row: usize,
    pub cursor_col: usize,
    pub alt_screen: bool,
    pub exited: Option<i32>,
}

impl Session {
    fn spawn(id: String, p: &OpenParams, askpass_cmd: Option<&str>) -> Result<Arc<Session>> {
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: p.rows,
                cols: p.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty")?;

        let shell = p.shell.clone().unwrap_or_else(userenv::shell);
        let mut cmd = CommandBuilder::new(&shell);

        // Run in the user's real environment (PATH as their interactive shell
        // has it), then layer on the sudo helpers so `sudo` inside the session
        // pops the OS dialog, then TERM, then caller overrides.
        cmd.env_clear();
        let base_path = userenv::user_env().get("PATH").cloned().unwrap_or_default();
        for (k, v) in userenv::user_env() {
            cmd.env(k, v);
        }
        for (k, v) in askpass::apply_to_env(&base_path, askpass_cmd) {
            cmd.env(k, v);
        }
        cmd.env("TERM", "xterm-256color");
        for (k, v) in &p.env {
            cmd.env(k, v);
        }

        let cwd = p.cwd.clone().unwrap_or_else(userenv::harness_cwd);
        cmd.cwd(&cwd);

        let child = pair
            .slave
            .spawn_command(cmd)
            .context("spawn shell in pty")?;
        // Drop the slave so the PTY reports EOF once the child exits.
        drop(pair.slave);

        let writer = pair.master.take_writer().context("take pty writer")?;
        let reader = pair.master.try_clone_reader().context("clone pty reader")?;
        let killer = child.clone_killer();

        let shared = Arc::new(Shared {
            emu: Mutex::new(Emulator::new(p.cols, p.rows, p.scrollback)),
            notify: Notify::new(),
            last_activity: Mutex::new(Instant::now()),
            exited: Mutex::new(None),
            taps: Mutex::new(Vec::new()),
        });

        spawn_reader(reader, Arc::clone(&shared));
        spawn_waiter(child, Arc::clone(&shared));

        Ok(Arc::new(Session {
            id,
            shell,
            cwd,
            created: Instant::now(),
            writer: Mutex::new(writer),
            master: Mutex::new(pair.master),
            killer: Mutex::new(killer),
            shared,
            last_read: Mutex::new(None),
        }))
    }

    pub fn write(&self, bytes: &[u8]) -> Result<()> {
        let mut w = self.writer.lock().unwrap();
        w.write_all(bytes).context("write to pty")?;
        w.flush().ok();
        *self.shared.last_activity.lock().unwrap() = Instant::now();
        Ok(())
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.master
            .lock()
            .unwrap()
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("resize pty")?;
        self.shared.emu.lock().unwrap().resize(cols, rows);
        Ok(())
    }

    pub fn snapshot(&self, scrollback: usize) -> Snapshot {
        let emu = self.shared.emu.lock().unwrap();
        let (row, col) = emu.cursor();
        Snapshot {
            screen: emu.render(scrollback),
            cursor_row: row,
            cursor_col: col,
            alt_screen: emu.alt_screen(),
            exited: *self.shared.exited.lock().unwrap(),
        }
    }

    /// Attach a human viewer: send a redraw of the current screen, then
    /// register the stream so all future PTY output is teed to it. The redraw
    /// happens under the taps lock so no output chunk can interleave with it.
    pub fn attach_tap(&self, mut stream: std::os::unix::net::UnixStream) {
        use std::io::Write as _;
        let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
        let redraw = {
            let emu = self.shared.emu.lock().unwrap();
            let (row, col) = emu.cursor();
            format!(
                "\x1b[2J\x1b[H{}\x1b[{};{}H",
                emu.render(0).replace('\n', "\r\n"),
                row + 1,
                col + 1
            )
        };
        let mut taps = self.shared.taps.lock().unwrap();
        if stream.write_all(redraw.as_bytes()).is_ok() {
            taps.push(stream);
        }
    }

    /// Record the screen hash `pty_read` is about to return; true if it's the
    /// same screen as the previous `pty_read` (nothing changed since).
    pub fn same_as_last_read(&self, screen: &str) -> bool {
        use std::hash::{Hash, Hasher};
        let mut h = std::hash::DefaultHasher::new();
        screen.hash(&mut h);
        let hash = h.finish();
        self.last_read.lock().unwrap().replace(hash) == Some(hash)
    }

    pub fn idle(&self) -> Duration {
        self.shared.last_activity.lock().unwrap().elapsed()
    }

    pub fn is_exited(&self) -> bool {
        self.shared.exited.lock().unwrap().is_some()
    }

    /// Wait until `pattern` matches the visible screen, or (if `pattern` is
    /// `None`) until output has been quiet for `quiet`, or `timeout` elapses.
    /// Returns whether the condition (not the timeout) was satisfied.
    pub async fn wait(
        &self,
        pattern: Option<&regex::Regex>,
        quiet: Duration,
        timeout: Duration,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            // Register for notification *before* checking, so we can't miss a
            // wakeup that lands between the check and the await.
            let notified = self.shared.notify.notified();

            if let Some(re) = pattern {
                if re.is_match(&self.shared.emu.lock().unwrap().render(WAIT_WINDOW)) {
                    return true;
                }
            } else if self.idle() >= quiet {
                return true;
            }
            if self.is_exited() {
                return true;
            }

            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            // For quiet-mode, don't sleep past the point the screen would go
            // quiet; wake then to re-check.
            let step = if pattern.is_none() {
                quiet.saturating_sub(self.idle()).min(deadline - now)
            } else {
                deadline - now
            };
            let _ = tokio::time::timeout(step.max(Duration::from_millis(5)), notified).await;
        }
    }

    fn kill(&self) {
        if let Ok(mut k) = self.killer.lock() {
            let _ = k.kill();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Blocking reader thread: PTY output → VT parser → grid. One std thread per
/// session; parks in `read` until bytes arrive, so idle sessions cost nothing.
fn spawn_reader(mut reader: Box<dyn Read + Send>, shared: Arc<Shared>) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => {
                    // EOF: the child is gone. The waiter thread records the real
                    // exit code; here we just wake anyone blocked on output.
                    shared.notify.notify_waiters();
                    break;
                }
                Ok(n) => {
                    shared.emu.lock().unwrap().advance(&buf[..n]);
                    *shared.last_activity.lock().unwrap() = Instant::now();
                    // Tee to attached human viewers; drop any that error or
                    // stall (1s write timeout) so they can't block this thread.
                    shared
                        .taps
                        .lock()
                        .unwrap()
                        .retain_mut(|t| t.write_all(&buf[..n]).is_ok());
                    shared.notify.notify_waiters();
                }
            }
        }
    });
}

/// Reap the child to record its real exit status (the reader thread only sees
/// EOF, not the code). One blocking std thread per session, parked in `wait`.
fn spawn_waiter(mut child: Box<dyn portable_pty::Child + Send + Sync>, shared: Arc<Shared>) {
    std::thread::spawn(move || {
        let code = child.wait().map(|s| s.exit_code() as i32).unwrap_or(-1);
        *shared.exited.lock().unwrap() = Some(code);
        shared.notify.notify_waiters();
    });
}

pub struct SessionManager {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    counter: AtomicU64,
    idle_timeout: Duration,
    default_scrollback: usize,
    max_sessions: usize,
    askpass: Option<String>,
}

/// Result of opening a session: the new session, plus the id of any session
/// evicted to stay under the cap (reported back to the caller).
pub struct OpenResult {
    pub session: Arc<Session>,
    pub evicted: Option<String>,
}

impl SessionManager {
    pub fn new(
        idle_timeout: Duration,
        default_scrollback: usize,
        max_sessions: usize,
        askpass: Option<String>,
    ) -> Arc<Self> {
        let mgr = Arc::new(SessionManager {
            sessions: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(1),
            idle_timeout,
            default_scrollback,
            max_sessions: max_sessions.max(1),
            askpass,
        });
        mgr.clone().spawn_reaper();
        mgr
    }

    pub fn open(&self, mut p: OpenParams) -> Result<OpenResult> {
        if p.scrollback == 0 {
            p.scrollback = self.default_scrollback;
        }
        let id = format!("pty-{}", self.counter.fetch_add(1, Ordering::Relaxed));
        let session = Session::spawn(id.clone(), &p, self.askpass.as_deref())?;

        let mut sessions = self.sessions.lock().unwrap();
        // Enforce the cap by evicting the best victim (dropping it kills the
        // child): exited sessions first, then the most-idle one — never a
        // session that's actively in use just because it's old.
        let mut evicted = None;
        if sessions.len() >= self.max_sessions
            && let Some(victim) = sessions
                .values()
                .max_by_key(|s| (s.is_exited(), s.idle()))
                .map(|s| s.id.clone())
        {
            sessions.remove(&victim);
            evicted = Some(victim);
        }
        sessions.insert(id, Arc::clone(&session));
        Ok(OpenResult { session, evicted })
    }

    pub fn get(&self, id: &str) -> Result<Arc<Session>> {
        self.sessions
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("no such session: {id}"))
    }

    pub fn close(&self, id: &str) -> Result<()> {
        self.sessions
            .lock()
            .unwrap()
            .remove(id)
            .ok_or_else(|| anyhow!("no such session: {id}"))?;
        // Drop kills the child + reaps the reader thread on EOF.
        Ok(())
    }

    pub fn list(&self) -> Vec<Arc<Session>> {
        let mut v: Vec<_> = self.sessions.lock().unwrap().values().cloned().collect();
        v.sort_by_key(|s| s.created);
        v
    }

    fn spawn_reaper(self: Arc<Self>) {
        if self.idle_timeout.is_zero() {
            return;
        }
        tokio::spawn(async move {
            // Keep exited sessions around for a grace period: a crashed process's
            // final screen is the post-mortem, and reaping it on the next tick
            // would erase the evidence before the agent reads it.
            const EXITED_GRACE: Duration = Duration::from_secs(300);
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            loop {
                tick.tick().await;
                let dead: Vec<String> = {
                    let sessions = self.sessions.lock().unwrap();
                    sessions
                        .values()
                        .filter(|s| {
                            s.idle() > self.idle_timeout
                                || (s.is_exited() && s.idle() > EXITED_GRACE)
                        })
                        .map(|s| s.id.clone())
                        .collect()
                };
                if !dead.is_empty() {
                    let mut sessions = self.sessions.lock().unwrap();
                    for id in dead {
                        sessions.remove(&id);
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> OpenParams {
        OpenParams {
            shell: Some("/bin/sh".into()),
            cwd: None,
            cols: 80,
            rows: 24,
            env: vec![],
            scrollback: 200,
        }
    }

    fn mgr() -> Arc<SessionManager> {
        SessionManager::new(Duration::from_secs(600), 1000, 50, None)
    }

    #[tokio::test]
    async fn echo_roundtrip() {
        let s = mgr().open(params()).unwrap().session;
        s.write(b"printf hello\r\n").unwrap();
        let re = regex::Regex::new("hello").unwrap();
        assert!(
            s.wait(Some(&re), Duration::ZERO, Duration::from_secs(5))
                .await,
            "expected 'hello' on screen, got: {:?}",
            s.snapshot(0).screen
        );
    }

    #[tokio::test]
    async fn exit_detected() {
        let s = mgr().open(params()).unwrap().session;
        s.write(b"exit\r\n").unwrap();
        // wait() returns true on exit regardless of pattern.
        let re = regex::Regex::new("this-will-never-match").unwrap();
        assert!(
            s.wait(Some(&re), Duration::ZERO, Duration::from_secs(5))
                .await
        );
        assert!(s.is_exited());
    }

    #[tokio::test]
    async fn real_exit_code_captured() {
        let s = mgr().open(params()).unwrap().session;
        s.write(b"exit 7\r\n").unwrap();
        let re = regex::Regex::new("nope").unwrap();
        s.wait(Some(&re), Duration::ZERO, Duration::from_secs(5))
            .await;
        assert_eq!(
            s.snapshot(0).exited,
            Some(7),
            "should capture real exit code"
        );
    }

    #[tokio::test]
    async fn evicts_most_idle_at_cap() {
        let m = SessionManager::new(Duration::from_secs(600), 1000, 2, None);
        let a = m.open(params()).unwrap();
        assert!(a.evicted.is_none());
        // Touch nothing on a; write to b so it's the recently-active one.
        let b = m.open(params()).unwrap();
        b.session.write(b"true\r\n").unwrap();
        let c = m.open(params()).unwrap(); // over cap of 2
        assert_eq!(c.evicted.as_deref(), Some(a.session.id.as_str()));
        assert_eq!(m.list().len(), 2);
    }

    #[tokio::test]
    async fn evicts_exited_before_idle() {
        let m = SessionManager::new(Duration::from_secs(600), 1000, 2, None);
        let a = m.open(params()).unwrap(); // oldest + most idle, but alive
        let b = m.open(params()).unwrap();
        b.session.write(b"exit\r\n").unwrap();
        let re = regex::Regex::new("never-matches").unwrap();
        b.session
            .wait(Some(&re), Duration::ZERO, Duration::from_secs(5))
            .await;
        assert!(b.session.is_exited());
        let c = m.open(params()).unwrap();
        assert_eq!(
            c.evicted.as_deref(),
            Some(b.session.id.as_str()),
            "exited session evicted before the idler live one"
        );
        assert!(m.get(&a.session.id).is_ok());
    }

    #[tokio::test]
    async fn quiet_settle() {
        let s = mgr().open(params()).unwrap().session;
        s.write(b"printf ready\r\n").unwrap();
        assert!(
            s.wait(None, Duration::from_millis(200), Duration::from_secs(5))
                .await
        );
        assert!(s.snapshot(0).screen.contains("ready"));
    }

    #[tokio::test]
    async fn read_dedup_detects_unchanged_screen() {
        let s = mgr().open(params()).unwrap().session;
        s.write(b"printf stable\r\n").unwrap();
        let re = regex::Regex::new("stable").unwrap();
        s.wait(Some(&re), Duration::ZERO, Duration::from_secs(5))
            .await;
        let screen = s.snapshot(0).screen;
        assert!(!s.same_as_last_read(&screen), "first read is fresh");
        assert!(s.same_as_last_read(&screen), "second identical read dedups");
        assert!(
            !s.same_as_last_read("different"),
            "changed screen is fresh again"
        );
    }

    #[tokio::test]
    async fn list_and_close() {
        let m = mgr();
        let s = m.open(params()).unwrap().session;
        assert_eq!(m.list().len(), 1);
        m.close(&s.id).unwrap();
        assert_eq!(m.list().len(), 0);
        assert!(m.get(&s.id).is_err());
    }
}
