# Troubleshooting

**Where are the logs?** In the systemd journal, and on stderr when started from a
terminal:

```bash
journalctl --user -t agent-terminal -b
RUST_LOG=debug agent-terminal
```

**"No AI CLI detected."** None of the configured commands was found on `PATH`, in the
usual install folders, or through your login shell. Install one (see
[The agents themselves](install.md#the-agents-themselves)), or set its full path in Settings →
Agents, then press **Check again**.

**Setup never appeared, or keeps coming back.** Setup opens when there is no settings file
at launch (`~/.config/agent-terminal/config.json`). Closing it writes that file. If it comes back
at every start, the file could not be written, and the app says so when setup closes. Run it
any time from Settings → General → **Run Setup Again…**.

**An agent says "Not signed in".** Use **Sign In…** in Settings → Agents (or on the
thread's banner), finish signing in, then **Check Again**.

**An agent is stuck or misbehaving.** ☰ → **Restart Agent** (`Ctrl+Shift+R`) stops the
thread's agent process and starts it again on the same conversation. Queued prompts are
dropped. It also picks up changes to the agent's settings, environment file, MCP servers
or hooks.

**agy edits files in Plan mode.** It does, without its approval hook. Install the
[hook](configuration.md#agys-approval-hook).

**My local build doesn't start / opens another copy.** Agent Terminal is
single-instance. If another copy is running, a new launch hands over to it and exits.
Run a development build on its own session bus:

```bash
dbus-run-session -- ./target/debug/agent-terminal
```

**Settings reset after a hand edit.** The file didn't parse. Your version was kept as
`config.json.invalid-…` beside it, and the app said so when it started.

