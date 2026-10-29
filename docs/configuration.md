# Configuration and options

## Command-line options

```text
agent-terminal [OPTIONS]
```

| Option | Meaning |
|---|---|
| `-r`, `--resume SESSION_ID` | Open a thread resuming that Claude or agy session, in the folder it was recorded in. |
| `-d`, `--dir DIR` | With `--resume`: resume in `DIR` instead of looking the folder up. |
| `-h`, `--help` | Show the options (`--help-all` adds GTK's own). |
| `--uninstall [--yes \| --keep-data]` | Remove Agent Terminal, asking whether to remove your configuration and data too. Must come first; see [Uninstalling](install.md#uninstalling). |
| `--approval-hook` | Internal: agy's approval hook runs this. Not for direct use. |
| `--chat-demo` | Open a window showing a scripted chat, with no agent running. |

To see the whole window with demo threads instead of yours (for screenshots), set
`AGENT_TERMINAL_DEMO_THREADS=1`. It keeps the threads in memory, starts no agent, and leaves
your real thread store alone.

Agent Terminal runs as a single instance. A second launch hands its request to the
window that is already open and exits, so `agent-terminal --resume <id>` opens the
thread in your existing window. A bad ID or folder is reported on the terminal you
ran it from, with exit status 2.

```bash
agent-terminal --resume 215bf7f7-88e4-4070-b41b-332500303534
agent-terminal --resume <id> --dir ~/git/some-project
```

### Environment variables

| Variable | Effect |
|---|---|
| `RUST_LOG` | Log level and filters (default `info`), e.g. `debug` or `agent_terminal::window=debug`. |
| `XDG_CONFIG_HOME` | Where `agent-terminal/config.json` lives (default `~/.config`). Point it at a scratch folder to run an isolated copy. |
| `XDG_STATE_HOME` | Where the thread store, hand-off briefs and approval rules live (default `~/.local/state`). |
| `XDG_CACHE_HOME` | Where the model catalogue is cached (default `~/.cache`). |
| `XDG_RUNTIME_DIR` | Private runtime files: agy's approval socket, diff temp files, the delegation socket. Set by your login session; without it agy's approval hook and delegation are unavailable. |
| `AGENT_TERMINAL_DEMO_THREADS` | Set to `1` for demo threads in place of yours (in memory, no agents run). |
| `AGENT_TERMINAL_DEMO_SCENE` | Opens a demo on one scene. With `--chat-demo`: `diff`, `workers`, `usage`, `queue`, `requests` or `requests-risky`. With `AGENT_TERMINAL_DEMO_THREADS=1`: `closing` (the closing screen). |
| `AGENT_TERMINAL_DEMO_CODEX_WORKERS` | With `--chat-demo`, `1` plays the Codex sub-agent script. |
| `AGENT_TERMINAL_DEMO_STRESS` | With `--chat-demo`, preloads that many transcript items (a long-thread stress test). |

The app sets these itself for the agent processes it starts. Never set them yourself:
`AGENT_TERMINAL_HOOK_BIN` (the binary agy's approval hook runs, taken from the running app),
`AGENT_TERMINAL_APPROVAL_SOCKET`, and the `AGENT_TERMINAL_DELEGATION_*` variables.

The test runs have their own variables; see [`tests/qa/README.md`](../tests/qa/README.md#environment-variables)
and [`.env.example`](../.env.example).

## Keyboard shortcuts

| Keys | Action |
|---|---|
| `Ctrl+Shift+T` | New thread |
| `Ctrl+Shift+G` | New thread in a new worktree |
| `Ctrl+Shift+W` | Close the thread |
| `Ctrl+Shift+R` | Restart the thread's agent on the same conversation (☰ → Restart Agent) |
| `Ctrl+Shift+O` | New thread in a chosen folder |
| `Ctrl+Shift+E` | Resume an existing session |
| `Ctrl+Shift+S` | Take a checkpoint now |
| `F9`, `Ctrl+Shift+B` | Show or hide the sidebar |
| `` Ctrl+` ``, `Ctrl+J` | Show or hide the terminal drawer |
| `Ctrl+Shift+D` | Show or hide the diff panel |
| `Ctrl+Shift+F` | Search a terminal's scrollback |
| `Ctrl+Shift+C` / `Ctrl+Shift+V` | Copy / paste |
| `Ctrl+,` | Open Settings |
| `Ctrl+?` | Show keyboard shortcuts |
| `Ctrl+Tab`, `Ctrl+Page Down` | Next thread |
| `Ctrl+Shift+Tab`, `Ctrl+Page Up` | Previous thread |
| `Alt+1`…`Alt+8`, `Alt+9` | Go to thread N, or the last one |
| `Ctrl+Alt+1`…`Ctrl+Alt+9` | New terminal thread with the Nth profile, in the current folder |
| `Ctrl+Plus` / `Ctrl+Minus` / `Ctrl+0` | Chat and terminal text zoom in / out / reset (saved); `Ctrl+=` also zooms in |
| `Ctrl+click` | Open a link in a terminal |
| `Ctrl+Shift+A` | Open or close every sub-agent card in the thread |

These are caught before a terminal sees them, so a CLI never receives them.

While an approval is waiting in the focused thread:

| Keys | Answer |
|---|---|
| `Ctrl+Enter` | Allow |
| `Ctrl+Shift+Enter` | Allow for session |
| `Ctrl+Alt+Enter` | Always allow |
| `Ctrl+Backspace` | Deny (Stop turn where the card has no Deny) |
| `Ctrl+.` | Stop turn |

A key the card does not offer does its usual job, and a Codex permission request is never
allowed from the keyboard.

## Configuration

Everything in Settings is saved to `~/.config/agent-terminal/config.json`. A few things
can only be set there: profiles, status indicators and the cleared environment.

| File | What |
|---|---|
| `~/.config/agent-terminal/config.json` | Settings, profiles, indicators. Written atomically, `0600`. |
| `~/.local/state/agent-terminal/threads.db` | The thread store (SQLite). Every stored event is redacted. |
| `~/.local/state/agent-terminal/handoffs/` | Hand-off briefs: `0700` folder, `0600` files, pruned after 7 days. |
| `~/.local/state/agent-terminal/approval-rules.json` | Approval rules (session and always) for every agent. Written `0600`. |

A settings file that can't be parsed is copied aside (`config.json.invalid-…`) and
reported before defaults replace it. An edit that doesn't parse yet suspends saving
until it is fixed.

### Settings reference

| Key | Default | Meaning |
|---|---|---|
| `theme` | `"agent-terminal"` | `system`, `agent-terminal`, `dracula`, `nord`, `gruvbox-dark`, `solarized-dark`, `one-dark` or `monokai` |
| `starting_directory` | `""` | Where new threads open (blank: your home folder) |
| `default_agent` | none | `claude`, `agy` or `codex` for new threads (none: the first one installed) |
| `profiles` | Claude, Agy, Codex | The CLIs on offer. See [Profiles](#profiles) |
| `default_profile` | none | The profile terminal threads run (none: the first one installed) |
| `checkpoints` | `true` | Snapshot each turn in a git repository |
| `worktree_root` | `""` | Where worktrees go (blank: a hidden folder beside the repository) |
| `diff_tool` | none | The external diff tool for **Open in…** |
| `diffs_expanded` | `false` | Open a file-change card's diff as soon as the edit lands |
| `notify_on_bell` | `false` | Notify when a background thread finishes |
| `notify_on_quota` | `true` | Notify, with a hand-off button, when an agent runs out of quota |
| `reopen_last_thread` | `false` | Start with the threads open at the last close |
| `restore_session` | `false` | Reopen the last terminal threads, as fresh sessions |
| `skip_load_animation` | `false` | Show the window as soon as it is ready |
| `pattern_auto_approval` | `true` | Answer requests that match a saved approval rule without asking |
| `confirm_rule_modification` | `true` | Confirm, or loosen, a rule's pattern when choosing Always allow |
| `permission_sources` | `[]` | Extra `permissions.toml`-style files that import reads and export writes (absolute or `~/`) |
| `suggest_quick_replies` | `true` | Offer quick replies under a Codex or Antigravity reply that ends with a question |
| `check_updates_on_startup` | `true` | Ask GitHub for the latest release once, a few seconds after startup |
| `font`, `font_scale` | JetBrains Mono 11, `1.0` | Terminal font and zoom |
| `cursor_shape`, `cursor_blink` | `block`, `true` | Terminal cursor |
| `scrollback_lines` | `10000` | Terminal scrollback |
| `indicators` | `[]` | Header status lights. See [Status indicators](reference.md#status-indicators) |
| `turn_command` | none | A command run when a turn ends (`{id}`, `{dir}` are filled in) |
| `clear_env` | session markers | Variables removed from a spawned agent's environment (`*` matches a prefix) |

### Profiles

Profiles define how each CLI is run: the chat agents (Settings → Agents edits these)
and anything you want as a terminal thread. Adding a CLI is a config edit, not a
rebuild:

```jsonc
{
  "profiles": [
    { "name": "Claude",  "command": "claude" },
    { "name": "Codex",   "command": "codex", "args": ["--full-auto"] },
    // Root a profile in a specific project, with its own environment.
    { "name": "Infra",   "command": "claude",
      "dir": "~/git/infra",
      "env_file": "~/.config/agent-terminal/infra.env" }
  ],
  "default_profile": "Claude"   // omit, or null, to use the first one installed
}
```

`env_file` is sourced in a subshell and its *exported environment* merged into the
session. Nothing it prints reaches the terminal, because output before `exec` breaks the
CLI's terminal handshake. Settings → Agents also sets each agent's extra arguments,
environment file, default model, mode and effort, and whether it is enabled.
`available_for_delegation` defaults to `false`; the corresponding Settings switch
opts a profile in as a parent and child target. Route support remains a separate check.

### Local and self-hosted models

Claude and Codex can run on a model server instead of their vendor's: Ollama (0.14 or
later), LM Studio (0.4.1 or later) or a LiteLLM gateway. In Settings → Agents, pick the
server under **Model Provider**, give its **Server URL** (blank: the server's default on
this machine, `http://localhost:11434`, `:1234` or `:4000`), then pick a **Default Model**
from the server's list. A profile can also set it in `config.json`, so one agent can have a
cloud profile and a local one:

```jsonc
{ "name": "Claude (Ollama)", "command": "claude", "driver": "claude",
  "default_model": "qwen3-coder",
  "provider": { "kind": "ollama", "base_url": "http://192.168.1.20:11434" } }
```

`kind` is `ollama`, `lm_studio` or `litellm`. A server that needs a key gets it from an
environment variable: set `api_key_env` (Settings: **API Key Variable**) to the variable's
name, defined in the profile's environment file or your session. The key itself is never
written to the settings. If the address is invalid, or the key variable is not set, the
thread refuses to start: it never falls back to the vendor's models.

The model list is the one request the app makes to the server itself (`GET <url>/v1/models`).
Claude reaches the server's Anthropic-compatible API at the URL's root; Codex reaches its
OpenAI-compatible Responses API at `<url>/v1`. Antigravity only runs Google's models.

Each profile's threads pick from its own server's list: a cloud profile and a local one of
the same agent each show their own models, and a model is only ever called retired against
the list of the server it runs on. A profile with no **Default Model** starts on the first
model its server lists that is not an embedding model; it never takes the CLI's own default
(such as `opus` in `~/.claude/settings.json`), which the server would not have.

Claude Code also picks models on its own: its `opus`, `sonnet`, `haiku` and `fable`
aliases (background tasks use `haiku`), its subagents and auto mode. On a model server all
of these name the thread's model (`ANTHROPIC_DEFAULT_*_MODEL`, `CLAUDE_CODE_SUBAGENT_MODEL`
and the rest are set for it, and follow a model change), so nothing is sent for a model the
server does not have. Values for them in your own environment are replaced. Changing the
model of a thread on a server restarts Claude on the new model, resuming the same
conversation (Claude Code cannot switch to a model id it does not know in place). Ollama
answers Claude's token-count requests with 404 (it has no `/v1/messages/count_tokens`);
Claude carries on without them.

Claude Code assumes a 200K-token context window for a model id it does not know, and
compacts only near that. Give the server a context window a coding agent can work in (64K
or more: `OLLAMA_CONTEXT_LENGTH` for Ollama, the model's context length in LM Studio), and,
if it is under 200K, set `CLAUDE_CODE_AUTO_COMPACT_WINDOW` to it in the profile's
environment file (100K at least) so Claude compacts before the server cuts it off.

## agy's approval hook

`--dangerously-skip-permissions` is agy's name for bypassing its built-in terminal
permission prompts. Agent Terminal needs that mode only because it is a GUI: agy's
headless process cannot display and receive its own interactive prompt there. The app
replaces that prompt path with its local approval hook, so each tool request is sent to
the thread and waits for your explicit Allow or Deny response. It is not enabled to
give an agent unasked access to your machine.

Agent Terminal passes this flag **only** when its approval hook is installed in
`~/.gemini/config/hooks.json` and an approval socket is live. That condition is
deliberately strict: if the hook is absent, unavailable, or fails to ask before a tool
step, the session is restarted without the flag. The hook is a safety boundary, not a
way to weaken one.

Without the hook, the mode picker drives agy's own `--mode` flag (Plan or Accept
edits), and **Ask before edits is unavailable**: headless agy has nobody to ask.
Without the skip flag agy refuses shell commands in every mode, but **it applies
file edits in every mode, Plan included** (it writes a plan, then carries it out). The
thread tells you so.

To install the hook, add this entry as a top-level key of
`~/.gemini/config/hooks.json`. Settings → Agents shows the same entry, with a Copy
button and whether it is installed:

```json
{
  "agent-terminal-approval": {
    "PreToolUse": [
      {
        "hooks": [
          {
            "command": "bash -c '[ -n \"$AGENT_TERMINAL_APPROVAL_SOCKET\" ] || exit 0; exec \"${AGENT_TERMINAL_HOOK_BIN:-agent-terminal}\" --approval-hook'",
            "timeout": 600,
            "type": "command"
          }
        ],
        "matcher": ".*"
      }
    ]
  }
}
```

The hook does nothing outside Agent Terminal, because the socket variable is unset
there. The app also checks that the hook really fires: if agy runs a tool without
asking first, the thread is restarted without `--dangerously-skip-permissions`.

