//! Capture the user's real login+interactive shell environment once, so `run`
//! and PTY sessions see the same `PATH` (nix, cargo, …) the user gets in their
//! own terminal — even when pty-mcp is launched over HTTP, by a proxy, or from
//! systemd with a stripped environment.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

static USER_ENV: OnceLock<HashMap<String, String>> = OnceLock::new();

/// The user's shell, from `$SHELL`, else `/bin/sh`.
pub fn shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
}

/// Captured user environment (KEY→VALUE). Computed once; cheap thereafter.
pub fn user_env() -> &'static HashMap<String, String> {
    USER_ENV.get_or_init(capture)
}

/// The user's home directory from the captured env, else `$HOME`, else ".".
pub fn home() -> String {
    user_env()
        .get("HOME")
        .cloned()
        .or_else(|| std::env::var("HOME").ok())
        .unwrap_or_else(|| ".".to_string())
}

/// The harness's working directory: MCP clients launch stdio servers in their
/// own cwd (the project dir), so the process cwd is the right session default.
/// Falls back to home when it's unusable — e.g. `/` under systemd or HTTP.
pub fn harness_cwd() -> String {
    std::env::current_dir()
        .ok()
        .filter(|p| p != std::path::Path::new("/"))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(home)
}

/// Run the login+interactive shell and dump its environment. Falls back to the
/// inherited environment if the shell can't be run or times out (e.g. an rc
/// file that blocks). `-l -i` so both profile and rc (where PATH usually lives)
/// are sourced. stdin is closed so an interactive rc can't hang on input.
fn capture() -> HashMap<String, String> {
    match capture_from_shell() {
        Some(env) if env.contains_key("PATH") => env,
        _ => std::env::vars().collect(),
    }
}

fn capture_from_shell() -> Option<HashMap<String, String>> {
    use std::io::Read;
    use std::process::{Command, Stdio};

    let mut child = Command::new(shell())
        .args(["-l", "-i", "-c", "env -0 2>/dev/null || env"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    // Bound the wait so a misbehaving rc can't wedge startup.
    let mut out = String::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                if let Some(mut s) = child.stdout.take() {
                    let _ = s.read_to_string(&mut out);
                }
                break;
            }
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(_) => return None,
        }
    }
    Some(parse_env(&out))
}

/// Parse `env -0` (NUL-separated) output, falling back to newline-separated
/// `KEY=VALUE` lines with continuation for values spanning newlines.
fn parse_env(s: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if s.contains('\0') {
        for entry in s.split('\0') {
            if let Some((k, v)) = entry.split_once('=') {
                map.insert(k.to_string(), v.to_string());
            }
        }
        return map;
    }
    // Newline fallback: a line that doesn't start with KEY= is a continuation
    // of the previous value.
    let mut last: Option<String> = None;
    for line in s.lines() {
        if let Some((k, v)) = split_kv(line) {
            map.insert(k.clone(), v.to_string());
            last = Some(k);
        } else if let Some(k) = &last {
            let e = map.get_mut(k).unwrap();
            e.push('\n');
            e.push_str(line);
        }
    }
    map
}

/// Split `KEY=VALUE` only when KEY is a valid shell identifier.
fn split_kv(line: &str) -> Option<(String, &str)> {
    let (k, v) = line.split_once('=')?;
    if k.is_empty()
        || !k
            .bytes()
            .enumerate()
            .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit()))
    {
        return None;
    }
    Some((k.to_string(), v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nul_separated() {
        let m = parse_env("PATH=/bin\0HOME=/home/x\0");
        assert_eq!(m.get("PATH").unwrap(), "/bin");
        assert_eq!(m.get("HOME").unwrap(), "/home/x");
    }

    #[test]
    fn parses_newline_with_continuation() {
        let m = parse_env("PATH=/bin\nMULTI=line1\ncontinued\nHOME=/h");
        assert_eq!(m.get("PATH").unwrap(), "/bin");
        assert_eq!(m.get("MULTI").unwrap(), "line1\ncontinued");
        assert_eq!(m.get("HOME").unwrap(), "/h");
    }

    #[test]
    fn real_capture_has_path() {
        // The real user env must expose PATH (falls back to inherited env).
        assert!(user_env().contains_key("PATH"));
    }
}
