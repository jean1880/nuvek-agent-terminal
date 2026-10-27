# Installing, verifying and uninstalling

## Verifying a download

Each release lists the SHA-256 of every package in `SHA256SUMS`. Download it next to the
package and check it:

```bash
sha256sum --check --ignore-missing SHA256SUMS
```

Every release also carries a signed build provenance attestation for every package. It
proves the file was built by this repository's release workflow from the tagged commit. Check
it with the [GitHub CLI](https://cli.github.com/):

```bash
gh attestation verify ./agent-terminal_<version>_amd64.deb --repo jean1880/nuvek-agent-terminal
```

Releases are also immutable: once published, their packages and tag cannot be changed.

## From source

See [Building from source](building.md). Then run `make install` to install
into `~/.local` (binary, icon and desktop entry), and `make uninstall` to remove it.

## Uninstalling

Use **Settings → General → Uninstall Agent Terminal…**, or quit the app and run:

```bash
agent-terminal --uninstall
```

It lists what it found and asks whether to remove your configuration and data too. The answer
defaults to yes, unless another copy stays installed (a `make install` copy beside the package,
or the reverse): both use the same data, so the default is then to keep it. Then it removes the program the way it was installed: `apt`, `dnf` or `pacman`
(asking for your password), or the `make install` files. `--yes` answers yes without asking, and
`--keep-data` keeps your configuration and data. Run it as yourself, not with `sudo`: your
configuration is in your home folder, and it asks for the password itself.

Your configuration and data include:

- your settings (`~/.config/agent-terminal`)
- threads, approval rules and hand-off briefs (`~/.local/state/agent-terminal`)
- caches (`~/.cache/agent-terminal`)
- the turn checkpoints (`refs/agent-terminal/*`) in each repository a thread ran in
- the `agent_terminal` MCP server and the allow rules Agent Terminal added to Antigravity, and
  the rules file it exported to Codex

Your own settings in those tools are kept. Your worktrees and branches are never touched. If you
pasted [agy's approval hook](configuration.md#agys-approval-hook) into `~/.gemini/config/hooks.json`, remove
that entry yourself: it does nothing without Agent Terminal.

Removing the package directly (`sudo apt remove agent-terminal`) keeps every user's
configuration and data, because a package never touches a home folder. Delete the folders
above to remove them.

## The agents themselves

Install at least one agent CLI; the [first start](walkthrough.md#1-first-start) checks you are signed in
and offers **Sign In…** if not. Agent Terminal finds them on your `PATH`, in the usual install
folders, and through your login shell (so `nvm` and `asdf` installs work):

| Agent | Command | Get it |
|---|---|---|
| Claude Code | `claude` | [docs.claude.com/claude-code](https://docs.claude.com/en/docs/claude-code) |
| Antigravity | `agy` | [antigravity.google](https://antigravity.google) |
| Codex | `codex` | [github.com/openai/codex](https://github.com/openai/codex) |

