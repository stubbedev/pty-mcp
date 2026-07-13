//! MCP server: the 9 tools (`run` + `pty_*`) wired to the session manager.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::exec;
use crate::keys;
use crate::session::{OpenParams, SessionManager};

#[derive(Clone)]
pub struct PtyServer {
    mgr: Arc<SessionManager>,
    /// Optional `--askpass` override command (baked into the sudo shim).
    askpass: Option<String>,
    /// When true, keep sudo's timestamp warm for the whole session after the
    /// first successful auth.
    keepalive: bool,
    keepalive_started: Arc<AtomicBool>,
}

/// Quiet-period after a write/sendkey before returning the screen. Short so
/// interactive driving feels responsive; slow output should use `wait_for`.
const SETTLE_MS: u64 = 40;

fn err(msg: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(msg.into())])
}

fn text(msg: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(msg.into())])
}

fn json(v: serde_json::Value) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(v.to_string())])
}

// ─────────────────────────── tool argument types ───────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct OpenArgs {
    /// Shell/program to run. Defaults to $SHELL, else /bin/sh.
    #[serde(default)]
    pub shell: Option<String>,
    /// Working directory. Defaults to the harness's working directory (the
    /// project the agent is working in).
    #[serde(default)]
    pub cwd: Option<String>,
    /// Terminal columns (default 120).
    #[serde(default)]
    pub cols: Option<u16>,
    /// Terminal rows (default 30).
    #[serde(default)]
    pub rows: Option<u16>,
    /// Extra environment variables.
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    /// Scrollback lines to retain (default 1000).
    #[serde(default)]
    pub scrollback: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
pub struct WriteArgs {
    pub session_id: String,
    /// Raw text to write to the PTY (no implicit newline — add \n or \r yourself).
    pub input: String,
    /// Optional regex; if set, wait until it matches the screen before returning.
    #[serde(default)]
    pub wait_for: Option<String>,
    /// Max wait in milliseconds (default 10000) when wait_for is set.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
pub struct SendKeyArgs {
    pub session_id: String,
    /// Named keys to send in order (e.g. ["ctrl+c"], ["escape", ":", "q", "enter"]).
    pub keys: Vec<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ReadArgs {
    pub session_id: String,
    /// Scrollback lines to include above the visible screen (default 0).
    #[serde(default)]
    pub scrollback_lines: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
pub struct WaitArgs {
    pub session_id: String,
    /// Regex to wait for on screen. If omitted, waits for output to go quiet.
    #[serde(default)]
    pub pattern: Option<String>,
    /// Quiet-period in ms used when `pattern` is omitted (default 500).
    #[serde(default)]
    pub quiet_ms: Option<u64>,
    /// Overall timeout in ms (default 10000).
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ResizeArgs {
    pub session_id: String,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Deserialize, JsonSchema)]
pub struct SessionArg {
    pub session_id: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct RunArgs {
    /// Shell command line, exactly as you'd type it in a terminal (pipes,
    /// globs, &&, quoting all work). Runs in the user's shell + environment.
    pub command: String,
    /// Working directory. Defaults to the harness's working directory (the
    /// project the agent is working in).
    #[serde(default)]
    pub cwd: Option<String>,
    /// Timeout in seconds (default 300, max 3600).
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
}

// ─────────────────────────────── tools ───────────────────────────────

#[tool_router]
impl PtyServer {
    pub fn new(mgr: Arc<SessionManager>, askpass: Option<String>, keepalive: bool) -> Self {
        Self {
            mgr,
            askpass,
            keepalive,
            keepalive_started: Arc::new(AtomicBool::new(false)),
        }
    }

    #[tool(
        description = "Open a persistent interactive terminal (PTY) session running a shell or program. Returns a session_id used by the other pty_* tools. Use this instead of one-shot bash calls when you need to drive an interactive program (REPL, ssh, vim, a prompt)."
    )]
    async fn pty_open(
        &self,
        Parameters(a): Parameters<OpenArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let params = OpenParams {
            shell: a.shell,
            cwd: a.cwd,
            cols: a.cols.unwrap_or(120),
            rows: a.rows.unwrap_or(30),
            env: a.env.into_iter().collect(),
            scrollback: a.scrollback.unwrap_or(0),
        };
        match self.mgr.open(params) {
            Ok(r) => Ok(json(serde_json::json!({
                "session_id": r.session.id,
                "shell": r.session.shell,
                "cwd": r.session.cwd,
                // Present only when opening this session evicted the oldest one
                // to stay under --max-sessions.
                "evicted_session": r.evicted,
            }))),
            Err(e) => Ok(err(format!("failed to open session: {e}"))),
        }
    }

    #[tool(
        description = "Write text to a session's PTY, then optionally wait for a regex to appear on screen. Add your own newline (\\n or \\r) to submit a command. Returns the resulting screen."
    )]
    async fn pty_write(
        &self,
        Parameters(a): Parameters<WriteArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let s = match self.mgr.get(&a.session_id) {
            Ok(s) => s,
            Err(e) => return Ok(err(e.to_string())),
        };
        if let Err(e) = s.write(a.input.as_bytes()) {
            return Ok(err(format!("write failed: {e}")));
        }
        let timeout = Duration::from_millis(a.timeout_ms.unwrap_or(10_000));
        match a.wait_for {
            Some(pat) => match regex::Regex::new(&pat) {
                Ok(re) => {
                    let matched = s.wait(Some(&re), Duration::ZERO, timeout).await;
                    let snap = s.snapshot(0);
                    Ok(text(format!(
                        "{}{}",
                        if matched {
                            ""
                        } else {
                            "[timeout waiting for pattern]\n"
                        },
                        snap.screen
                    )))
                }
                Err(e) => Ok(err(format!("invalid wait_for regex: {e}"))),
            },
            None => {
                // Brief settle so the caller sees the immediate echo, kept
                // short so interactive driving stays snappy.
                s.wait(
                    None,
                    Duration::from_millis(SETTLE_MS),
                    Duration::from_millis(1000),
                )
                .await;
                Ok(text(s.snapshot(0).screen))
            }
        }
    }

    #[tool(
        description = "Send named keys to a session (control chars, arrows, function keys). Supported: enter, tab, backtab, escape, space, backspace, delete, up, down, left, right, home, end, pageup, pagedown, insert, f1-f12, and any ctrl+<letter>."
    )]
    async fn pty_sendkey(
        &self,
        Parameters(a): Parameters<SendKeyArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let s = match self.mgr.get(&a.session_id) {
            Ok(s) => s,
            Err(e) => return Ok(err(e.to_string())),
        };
        let mut bytes = Vec::new();
        for k in &a.keys {
            match keys::key_bytes(k) {
                Some(b) => bytes.extend_from_slice(&b),
                None => {
                    return Ok(err(format!(
                        "unknown key: {k}. Supported: {}",
                        keys::SUPPORTED
                    )));
                }
            }
        }
        if let Err(e) = s.write(&bytes) {
            return Ok(err(format!("write failed: {e}")));
        }
        s.wait(
            None,
            Duration::from_millis(SETTLE_MS),
            Duration::from_millis(1000),
        )
        .await;
        Ok(text(s.snapshot(0).screen))
    }

    #[tool(
        description = "Read the current screen of a session: rendered terminal text, cursor position, whether an alt-screen app (vim/htop) is active, and exit status if the shell has ended."
    )]
    async fn pty_read(
        &self,
        Parameters(a): Parameters<ReadArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let s = match self.mgr.get(&a.session_id) {
            Ok(s) => s,
            Err(e) => return Ok(err(e.to_string())),
        };
        let snap = s.snapshot(a.scrollback_lines.unwrap_or(0));
        Ok(json(serde_json::json!({
            "screen": snap.screen,
            "cursor": { "row": snap.cursor_row, "col": snap.cursor_col },
            "alt_screen": snap.alt_screen,
            "exited": snap.exited,
        })))
    }

    #[tool(
        description = "Block until a session's output matches `pattern` (regex), or — if no pattern is given — until output has been quiet for `quiet_ms`. Returns the screen. Useful before reading the result of a slow command."
    )]
    async fn pty_wait(
        &self,
        Parameters(a): Parameters<WaitArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let s = match self.mgr.get(&a.session_id) {
            Ok(s) => s,
            Err(e) => return Ok(err(e.to_string())),
        };
        let timeout = Duration::from_millis(a.timeout_ms.unwrap_or(10_000));
        let quiet = Duration::from_millis(a.quiet_ms.unwrap_or(500));
        let matched = match &a.pattern {
            Some(pat) => match regex::Regex::new(pat) {
                Ok(re) => s.wait(Some(&re), quiet, timeout).await,
                Err(e) => return Ok(err(format!("invalid pattern regex: {e}"))),
            },
            None => s.wait(None, quiet, timeout).await,
        };
        let snap = s.snapshot(0);
        Ok(text(format!(
            "{}{}",
            if matched { "" } else { "[timeout]\n" },
            snap.screen
        )))
    }

    #[tool(description = "Resize a session's terminal (sends SIGWINCH to the program).")]
    async fn pty_resize(
        &self,
        Parameters(a): Parameters<ResizeArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let s = match self.mgr.get(&a.session_id) {
            Ok(s) => s,
            Err(e) => return Ok(err(e.to_string())),
        };
        match s.resize(a.cols, a.rows) {
            Ok(()) => Ok(text(format!("resized to {}x{}", a.cols, a.rows))),
            Err(e) => Ok(err(format!("resize failed: {e}"))),
        }
    }

    #[tool(description = "Close a session: terminate its program and free the PTY.")]
    async fn pty_close(
        &self,
        Parameters(a): Parameters<SessionArg>,
    ) -> Result<CallToolResult, ErrorData> {
        match self.mgr.close(&a.session_id) {
            Ok(()) => Ok(text(format!("closed {}", a.session_id))),
            Err(e) => Ok(err(e.to_string())),
        }
    }

    #[tool(description = "List active PTY sessions with their shell, cwd, age, and idle time.")]
    async fn pty_list(&self) -> Result<CallToolResult, ErrorData> {
        let list: Vec<_> = self
            .mgr
            .list()
            .iter()
            .map(|s| {
                serde_json::json!({
                    "session_id": s.id,
                    "shell": s.shell,
                    "cwd": s.cwd,
                    "age_secs": s.created.elapsed().as_secs(),
                    "idle_secs": s.idle().as_secs(),
                    "exited": s.is_exited(),
                })
            })
            .collect();
        Ok(json(serde_json::json!({ "sessions": list })))
    }

    #[tool(
        description = "Preferred way to run one-shot shell commands. Runs exactly as the user would in their own terminal: their shell, their full environment (PATH matches their interactive shell — nix, cargo, custom bins), defaulting to the project directory you're working in. Pipes/globs/&&/quoting all work. Prefix with sudo for privileged commands — the password is entered in an OS dialog, never in your context. For persistent/interactive programs (REPL, vim, ssh) use pty_open instead."
    )]
    async fn run(&self, Parameters(a): Parameters<RunArgs>) -> Result<CallToolResult, ErrorData> {
        let secs = a.timeout_seconds.unwrap_or(300).min(3600);
        tracing::info!(command = %a.command, "run");
        match exec::run(
            &a.command,
            a.cwd.as_deref(),
            Duration::from_secs(secs),
            self.askpass.as_deref(),
        )
        .await
        {
            Ok(out) => {
                // After a successful sudo command, optionally hold the
                // timestamp for the rest of the session so later sudo commands
                // skip the prompt.
                if self.keepalive && out.used_sudo && !out.timed_out {
                    exec::spawn_keepalive(Arc::clone(&self.keepalive_started));
                }
                Ok(json(serde_json::json!({
                    "exit_code": out.exit_code,
                    "stdout": out.stdout,
                    "stderr": out.stderr,
                    "timed_out": out.timed_out,
                })))
            }
            Err(e) => Ok(err(format!("run failed: {e}"))),
        }
    }
}

#[tool_handler]
impl ServerHandler for PtyServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.instructions = Some(
            "Preferred shell for this session. Use `run` for one-shot commands (build, test, \
             install, git) — it runs in the user's full login-shell environment; prefix with \
             sudo for privileged commands (password prompted via OS dialog, never in context). \
             For interactive programs open a session with pty_open, drive it with \
             pty_write/pty_sendkey, and observe with pty_read/pty_wait."
                .into(),
        );
        info.server_info.name = "pty-mcp".into();
        info.server_info.version = env!("CARGO_PKG_VERSION").into();
        info
    }
}
