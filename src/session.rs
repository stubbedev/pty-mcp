//! PTY session lifecycle: spawn, background reader → emulator, wait/notify,
//! idle reaping.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::Notify;

use crate::screen::Emulator;

/// Shared, thread-crossing state: the reader thread writes it, async tool
/// handlers read it.
struct Shared {
    emu: Mutex<Emulator>,
    /// Woken on every chunk of new output and on child exit.
    notify: Notify,
    last_activity: Mutex<Instant>,
    /// `Some(code)` once the child has exited (EOF on the PTY).
    exited: Mutex<Option<i32>>,
}

pub struct Session {
    pub id: String,
    pub shell: String,
    pub cwd: String,
    pub created: Instant,
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    shared: Arc<Shared>,
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
    fn spawn(id: String, p: &OpenParams) -> Result<Arc<Session>> {
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: p.rows,
                cols: p.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty")?;

        let shell = p
            .shell
            .clone()
            .or_else(|| std::env::var("SHELL").ok())
            .unwrap_or_else(|| "/bin/sh".to_string());
        let mut cmd = CommandBuilder::new(&shell);
        if let Some(cwd) = &p.cwd {
            cmd.cwd(cwd);
        }
        // A sane default TERM so programs emit the escape sequences the
        // emulator understands.
        cmd.env("TERM", "xterm-256color");
        for (k, v) in &p.env {
            cmd.env(k, v);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .context("spawn shell in pty")?;
        // Drop the slave so the PTY reports EOF once the child exits.
        drop(pair.slave);

        let writer = pair.master.take_writer().context("take pty writer")?;
        let reader = pair.master.try_clone_reader().context("clone pty reader")?;

        let cwd = p
            .cwd
            .clone()
            .or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|d| d.display().to_string())
            })
            .unwrap_or_default();

        let shared = Arc::new(Shared {
            emu: Mutex::new(Emulator::new(p.cols, p.rows, p.scrollback)),
            notify: Notify::new(),
            last_activity: Mutex::new(Instant::now()),
            exited: Mutex::new(None),
        });

        spawn_reader(reader, Arc::clone(&shared));

        Ok(Arc::new(Session {
            id,
            shell,
            cwd,
            created: Instant::now(),
            writer: Mutex::new(writer),
            master: Mutex::new(pair.master),
            child: Mutex::new(child),
            shared,
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
                if re.is_match(&self.shared.emu.lock().unwrap().visible_text()) {
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
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill();
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
                    *shared.exited.lock().unwrap() = Some(0);
                    shared.notify.notify_waiters();
                    break;
                }
                Ok(n) => {
                    shared.emu.lock().unwrap().advance(&buf[..n]);
                    *shared.last_activity.lock().unwrap() = Instant::now();
                    shared.notify.notify_waiters();
                }
            }
        }
    });
}

pub struct SessionManager {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    counter: AtomicU64,
    idle_timeout: Duration,
    default_scrollback: usize,
}

impl SessionManager {
    pub fn new(idle_timeout: Duration, default_scrollback: usize) -> Arc<Self> {
        let mgr = Arc::new(SessionManager {
            sessions: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(1),
            idle_timeout,
            default_scrollback,
        });
        mgr.clone().spawn_reaper();
        mgr
    }

    pub fn open(&self, mut p: OpenParams) -> Result<Arc<Session>> {
        if p.scrollback == 0 {
            p.scrollback = self.default_scrollback;
        }
        let id = format!("pty-{}", self.counter.fetch_add(1, Ordering::Relaxed));
        let session = Session::spawn(id.clone(), &p)?;
        self.sessions
            .lock()
            .unwrap()
            .insert(id, Arc::clone(&session));
        Ok(session)
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
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            loop {
                tick.tick().await;
                let dead: Vec<String> = {
                    let sessions = self.sessions.lock().unwrap();
                    sessions
                        .values()
                        .filter(|s| s.idle() > self.idle_timeout || s.is_exited())
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

    #[tokio::test]
    async fn echo_roundtrip() {
        let mgr = SessionManager::new(Duration::from_secs(600), 1000);
        let s = mgr.open(params()).unwrap();
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
        let mgr = SessionManager::new(Duration::from_secs(600), 1000);
        let s = mgr.open(params()).unwrap();
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
    async fn quiet_settle() {
        let mgr = SessionManager::new(Duration::from_secs(600), 1000);
        let s = mgr.open(params()).unwrap();
        s.write(b"printf ready\r\n").unwrap();
        assert!(
            s.wait(None, Duration::from_millis(200), Duration::from_secs(5))
                .await
        );
        assert!(s.snapshot(0).screen.contains("ready"));
    }

    #[tokio::test]
    async fn list_and_close() {
        let mgr = SessionManager::new(Duration::from_secs(600), 1000);
        let s = mgr.open(params()).unwrap();
        assert_eq!(mgr.list().len(), 1);
        mgr.close(&s.id).unwrap();
        assert_eq!(mgr.list().len(), 0);
        assert!(mgr.get(&s.id).is_err());
    }
}
