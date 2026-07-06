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

Options: `--idle-timeout <secs>` (default 1800, 0 disables the reaper),
`--scrollback <lines>` (default 1000).

Register with Claude Code:

```sh
claude mcp add pty-mcp -- pty-mcp
```

## Tools

| Tool | What it does |
|------|--------------|
| `pty_open` | Start a session (shell/cwd/size/env), returns a `session_id`. |
| `pty_write` | Write text to the PTY; optionally wait for a regex; returns the screen. |
| `pty_sendkey` | Send named keys: `enter`, arrows, `f1`–`f12`, `ctrl+<letter>`, … |
| `pty_read` | Rendered screen + cursor + alt-screen flag + exit status. |
| `pty_wait` | Block until a regex matches, or until output goes quiet. |
| `pty_resize` | Resize the terminal (SIGWINCH). |
| `pty_close` | Terminate a session. |
| `pty_list` | List active sessions with age/idle. |
| `sudo_run` | Run an argv (no shell) under `sudo`; password via OS dialog. |

## How `sudo_run` keeps the password out of context

`sudo_run` runs `sudo -A` with `SUDO_ASKPASS` pointed at this binary's hidden
`askpass` subcommand. When sudo needs a password it launches that helper, which
pops a native dialog (`osascript` on macOS; `zenity`/`kdialog` on Linux). The
password flows dialog → helper → sudo. It is never sent over the MCP transport,
never stored, and never enters the model's context. The command is passed as an
argv list and executed without a shell, so there is no shell-injection surface.

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
