# Building from source

Requirements: **Rust 1.92+** and the development packages below.

| Distribution / OS | Packages |
|---|---|
| Debian / Ubuntu | `libgtk-4-dev libadwaita-1-dev libvte-2.91-gtk4-dev libgtksourceview-5-dev libglib2.0-dev-bin pkg-config` |
| Fedora | `gtk4-devel libadwaita-devel vte291-gtk4-devel gtksourceview5-devel glib2-devel` |
| Arch | `gtk4 libadwaita vte4 gtksourceview5 pkgconf` |
| macOS (Homebrew) | `brew install gtk4 libadwaita gtksourceview5 pkg-config` |

```bash
git clone https://github.com/jean1880/nuvek-agent-terminal.git
cd nuvek-agent-terminal
make deps          # Debian/Ubuntu: install the packages above
make start-local   # build (debug) and run
make build         # release build: target/release/agent-terminal
make install       # into ~/.local (binary, icon, desktop entry)
make package       # a .deb, via cargo-deb
```

## Building on macOS

Agent Terminal can be compiled on macOS without Linux-specific components (`vte4` terminal tabs and systemd `journald` logging):

```bash
brew install gtk4 libadwaita gtksourceview5 pkg-config
cargo build --no-default-features --release
```

All chat thread features, multi-agent switching, approvals, diff panels, and git checkpoints work natively.

Development checks (CI runs the same):

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Project structure

- `src/main.rs`: entry point, command-line options, logging, global CSS, shortcuts.
- `src/window/imp.rs`: the window, settings, spawning, terminal pages.
- `src/window/imp/threads.rs`: the thread sidebar, thread pages, terminal drawer.
- `src/window/imp/agents_prefs.rs`, `setup.rs`: Settings → Agents, and the first-start walkthrough.
- `src/chat/`: the chat thread: `session.rs` (adapter, process, store, switching,
  hand-off) and `view/` (transcript, composer, panels, model picker).
- `src/palette.rs`, `src/theme.rs`: app-wide colours (and the system theme), terminal palettes.
- `src/config.rs`: settings, migration and validation.
- `src/agent_proc.rs`, `src/approval_server.rs`, `src/approval_hook.rs`: the agent process
  transport and agy's approval socket and hook.
- `crates/agent-core`: pure logic, with no GTK or I/O: events, the Claude/agy/Codex
  adapters, policies, redaction.
- `crates/agent-kit`: GTK-free I/O: the thread store, git checkpoints, diffs, restore,
  worktrees, hand-off briefs, session readers.

