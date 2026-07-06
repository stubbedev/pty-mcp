# pty-mcp

A low-footprint [MCP](https://modelcontextprotocol.io) server that lets an AI
agent use a real terminal:

- **`run`** a shell command exactly as the user would — their shell, full
  environment, and cwd.
- **`pty_*`** drive a persistent pseudo-terminal with full VT/xterm emulation,
  for anything interactive: REPLs, `ssh`, `vim`, `htop`, prompts.
- **`sudo`** works transparently — the password is typed by the user into a
  native OS dialog and never touches the MCP transport or the model's context.

A single static Rust binary, ~5 MB idle RSS. Terminal emulation uses
[`alacritty_terminal`](https://crates.io/crates/alacritty_terminal), the same
engine Alacritty ships.

Linux and macOS. No Windows/ConPTY yet.

## Install

```sh
brew install stubbedev/pty-mcp/pty-mcp     # homebrew tap
# or
nix run github:stubbedev/pty-mcp           # nix flake
# or
cargo install --git https://github.com/stubbedev/pty-mcp
```

## Run

```sh
pty-mcp                       # stdio (default) — for Claude Code / editors
pty-mcp --http 127.0.0.1:8722 # streamable HTTP
```

Options:
- `--idle-timeout <secs>` — kill sessions idle longer than this (default 1800, 0 disables).
- `--scrollback <lines>` — default scrollback per session (default 1000).
- `--max-sessions <n>` — cap on concurrent sessions; opening past it evicts the oldest (default 50).
- `--askpass <CMD>` — command used to prompt for the sudo password (see below).
- `--sudo-keepalive` — after the first sudo auth, keep the credential warm for
  the whole session so later `sudo` commands skip the prompt. Off by default;
  grants passwordless root for as long as the server runs.

Register with Claude Code:

```sh
claude mcp add pty-mcp -- pty-mcp
```

## Tools

| Tool | What it does |
|------|--------------|
| `run` | Run a one-shot shell command as the user (their shell, env, cwd); `sudo` auto-uses the dialog. |
| `pty_open` | Start a session (shell/cwd/size/env), returns a `session_id`. |
| `pty_write` | Write text to the PTY; optionally wait for a regex; returns the screen. |
| `pty_sendkey` | Send named keys: `enter`, arrows, `f1`–`f12`, `ctrl+<letter>`, … |
| `pty_read` | Rendered screen + cursor + alt-screen flag + real exit code. |
| `pty_wait` | Block until a regex matches, or until output goes quiet. |
| `pty_resize` | Resize the terminal (SIGWINCH). |
| `pty_close` | Terminate a session. |
| `pty_list` | List active sessions with age/idle. |

Use `run` for one-shot commands ("build the project", "install a package"), and
`pty_open` + friends when you need to *drive* something interactive (a REPL,
`vim`, `ssh`, a prompt).

## Runs as the user

`run` and every PTY session execute in the user's own shell and environment:
`PATH` matches their interactive shell (nix, cargo, custom bins), `HOME` is the
default cwd. The environment is captured once at startup by sourcing the login
shell, so it's correct even when pty-mcp runs over HTTP, behind a proxy, or under
systemd — where the process's own environment would otherwise be stripped.

## Sudo

`sudo` in any command or session prompts the user through a native OS dialog; the
password goes straight to sudo and is never sent over the transport, stored, or
shown to the model. `--sudo-keepalive` keeps one entry valid for the session.

The prompt is pluggable via `--askpass <CMD>` — any launcher that prints the
password to stdout (prompt text is in `$PTY_MCP_PROMPT`):

```sh
pty-mcp --askpass 'wofi --dmenu --password --prompt sudo </dev/null'
pty-mcp --askpass 'fuzzel --dmenu --password --prompt "sudo: "'
pty-mcp --askpass 'rofi -dmenu -password -p sudo'
```

Without `--askpass`, it autodetects an ssh-askpass-style helper (`ksshaskpass`,
`ssh-askpass`, …), then `kdialog`, then `zenity`.

## Development

```sh
just build        # release binary → ./bin/
just test         # cargo test
just lint         # fmt + clippy -D warnings
just check        # lint + test + flake sync
just release-patch / release-minor / release-major
```

Releases are tag-driven (`vX.Y.Z`): GitHub Actions builds static musl + macOS
binaries, publishes a GitHub release, pushes the nix closure to the attic cache,
and bumps the homebrew tap formula.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE).
