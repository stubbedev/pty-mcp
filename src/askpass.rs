//! Sudo password prompting without the password touching the model.
//!
//! We install a private 0700 helper dir containing two shims:
//!   * `askpass.sh` — what `sudo -A` runs as `SUDO_ASKPASS`; it re-execs this
//!     binary in `askpass` mode, which prompts and prints the password.
//!   * `sudo`       — a wrapper prepended to `PATH` that adds `-A` for real
//!     command execution (so any `sudo` in a `run` command or PTY session
//!     transparently uses the dialog) while leaving management flags
//!     (`-k`/`-v`/`-l`/…) alone. It calls the real sudo by absolute path.
//!
//! The prompt program is pluggable via `--askpass "<command>"` (baked into the
//! askpass shim as `$PTY_MCP_ASKPASS`); the command prints the typed password
//! to stdout and sees the prompt in `$PTY_MCP_PROMPT`. With no override we
//! autodetect an ssh-askpass-style helper, then kdialog, then zenity.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use anyhow::{Context, Result};

const OVERRIDE_ENV: &str = "PTY_MCP_ASKPASS";
const PROMPT_ENV: &str = "PTY_MCP_PROMPT";

/// Paths to the installed helpers plus the env additions to apply.
pub struct Helpers {
    /// Directory to prepend to `PATH` (holds the `sudo` wrapper).
    pub path_dir: PathBuf,
    /// Value for `SUDO_ASKPASS`.
    pub askpass: PathBuf,
}

static HELPERS: OnceLock<Option<Helpers>> = OnceLock::new();

/// Install the helper dir once and return it. `askpass_override` is baked in on
/// first call (subsequent calls ignore a changed value — one config per run).
pub fn helpers(askpass_override: Option<&str>) -> Option<&'static Helpers> {
    HELPERS
        .get_or_init(|| install(askpass_override).ok())
        .as_ref()
}

/// Apply the sudo helpers to a command's environment: prepend the wrapper dir
/// to `PATH` and set `SUDO_ASKPASS`. `base_path` is the user's existing PATH.
pub fn apply_to_env(base_path: &str, askpass_override: Option<&str>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(h) = helpers(askpass_override) {
        out.push((
            "PATH".to_string(),
            format!("{}:{}", h.path_dir.display(), base_path),
        ));
        out.push(("SUDO_ASKPASS".to_string(), h.askpass.display().to_string()));
    }
    out
}

fn install(askpass_override: Option<&str>) -> Result<Helpers> {
    let exe = std::env::current_exe().context("resolve current exe")?;
    // The dir is per-pid and can't be removed on our own exit (kill -9), so
    // sweep predecessors' leftovers now instead.
    clean_stale(&std::env::temp_dir());
    // A private per-process dir; creating it 0700 keeps a local attacker from
    // pre-planting symlinks the shims would follow.
    let dir = std::env::temp_dir().join(format!("pty-mcp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).context("create helper dir")?;
    set_mode(&dir, 0o700)?;

    // askpass.sh
    let askpass = dir.join("askpass.sh");
    let mut body = String::from("#!/bin/sh\n");
    if let Some(h) = askpass_override {
        body.push_str(&format!(
            "{OVERRIDE_ENV}='{}'\nexport {OVERRIDE_ENV}\n",
            h.replace('\'', "'\\''")
        ));
    }
    body.push_str(&format!("exec {exe:?} askpass \"$@\"\n"));
    write_exec(&askpass, &body)?;

    // sudo wrapper — only if a real sudo exists on the current PATH.
    if let Some(real) = which_abs("sudo") {
        let sudo = dir.join("sudo");
        // Add -A unless the args are a pure management op or already ask-passing.
        let body = format!(
            "#!/bin/sh\nadd=1\nfor a in \"$@\"; do\n  case \"$a\" in\n    -A|--askpass|-k|--remove-timestamp|-K|--reset-timestamp|-v|--validate|-l|--list|-h|--help|-V|--version) add=0;;\n  esac\ndone\nif [ \"$add\" = 1 ]; then exec {real:?} -A \"$@\"; else exec {real:?} \"$@\"; fi\n"
        );
        write_exec(&sudo, &body)?;
    }

    Ok(Helpers {
        path_dir: dir,
        askpass,
    })
}

/// Remove `pty-mcp-<pid>` helper dirs left by processes that no longer exist.
/// Best-effort: other users' dirs fail to remove and are skipped silently.
fn clean_stale(tmp: &Path) {
    let Ok(entries) = std::fs::read_dir(tmp) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.strip_prefix("pty-mcp-"))
            .and_then(|p| p.parse::<i32>().ok())
        else {
            continue;
        };
        if pid == std::process::id() as i32 {
            continue;
        }
        // Signal 0 probes liveness: ESRCH → the owning process is gone.
        let dead = unsafe { libc::kill(pid, 0) } != 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        if dead {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

fn write_exec(path: &Path, body: &str) -> Result<()> {
    std::fs::write(path, body).with_context(|| format!("write {}", path.display()))?;
    set_mode(path, 0o700)
}

fn set_mode(_path: &Path, _mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(_path, std::fs::Permissions::from_mode(_mode))
            .with_context(|| format!("chmod {}", _path.display()))?;
    }
    Ok(())
}

/// Absolute path of `bin` on the current PATH, excluding our own helper dir
/// (so the sudo wrapper never resolves to itself).
fn which_abs(bin: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        if dir
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("pty-mcp-"))
        {
            continue;
        }
        let p = dir.join(bin);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

// ─────────────────────────── askpass mode ───────────────────────────

/// Run the askpass flow: prompt for the password, print it to stdout. Exit
/// non-zero if the user cancels or no prompt is available.
pub fn run(prompt: &str) -> ! {
    let pw = match std::env::var(OVERRIDE_ENV) {
        Ok(cmd) if !cmd.is_empty() => run_command(&cmd, prompt),
        _ => autodetect(prompt),
    };
    match pw {
        Some(pw) => {
            print!("{pw}");
            std::process::exit(0);
        }
        None => std::process::exit(1),
    }
}

fn run_command(cmd: &str, prompt: &str) -> Option<String> {
    let mut c = Command::new("sh");
    c.arg("-c").arg(cmd).env(PROMPT_ENV, prompt);
    capture(c)
}

fn autodetect(prompt: &str) -> Option<String> {
    for backend in BACKENDS {
        if which(backend.bin) {
            return (backend.run)(backend.bin, prompt);
        }
    }
    None
}

struct Backend {
    bin: &'static str,
    run: fn(&str, &str) -> Option<String>,
}

// ssh-askpass-style helpers first (prompt as argv[1], password on stdout), then
// macOS native, then desktop dialogs. zenity last — ugliest, least
// configurable. For a nicer prompt use `--askpass` with your own launcher.
const BACKENDS: &[Backend] = &[
    Backend {
        bin: "ksshaskpass",
        run: ssh_style,
    },
    Backend {
        bin: "ssh-askpass-gnome",
        run: ssh_style,
    },
    Backend {
        bin: "lxqt-openssh-askpass",
        run: ssh_style,
    },
    Backend {
        bin: "ssh-askpass",
        run: ssh_style,
    },
    Backend {
        bin: "x11-ssh-askpass",
        run: ssh_style,
    },
    Backend {
        bin: "osascript",
        run: osascript,
    },
    Backend {
        bin: "kdialog",
        run: kdialog,
    },
    Backend {
        bin: "zenity",
        run: zenity,
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
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    if s.ends_with('\n') {
        s.pop();
        if s.ends_with('\r') {
            s.pop();
        }
    }
    Some(s)
}

fn ssh_style(bin: &str, prompt: &str) -> Option<String> {
    let mut c = Command::new(bin);
    c.arg(prompt);
    capture(c)
}

fn osascript(_bin: &str, prompt: &str) -> Option<String> {
    let script = format!(
        "display dialog \"{}\" default answer \"\" with hidden answer with title \"pty-mcp sudo\"; text returned of result",
        prompt.replace('\\', "\\\\").replace('"', "\\\"")
    );
    let mut c = Command::new("osascript");
    c.arg("-e").arg(script);
    capture(c)
}

fn zenity(_bin: &str, _prompt: &str) -> Option<String> {
    let mut c = Command::new("zenity");
    c.arg("--password").arg("--title=pty-mcp sudo");
    capture(c)
}

fn kdialog(_bin: &str, prompt: &str) -> Option<String> {
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

    #[test]
    fn override_command_via_sh() {
        assert_eq!(
            run_command("printf hunter2", "prompt"),
            Some("hunter2".into())
        );
    }

    #[test]
    fn override_sees_prompt_env() {
        assert_eq!(
            run_command("printf %s \"$PTY_MCP_PROMPT\"", "sudo pw"),
            Some("sudo pw".into())
        );
    }

    #[test]
    fn clean_stale_removes_dead_pid_dirs_only() {
        let tmp = std::env::temp_dir().join("pty-mcp-clean-test");
        std::fs::create_dir_all(&tmp).unwrap();
        // A dir whose pid can't be alive (> pid_max on default Linux).
        let dead = tmp.join("pty-mcp-4999999");
        // Our own pid → must survive. Plus a non-matching name.
        let ours = tmp.join(format!("pty-mcp-{}", std::process::id()));
        let other = tmp.join("unrelated");
        for d in [&dead, &ours, &other] {
            std::fs::create_dir_all(d).unwrap();
        }
        clean_stale(&tmp);
        assert!(!dead.exists(), "dead-pid dir removed");
        assert!(ours.exists(), "own dir kept");
        assert!(other.exists(), "non-matching dir untouched");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn installs_helpers_with_wrapper() {
        // ensure_script-equivalent: helper dir exists, askpass shim present, and
        // (since sudo exists here) the sudo wrapper is installed and adds -A.
        let h = helpers(None).expect("helpers install");
        assert!(h.askpass.is_file());
        let sudo = h.path_dir.join("sudo");
        assert!(sudo.is_file(), "sudo wrapper should be installed");
        let body = std::fs::read_to_string(&sudo).unwrap();
        assert!(body.contains("-A"), "wrapper adds -A");
    }
}
