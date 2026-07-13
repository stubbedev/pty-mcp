//! Human takeover: `pty-mcp attach <session-id>` bridges a real terminal into
//! a live PTY session, so the user can type a password, drive vim, or fix
//! something by hand — then Ctrl+] detaches and hands control back to the
//! agent. Out-of-band by design: it works identically under every harness
//! because it never touches the harness at all.
//!
//! Wire form: one unix socket per server process in a private runtime dir.
//! The client scans the dir, asks each live server `ATTACH <id>`, and the
//! first `OK` wins. After the handshake it's a raw byte bridge: client stdin →
//! PTY input, PTY output (teed by the session reader) → client stdout.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tokio::net::UnixListener;

use crate::session::SessionManager;

/// Byte the attach client treats as "detach" (Ctrl+], as in telnet).
const DETACH: u8 = 0x1d;

/// Directory holding one `<pid>.sock` per running server.
fn sock_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("pty-mcp")
}

/// The attach command an agent should surface to the user for a session.
pub fn attach_command(session_id: &str) -> String {
    format!("pty-mcp attach {session_id}")
}

// ─────────────────────────────── server side ───────────────────────────────

/// Start the attach listener for this server process. Failure is logged, not
/// fatal — the server is fully functional without takeover.
pub fn spawn_listener(mgr: Arc<SessionManager>) {
    let dir = sock_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return tracing::warn!(error = %e, "attach: cannot create socket dir");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    clean_stale(&dir);

    let path = dir.join(format!("{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => return tracing::warn!(error = %e, "attach: cannot bind {}", path.display()),
    };
    tracing::info!("attach socket at {}", path.display());

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            // Each attached human gets one blocking task: the handshake and
            // the input pump are simple blocking IO, and attaches are rare.
            let Ok(std_stream) = stream.into_std() else {
                continue;
            };
            let mgr = Arc::clone(&mgr);
            tokio::task::spawn_blocking(move || {
                let _ = std_stream.set_nonblocking(false);
                let _ = handle_client(std_stream, mgr);
            });
        }
    });
}

/// Handshake, then pump client keystrokes into the PTY. Output flows the other
/// way through the session's tap (a clone of this socket registered with the
/// reader thread).
fn handle_client(mut stream: UnixStream, mgr: Arc<SessionManager>) -> Result<()> {
    let line = read_line(&mut stream)?;
    let Some(id) = line
        .trim()
        .strip_prefix("ATTACH ")
        .filter(|s| !s.is_empty())
    else {
        stream.write_all(b"ERR bad handshake\n")?;
        return Ok(());
    };
    let Ok(session) = mgr.get(id) else {
        stream.write_all(b"ERR no such session\n")?;
        return Ok(());
    };
    stream.write_all(b"OK\n")?;
    tracing::info!(session = id, "human attached");

    session.attach_tap(stream.try_clone().context("clone socket for tap")?);
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if session.write(&buf[..n]).is_err() {
                    break;
                }
            }
        }
    }
    tracing::info!(session = id, "human detached");
    Ok(())
}

/// Read one `\n`-terminated line, byte-at-a-time so no post-handshake input is
/// buffered away from the pump loop.
fn read_line(stream: &mut UnixStream) -> Result<String> {
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    while line.len() < 256 {
        if stream.read(&mut b)? == 0 || b[0] == b'\n' {
            break;
        }
        line.push(b[0]);
    }
    Ok(String::from_utf8_lossy(&line).into_owned())
}

// ─────────────────────────────── client side ───────────────────────────────

/// Restores the local terminal on drop, even on panic.
struct RawGuard(libc::termios);

impl RawGuard {
    fn enter() -> Option<RawGuard> {
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut t) != 0 {
                return None; // not a terminal (piped stdin) — run cooked
            }
            let orig = t;
            libc::cfmakeraw(&mut t);
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &t);
            Some(RawGuard(orig))
        }
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.0) };
    }
}

/// `pty-mcp attach <id>`: find the server holding the session, bridge this
/// terminal into it raw, detach with Ctrl+].
pub fn run_client(session_id: &str) -> Result<()> {
    let mut stream = connect(session_id)?;
    eprintln!("[attached to {session_id} — Ctrl+] to detach]");
    let guard = RawGuard::enter();

    // Keystrokes → socket, on its own thread; Ctrl+] stops everything.
    let mut input_sock = stream.try_clone().context("clone socket")?;
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 1024];
        while let Ok(n) = stdin.read(&mut buf) {
            if n == 0 {
                break;
            }
            if let Some(i) = buf[..n].iter().position(|&b| b == DETACH) {
                let _ = input_sock.write_all(&buf[..i]);
                // Shut the socket down; the output loop unblocks and exits.
                let _ = input_sock.shutdown(std::net::Shutdown::Both);
                break;
            }
            if input_sock.write_all(&buf[..n]).is_err() {
                break;
            }
        }
    });

    // Socket → this terminal, until detach or the session ends.
    let mut stdout = std::io::stdout().lock();
    let mut buf = [0u8; 8192];
    loop {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if stdout.write_all(&buf[..n]).is_err() {
                    break;
                }
                let _ = stdout.flush();
            }
        }
    }
    drop(guard);
    eprintln!("\n[detached from {session_id}]");
    Ok(())
}

/// Scan the socket dir for live servers and ask each for the session.
fn connect(session_id: &str) -> Result<UnixStream> {
    let dir = sock_dir();
    let entries = std::fs::read_dir(&dir)
        .with_context(|| format!("no pty-mcp servers found ({} missing)", dir.display()))?;
    let mut tried = 0;
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().is_none_or(|x| x != "sock") {
            continue;
        }
        let Ok(mut s) = UnixStream::connect(&path) else {
            let _ = std::fs::remove_file(&path); // stale
            continue;
        };
        tried += 1;
        if s.write_all(format!("ATTACH {session_id}\n").as_bytes())
            .is_err()
        {
            continue;
        }
        match read_line(&mut s)?.as_str() {
            "OK" => return Ok(s),
            _ => continue,
        }
    }
    bail!(
        "session {session_id:?} not found on any running pty-mcp server ({tried} server(s) checked). \
         List sessions with the pty_list tool."
    )
}

/// Remove sockets whose owning server process is gone.
fn clean_stale(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.strip_suffix(".sock"))
            .and_then(|p| p.parse::<i32>().ok())
        else {
            continue;
        };
        if pid == std::process::id() as i32 {
            continue;
        }
        let dead = unsafe { libc::kill(pid, 0) } != 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        if dead {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::OpenParams;
    use std::time::Duration;

    fn open_session(mgr: &SessionManager) -> Arc<crate::session::Session> {
        mgr.open(OpenParams {
            shell: Some("/bin/sh".into()),
            cwd: None,
            cols: 80,
            rows: 24,
            env: vec![],
            scrollback: 200,
        })
        .unwrap()
        .session
    }

    /// Full takeover flow against a real session: handshake, initial redraw,
    /// human types a command, output is teed back to the attached socket.
    #[tokio::test]
    async fn attach_handshake_and_takeover() {
        let mgr = SessionManager::new(Duration::from_secs(600), 1000, 50, None);
        let s = open_session(&mgr);

        // Server end: a socketpair stands in for the accepted connection.
        let (client, server) = UnixStream::pair().unwrap();
        let mgr2 = Arc::clone(&mgr);
        let handler = tokio::task::spawn_blocking(move || handle_client(server, mgr2));

        let mut c = client;
        c.write_all(format!("ATTACH {}\n", s.id).as_bytes())
            .unwrap();
        assert_eq!(read_line(&mut c).unwrap(), "OK");

        // Human types a command through the bridge…
        c.write_all(b"printf takeover-works\r\n").unwrap();
        // …and sees the result teed back on the same socket.
        let re = regex::Regex::new("takeover-works").unwrap();
        assert!(
            s.wait(Some(&re), Duration::ZERO, Duration::from_secs(5))
                .await
        );
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut seen = String::new();
        let mut buf = [0u8; 4096];
        while !seen.contains("takeover-works") {
            let n = c.read(&mut buf).expect("teed output");
            assert!(n > 0, "socket closed before output arrived");
            seen.push_str(&String::from_utf8_lossy(&buf[..n]));
        }

        drop(c); // detach → handler's pump loop ends
        handler.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn attach_unknown_session_errors() {
        let mgr = SessionManager::new(Duration::from_secs(600), 1000, 50, None);
        let (mut client, server) = UnixStream::pair().unwrap();
        let h = tokio::task::spawn_blocking(move || handle_client(server, mgr));
        client.write_all(b"ATTACH pty-nope\n").unwrap();
        let resp = read_line(&mut client).unwrap();
        assert!(resp.starts_with("ERR"), "{resp}");
        h.await.unwrap().unwrap();
    }
}
