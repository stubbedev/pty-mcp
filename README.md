# pty-mcp

A low-footprint [MCP](https://modelcontextprotocol.io) server that gives an AI
agent two things:

1. **Persistent interactive PTY sessions** — a real pseudo-terminal with full
   VT/xterm emulation, so the agent can drive REPLs, `ssh`, `vim`, `htop`,
   password prompts, and anything else that needs a live terminal instead of
   one-shot `bash` calls.
2. **Passwordless `sudo`** — run privileged commands where the password is
   entered by the *user* in a native OS dialog and never touches the MCP
   transport or the model's context.

It replaces [ptyai](https://github.com/xdrr/ptyai) (Node) and
[sudo-mcp](https://github.com/0xMH/sudo-mcp) with a single static Rust binary.
Idle RSS is ~5 MB (vs. ~50–100 MB for a Node runtime); terminal emulation is
handled by [`alacritty_terminal`](https://crates.io/crates/alacritty_terminal),
the same engine Alacritty ships.

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

`run` and every PTY session execute in the user's **own shell and environment**:
`PATH` matches their interactive shell (nix, cargo, custom bins), `HOME` is the
default cwd. The environment is captured once at startup by sourcing the user's
login+interactive shell, so it's correct even when pty-mcp runs over HTTP, from
a proxy, or under systemd where the process's own env is stripped.

## How `sudo` keeps the password out of context

Any `sudo` in a `run` command or a PTY session transparently uses an OS password
dialog — never the transport or the model's context. A `sudo` wrapper is
prepended to `PATH` that adds `-A` (askpass) for real command execution while
leaving management flags (`-k`/`-v`/`-l`) alone, with `SUDO_ASKPASS` pointing at
this binary's hidden `askpass` subcommand. When sudo needs a password the helper
prompts and prints it straight to sudo. It is never sent over the transport,
stored, or shown to the model. (`--sudo-keepalive` keeps one entry valid for the
whole session.)

### Choosing the prompt

With no `--askpass`, the helper autodetects an ssh-askpass-style program
(`ksshaskpass`, `ssh-askpass`, …), then `kdialog`, then `zenity` (last resort).

For a nicer prompt, pass any launcher that prints the typed password to stdout —
the prompt text is available to it as `$PTY_MCP_PROMPT`:

```sh
# gtk-layer-shell (no Qt):
pty-mcp --askpass 'wofi --dmenu --password --prompt sudo </dev/null'
# layer-shell:
pty-mcp --askpass 'fuzzel --dmenu --password --prompt "sudo: "'
# rofi:
pty-mcp --askpass 'rofi -dmenu -password -p sudo'
```

Note: there is no xdg-desktop-portal interface for prompting a password (the
`Secret` portal fetches an app's stored keyring secret; it does not prompt), so
a portal-based prompt isn't possible — use a layer-shell launcher via
`--askpass` instead.

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
