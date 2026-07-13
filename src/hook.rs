//! Agent-harness integration: the `hook` (pre-tool redirect) and `install`
//! subcommands that make an agent route shell commands through pty-mcp's `run`
//! tool instead of its built-in bash tool.
//!
//! An MCP server can't *make* a client prefer its tools — that's the client's
//! call, and bash is the default reflex. A pre-tool hook is the only reliable
//! lever: it inspects each bash call and, unless it's something `run` can't do,
//! denies it with a reason that tells the model to re-issue via
//! `mcp__pty-mcp__run` (carrying the cwd so relative paths survive).
//!
//! Four harnesses are supported. Their hook systems converged on nearly the same
//! shape — a `{tool_name, tool_input:{command}, cwd, hook_event_name}` event on
//! stdin — so ONE `pty-mcp hook` binary serves all of them; only the deny-output
//! encoding differs, keyed on `hook_event_name`:
//!
//! * Claude Code / Codex — event `PreToolUse`, nested `hookSpecificOutput`.
//! * Gemini CLI — event `BeforeTool`, flat `{decision, reason}`.
//! * opencode — an installed JS plugin shells out to us with a synthetic
//!   `opencode` event; flat output, and the plugin throws to deny.

use std::io::Read;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::{Value, json};

// ─────────────────────────────── the hook ───────────────────────────────

/// Shell/bash tool names across the supported harnesses.
const SHELL_TOOLS: [&str; 3] = ["Bash", "bash", "run_shell_command"];

/// Read a pre-tool event on stdin and decide whether a shell call proceeds or
/// gets redirected to `mcp__pty-mcp__run`. Deny → print the harness's deny JSON.
/// Allow-through (background jobs, or an explicit `#bash` opt-out) → exit
/// silently so the harness's normal flow runs.
pub fn run_hook() -> ! {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    if let Some((reason, event)) = decide(&input) {
        println!("{}", deny_json(&reason, &event));
    }
    std::process::exit(0);
}

/// `Some((reason, event))` → deny and redirect; `None` → let the call proceed.
fn decide(input: &str) -> Option<(String, String)> {
    let (reason, cwd, event) = classify(input)?;
    // Noop if the pty-mcp MCP server isn't registered anywhere: redirecting to a
    // tool that doesn't exist would strand every command. Fail open.
    if !mcp_registered(&cwd) {
        return None;
    }
    Some((reason, event))
}

/// The pure decision, independent of MCP registration: `Some((reason, cwd,
/// event))` to redirect, `None` to let the call proceed.
fn classify(input: &str) -> Option<(String, String, String)> {
    let v: Value = serde_json::from_str(input).ok()?;
    if !SHELL_TOOLS.contains(&v.get("tool_name")?.as_str()?) {
        return None;
    }
    let ti = v.get("tool_input")?;
    let command = ti.get("command")?.as_str()?;
    if command.trim().is_empty() {
        return None;
    }

    // Escape hatches: `run` has no TTY and can't background, and the model may
    // deliberately want built-in bash — honor an explicit opt-out.
    if ti.get("run_in_background").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    if has_bash_optout(command) {
        return None;
    }

    let cwd = v
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let event = v
        .get("hook_event_name")
        .and_then(Value::as_str)
        .unwrap_or("PreToolUse")
        .to_string();
    Some((redirect_reason(command, &cwd), cwd, event))
}

/// Encode a deny in the shape the calling harness expects. Gemini and our
/// opencode shim take the flat `{decision, reason}`; Claude Code and Codex take
/// the nested `hookSpecificOutput.permissionDecision`.
fn deny_json(reason: &str, event: &str) -> Value {
    if event == "BeforeTool" || event == "opencode" {
        json!({ "decision": "deny", "reason": reason })
    } else {
        json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            }
        })
    }
}

/// Is the pty-mcp MCP server registered anywhere a supported harness would look?
/// Best-effort substring scan across every harness's MCP-config location — if
/// the server is registered in the harness that's calling us, one of these
/// mentions it. Fail open (return false → hook noops) when none do.
fn mcp_registered(cwd: &str) -> bool {
    let home = PathBuf::from(crate::userenv::home());
    let mut candidates = vec![
        home.join(".claude.json"),                  // Claude (user/local)
        home.join(".codex").join("config.toml"),    // Codex
        home.join(".gemini").join("settings.json"), // Gemini
        home.join(".config").join("opencode").join("opencode.json"), // opencode
        home.join(".config").join("opencode").join("opencode.jsonc"),
    ];
    if !cwd.is_empty() {
        let d = PathBuf::from(cwd);
        candidates.push(d.join(".mcp.json")); // Claude project scope
        candidates.push(d.join("opencode.json"));
        candidates.push(d.join(".gemini").join("settings.json"));
    }
    candidates.iter().any(|p| file_mentions(p, "pty-mcp"))
}

fn file_mentions(path: &std::path::Path, needle: &str) -> bool {
    std::fs::read_to_string(path)
        .map(|s| s.contains(needle))
        .unwrap_or(false)
}

/// A trailing `#bash` comment (e.g. `ls -la #bash`) opts this call out of the
/// redirect. Bash ignores the comment, so the command still runs unchanged.
/// Anchored to the end so a command merely *mentioning* it (`grep '#bash'`)
/// doesn't silently opt out.
fn has_bash_optout(command: &str) -> bool {
    let c = command.trim_end().to_ascii_lowercase();
    c.ends_with("#bash") || c.ends_with("# bash")
}

fn redirect_reason(command: &str, cwd: &str) -> String {
    let cwd_clause = if cwd.is_empty() {
        String::new()
    } else {
        format!(" and cwd={cwd:?}")
    };
    format!(
        "Route shell through pty-mcp: call the mcp__pty-mcp__run tool with \
         command={command:?}{cwd_clause} instead of the built-in bash tool. \
         run executes in your full login-shell environment (PATH with nix/cargo/etc.) \
         and sends any sudo prompt to the OS password dialog — never your context. \
         To force built-in bash for this one call, append \" #bash\" to the command \
         or set run_in_background:true."
    )
}

// ───────────────────────────── the installer ─────────────────────────────

/// Agent harnesses we know how to wire the redirect into. We only touch a
/// harness that's actually installed — detected by its config directory
/// existing — so `install` never fabricates config for a tool you don't use.
#[derive(Clone, Copy)]
enum Harness {
    ClaudeCode,
    Codex,
    GeminiCli,
    Opencode,
}

/// What to do with a harness's config file.
enum Action {
    Write(String),
    Remove,
}

impl Harness {
    const ALL: &'static [Harness] = &[
        Harness::ClaudeCode,
        Harness::Codex,
        Harness::GeminiCli,
        Harness::Opencode,
    ];

    fn name(self) -> &'static str {
        match self {
            Harness::ClaudeCode => "Claude Code",
            Harness::Codex => "Codex",
            Harness::GeminiCli => "Gemini CLI",
            Harness::Opencode => "opencode",
        }
    }

    /// Config directory whose existence means the harness is installed.
    fn dir(self) -> PathBuf {
        let home = PathBuf::from(crate::userenv::home());
        match self {
            Harness::ClaudeCode => home.join(".claude"),
            Harness::Codex => home.join(".codex"),
            Harness::GeminiCli => home.join(".gemini"),
            Harness::Opencode => home.join(".config").join("opencode"),
        }
    }

    /// The file we create/mutate to install the hook.
    fn target(self) -> PathBuf {
        let dir = self.dir();
        match self {
            Harness::ClaudeCode => dir.join("settings.json"),
            Harness::Codex => dir.join("hooks.json"),
            Harness::GeminiCli => dir.join("settings.json"),
            Harness::Opencode => dir.join("plugins").join("pty-mcp.js"),
        }
    }

    /// For a JSON-hook harness: `(hooks-key, tool matcher)`. `None` for opencode,
    /// which uses a JS plugin instead of a config-declared command hook.
    fn json_spec(self) -> Option<(&'static str, &'static str)> {
        match self {
            Harness::ClaudeCode => Some(("PreToolUse", "Bash")),
            Harness::Codex => Some(("PreToolUse", "^Bash$")),
            Harness::GeminiCli => Some(("BeforeTool", "run_shell_command")),
            Harness::Opencode => None,
        }
    }

    fn detect_installed() -> Vec<Harness> {
        Harness::ALL
            .iter()
            .copied()
            .filter(|h| h.dir().is_dir())
            .collect()
    }

    fn names() -> String {
        Harness::ALL
            .iter()
            .map(|h| h.name())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Add (or, with `uninstall`, remove) the redirect hook. With no `project`,
/// mutate every installed harness; `project` targets Claude Code's per-project
/// `<dir>/.claude/settings.json` (the only harness with a stable project scope).
pub fn install(project: Option<PathBuf>, print_only: bool, uninstall: bool) -> Result<()> {
    let targets: Vec<(Harness, PathBuf)> = match &project {
        Some(dir) => vec![(
            Harness::ClaudeCode,
            dir.join(".claude").join("settings.json"),
        )],
        None => Harness::detect_installed()
            .into_iter()
            .map(|h| (h, h.target()))
            .collect(),
    };
    if targets.is_empty() {
        println!(
            "No supported agent harness detected (looked for: {}).",
            Harness::names()
        );
        return Ok(());
    }

    for (h, path) in &targets {
        let (action, _changed) = plan(*h, path, uninstall)?;
        if print_only {
            match &action {
                Action::Write(c) => {
                    println!("# {} → {} (not written)\n{c}", h.name(), path.display())
                }
                Action::Remove => {
                    println!("# {} → remove {} (not written)", h.name(), path.display())
                }
            }
            continue;
        }
        match action {
            Action::Write(c) => write_file(path, &c)?,
            Action::Remove => {
                if path.exists() {
                    std::fs::remove_file(path)
                        .with_context(|| format!("remove {}", path.display()))?;
                }
            }
        }
        let verb = if uninstall { "Removed" } else { "Installed" };
        println!("{verb} pty-mcp hook: {} ({})", path.display(), h.name());
    }
    if !uninstall && !print_only {
        warn_if_mcp_unregistered();
    }
    Ok(())
}

/// Called at server startup with `--install-hook`: add the hook to every
/// installed harness if missing. Logs via tracing (stderr) — never stdout, which
/// on the stdio transport is the JSON-RPC stream. Best-effort; never removes.
/// The hook takes effect the *next* session, since harnesses load hooks at
/// startup and won't hot-apply an externally written one (a security measure).
pub fn install_on_spawn() {
    for h in Harness::detect_installed() {
        let path = h.target();
        let res = plan(h, &path, false).and_then(|(action, changed)| {
            if let Action::Write(c) = action {
                write_file(&path, &c)?;
            }
            Ok(changed)
        });
        match res {
            Ok(true) => tracing::info!(
                "installed {} hook at {} — active next session",
                h.name(),
                path.display()
            ),
            Ok(false) => tracing::debug!(harness = h.name(), "hook already present"),
            Err(e) => tracing::warn!(error = %e, harness = h.name(), "hook self-install failed"),
        }
    }
}

/// Compute the action for one harness: the file content to write (or a remove),
/// and whether it differs from what's on disk. For JSON harnesses we strip our
/// old entry first — idempotent, and refreshes a stale binary path on reinstall.
fn plan(h: Harness, path: &std::path::Path, uninstall: bool) -> Result<(Action, bool)> {
    match h.json_spec() {
        Some((event, matcher)) => {
            let mut root = read_json(path)?;
            let before = root.clone();
            remove_our_hook(&mut root, event);
            if !uninstall {
                add_hook(&mut root, event, matcher, &hook_command());
            }
            let changed = root != before;
            Ok((
                Action::Write(serde_json::to_string_pretty(&root)? + "\n"),
                changed,
            ))
        }
        None => {
            // opencode: a JS plugin file, all-or-nothing.
            if uninstall {
                Ok((Action::Remove, path.exists()))
            } else {
                let changed = std::fs::read_to_string(path)
                    .map(|s| s != OPENCODE_PLUGIN)
                    .unwrap_or(true);
                Ok((Action::Write(OPENCODE_PLUGIN.to_string()), changed))
            }
        }
    }
}

fn write_file(path: &std::path::Path, content: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    std::fs::write(path, content).with_context(|| format!("write {}", path.display()))
}

fn read_json(path: &std::path::Path) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(s) if !s.trim().is_empty() => {
            serde_json::from_str(&s).with_context(|| format!("parse {}", path.display()))
        }
        _ => Ok(json!({})),
    }
}

/// The command a harness runs for the hook: a bare binary name resolved on PATH
/// at run time, NOT an absolute path. A pinned `/nix/store/<hash>/…` path breaks
/// the moment you upgrade or garbage-collect that generation; the name tracks
/// whatever `pty-mcp` is on PATH — the same binary the harness already launches
/// the server with, so if the server runs, the hook resolves.
fn hook_command() -> String {
    let name = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "pty-mcp".to_string());
    format!("{name} hook")
}

/// Our hook: any command entry that runs this binary's `hook` subcommand.
fn is_our_hook(cmd: &str) -> bool {
    cmd.contains("pty-mcp") && cmd.trim_end().ends_with("hook")
}

fn add_hook(root: &mut Value, event: &str, matcher: &str, command: &str) {
    let entry = json!({
        "matcher": matcher,
        "hooks": [ { "type": "command", "command": command } ],
    });
    let arr = root
        .as_object_mut()
        .expect("root is an object")
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .expect("hooks is an object")
        .entry(event)
        .or_insert_with(|| json!([]));
    if let Some(a) = arr.as_array_mut() {
        a.push(entry);
    } else {
        *arr = json!([entry]);
    }
}

/// Remove our hook from a harness's `event` array, pruning entries and keys that
/// become empty so we never leave dangling `{}` / `[]` behind.
fn remove_our_hook(root: &mut Value, event: &str) {
    let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    if let Some(pre) = hooks.get_mut(event).and_then(Value::as_array_mut) {
        for entry in pre.iter_mut() {
            if let Some(list) = entry.get_mut("hooks").and_then(Value::as_array_mut) {
                list.retain(|h| {
                    h.get("command")
                        .and_then(Value::as_str)
                        .map(|c| !is_our_hook(c))
                        .unwrap_or(true)
                });
            }
        }
        pre.retain(|e| {
            e.get("hooks")
                .and_then(Value::as_array)
                .map(|l| !l.is_empty())
                .unwrap_or(true)
        });
        if pre.is_empty() {
            hooks.remove(event);
        }
    }
    if hooks.is_empty() {
        root.as_object_mut().unwrap().remove("hooks");
    }
}

/// Best-effort: the hook is a footgun if the MCP server isn't registered, so
/// nudge the user when we can't find it in any harness config.
fn warn_if_mcp_unregistered() {
    if !mcp_registered("") {
        println!(
            "\nNote: couldn't confirm the pty-mcp MCP server is registered in any harness. \
             If shell calls start failing, register it, e.g.:\n    claude mcp add pty-mcp -- pty-mcp"
        );
    }
}

/// opencode plugin: intercepts the `bash` tool and shells out to `pty-mcp hook`
/// for the decision (keeping all logic in one place), throwing the reason to
/// deny. Fails open if pty-mcp isn't on PATH.
const OPENCODE_PLUGIN: &str = r#"// pty-mcp — route the `bash` tool through the pty-mcp `run` MCP tool.
// Auto-generated by `pty-mcp install`. Delete this file to disable.
export const PtyMcp = async ({ directory }) => ({
  "tool.execute.before": async ({ tool }, output) => {
    if (tool !== "bash") return;
    const event = JSON.stringify({
      hook_event_name: "opencode",
      tool_name: "bash",
      tool_input: output.args,
      cwd: directory,
    });
    let out = "";
    try {
      const proc = Bun.spawn(["pty-mcp", "hook"], {
        stdin: "pipe",
        stdout: "pipe",
        stderr: "ignore",
      });
      proc.stdin.write(event);
      proc.stdin.end();
      out = await new Response(proc.stdout).text();
      await proc.exited;
    } catch {
      return; // pty-mcp not on PATH → fail open, allow bash
    }
    const trimmed = out.trim();
    if (!trimmed) return;
    let decision;
    try {
      decision = JSON.parse(trimmed);
    } catch {
      return;
    }
    if (decision && decision.decision === "deny") throw new Error(decision.reason);
  },
});
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn event(command: &str) -> String {
        json!({
            "tool_name": "Bash",
            "cwd": "/home/x/proj",
            "tool_input": { "command": command },
        })
        .to_string()
    }

    fn reason(command: &str) -> Option<String> {
        classify(&event(command)).map(|(r, _, _)| r)
    }

    #[test]
    fn redirects_plain_bash() {
        let (r, cwd, event) = classify(&event("cargo build")).unwrap();
        assert!(r.contains("mcp__pty-mcp__run"));
        assert!(r.contains("cargo build"));
        assert!(r.contains("/home/x/proj"));
        assert_eq!(cwd, "/home/x/proj");
        assert_eq!(event, "PreToolUse");
    }

    #[test]
    fn accepts_gemini_shell_tool() {
        let ev = json!({
            "tool_name": "run_shell_command",
            "hook_event_name": "BeforeTool",
            "tool_input": { "command": "ls" },
        })
        .to_string();
        let (_, _, event) = classify(&ev).unwrap();
        assert_eq!(event, "BeforeTool");
    }

    #[test]
    fn allows_background() {
        let ev = json!({
            "tool_name": "Bash",
            "tool_input": { "command": "sleep 5", "run_in_background": true },
        })
        .to_string();
        assert!(classify(&ev).is_none());
    }

    #[test]
    fn allows_optout() {
        assert!(reason("ls -la #bash").is_none());
        assert!(reason("ls -la # bash  ").is_none());
    }

    #[test]
    fn optout_must_be_trailing() {
        // Merely mentioning #bash mid-command is not an opt-out.
        assert!(reason("grep '#bash' src/hook.rs").is_some());
        assert!(reason("echo '#bash' > note.txt").is_some());
    }

    #[test]
    fn ignores_other_tools() {
        let ev = json!({ "tool_name": "Read", "tool_input": {} }).to_string();
        assert!(classify(&ev).is_none());
    }

    #[test]
    fn ignores_empty_command() {
        assert!(reason("   ").is_none());
    }

    #[test]
    fn deny_json_shapes() {
        let nested = deny_json("why", "PreToolUse");
        assert_eq!(nested["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(
            nested["hookSpecificOutput"]["permissionDecisionReason"],
            "why"
        );

        let flat = deny_json("why", "BeforeTool");
        assert_eq!(flat["decision"], "deny");
        assert_eq!(flat["reason"], "why");
        // opencode shim uses the flat shape too.
        assert_eq!(deny_json("why", "opencode")["decision"], "deny");
    }

    #[test]
    fn install_roundtrip_is_idempotent() {
        let mut root = json!({ "theme": "dark" });
        add_hook(&mut root, "PreToolUse", "Bash", "pty-mcp hook");
        remove_our_hook(&mut root, "PreToolUse");
        add_hook(&mut root, "PreToolUse", "Bash", "pty-mcp hook");
        let arr = root["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["matcher"], "Bash");
        assert_eq!(root["theme"], "dark");
        remove_our_hook(&mut root, "PreToolUse");
        assert!(root.get("hooks").is_none());
        assert_eq!(root, json!({ "theme": "dark" }));
    }

    #[test]
    fn gemini_uses_before_tool_key() {
        let mut root = json!({});
        add_hook(&mut root, "BeforeTool", "run_shell_command", "pty-mcp hook");
        assert_eq!(
            root["hooks"]["BeforeTool"][0]["matcher"],
            "run_shell_command"
        );
    }

    #[test]
    fn remove_preserves_foreign_hooks() {
        let mut root = json!({
            "hooks": { "PreToolUse": [
                { "matcher": "Bash", "hooks": [
                    { "type": "command", "command": "/x/pty-mcp hook" },
                    { "type": "command", "command": "other-tool" }
                ]}
            ]}
        });
        remove_our_hook(&mut root, "PreToolUse");
        let list = root["hooks"]["PreToolUse"][0]["hooks"].as_array().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["command"], "other-tool");
    }

    #[test]
    fn file_mentions_detects_registration() {
        let missing = std::env::temp_dir().join("pty-mcp-nope-does-not-exist.json");
        assert!(!file_mentions(&missing, "pty-mcp"));

        let present = std::env::temp_dir().join("pty-mcp-hook-test-config.json");
        std::fs::write(&present, r#"{"mcpServers":{"pty-mcp":{}}}"#).unwrap();
        assert!(file_mentions(&present, "pty-mcp"));
        let _ = std::fs::remove_file(&present);
    }
}
