//! `SUDO_ASKPASS` helper. When `sudo -A` needs a password it runs this binary
//! in `askpass` mode; we pop a native OS dialog and print the entered password
//! to stdout. The password therefore lives only in the dialog → this process →
//! sudo — never in the MCP transport or the model's context.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};

/// Write (once per process) a 0700 wrapper script that `sudo -A` will run as
/// `SUDO_ASKPASS`. `sudo` invokes the askpass program with no useful args, so
/// we need a small shell shim that re-execs this binary in `askpass` mode.
/// Returns the script path; both `sudo_run` and interactive PTY sessions point
/// `SUDO_ASKPASS` at it.
pub fn ensure_script() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("resolve current exe")?;
    let path = std::env::temp_dir().join(format!("pty-mcp-askpass-{}.sh", std::process::id()));
    if !path.exists() {
        let body = format!("#!/bin/sh\nexec {:?} askpass \"$@\"\n", exe);
        std::fs::write(&path, body).context("write askpass script")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                .context("chmod askpass script")?;
        }
    }
    Ok(path)
}

/// Run the askpass flow: try each available GUI prompt in turn, print the
/// password to stdout. Exit non-zero if the user cancels or no prompt exists.
pub fn run(prompt: &str) -> ! {
    match ask(prompt) {
        Some(pw) => {
            print!("{pw}");
            std::process::exit(0);
        }
        None => std::process::exit(1),
    }
}

/// Try each backend in order; first one present on the system wins.
fn ask(prompt: &str) -> Option<String> {
    for backend in BACKENDS {
        if which(backend.bin) {
            if let Some(pw) = (backend.run)(prompt) {
                return Some(pw);
            }
            // Present but cancelled/failed — respect that, don't fall through.
            return None;
        }
    }
    None
}

struct Backend {
    bin: &'static str,
    run: fn(&str) -> Option<String>,
}

// Order: macOS native first, then the common Linux dialog tools.
const BACKENDS: &[Backend] = &[
    Backend {
        bin: "osascript",
        run: osascript,
    },
    Backend {
        bin: "zenity",
        run: zenity,
    },
    Backend {
        bin: "kdialog",
        run: kdialog,
    },
];

fn which(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(bin).is_file()))
        .unwrap_or(false)
}

fn capture(mut cmd: Command) -> Option<String> {
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    // Strip the single trailing newline the tools append.
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    if s.ends_with('\n') {
        s.pop();
        if s.ends_with('\r') {
            s.pop();
        }
    }
    Some(s)
}

fn osascript(prompt: &str) -> Option<String> {
    // AppleScript with hidden answer; returns the text or errors on cancel.
    let script = format!(
        "display dialog \"{}\" default answer \"\" with hidden answer with title \"pty-mcp sudo\"; text returned of result",
        prompt.replace('\\', "\\\\").replace('"', "\\\"")
    );
    let mut c = Command::new("osascript");
    c.arg("-e").arg(script);
    capture(c)
}

fn zenity(prompt: &str) -> Option<String> {
    let mut c = Command::new("zenity");
    c.arg("--password").arg("--title=pty-mcp sudo");
    let _ = prompt; // zenity --password has no custom text field
    capture(c)
}

fn kdialog(prompt: &str) -> Option<String> {
    let mut c = Command::new("kdialog");
    c.arg("--password")
        .arg(prompt)
        .arg("--title")
        .arg("pty-mcp sudo");
    capture(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_finds_sh() {
        assert!(which("sh"));
        assert!(!which("definitely-not-a-real-binary-xyz"));
    }

    #[test]
    fn capture_trims_newline() {
        let mut c = Command::new("printf");
        c.arg("secret\\n");
        assert_eq!(capture(c), Some("secret".to_string()));
    }

    #[test]
    fn capture_fails_on_nonzero() {
        let c = Command::new("false");
        assert_eq!(capture(c), None);
    }
}
