# Reference

### Resuming sessions

Resuming is declared per profile, so the binary knows no CLI's conventions:

```jsonc
{ "name": "Claude", "command": "claude",
  "resume_args": ["--resume", "{id}"],          // {id} is the session ID
  "session_store": "~/.claude/projects",        // where <id>.jsonl transcripts live
  "session_title": "/aiTitle" }                 // JSON pointer to a session's title
```

**Resume Session…** opens a browser of the store's 200 most recently active sessions,
newest first. Each row shows the session's title, the folder it will resume in, and
how long ago it was last active. Type to filter by title, folder or ID; Enter resumes
the top match. **Enter ID…** in its header falls back to pasting an ID. Only the first
and last 256 KiB of each transcript are read, so a store of multi-MiB transcripts
still lists quickly. A store that cannot be read says so rather than showing an empty
list.

`session_store` exists because Claude only resumes a session from the project
directory it was recorded in. The transcript is looked up (directly in the store or
one directory down) and its first recorded `cwd` becomes the thread's directory. If it
can't be found, the thread opens in the profile's default directory and a dialog
explains how to pass `--dir`.

agy keeps one history log instead of a transcript per session, so its profile declares
`"session_format": "agy-history"`. Its sessions are titled by their first prompt and
resumed with `--conversation <id>` in the workspace they were recorded in. Only the
last 4 MiB of the log is read.

Claude and agy profiles are filled in with these settings on load; so are the hand-off
settings below. An explicit empty list, such as `"resume_args": []`, opts out.

### Handing off between CLIs

For a terminal thread, **Continue In ▸ <profile>** opens one running another CLI in
the same folder, starting with a prompt that points at a *hand-off brief*. The
out-of-quota CLI cannot summarize its own work, so the brief is written from disk:

- the original request, the latest requests, the last reply and the files the session
  edited, from the transcript (agy's log records requests only);
- `git status`, `git diff HEAD --stat` and the last five commits;
- the end of the screen, if no transcript can be found.

Briefs are written to `$XDG_STATE_HOME/agent-terminal/handoffs/`. The directory is
`0700`, each file is `0600`, and briefs older than 7 days are pruned. Credentials in
common formats (`ghp_…`, `sk-…`, `AIza…`, `Bearer …`, `*_KEY=…`, `"password": …`, PEM
blocks) are masked as `****` plus their last four characters. The masking works by
pattern, so a secret in an unrecognized format can get through, and each brief says
so.

Quota exhaustion is detected from Claude's transcript (a `rate_limit` error), or by
`limit_markers`, text matched near the bottom of the screen, for a CLI with no such
record:

```jsonc
{ "name": "Agy", "command": "agy",
  "prompt_args": ["--prompt-interactive", "{prompt}"],   // how to start with a prompt
  "limit_markers": ["RESOURCE_EXHAUSTED", "quota exceeded"] },
{ "name": "Claude", "command": "claude",
  "prompt_args": ["{prompt}"],
  "session_id_args": ["--session-id", "{id}"] }          // start under a chosen ID
```

### Exporting a thread

**Export…** in a thread's context menu (the third section, after **Open folder**; greyed out while
the thread has no messages) asks for a format, whether to include the model's reasoning (off) and
tool output (on), then where to save. The suggested name is `<title-slug>-<yyyy-mm-dd>.md` or
`.json`. The file is `0600`, written through a temporary file beside it and renamed into place, so
an existing file is replaced whole and a symbolic link at the name is replaced, never written
through. The whole document is masked once more before writing.

**Markdown** has a header (title, folder, last-active date, agents, app version, the masking
note), then `### You` and `### <Agent>` blocks, a `**<kind>:** <title>` line per tool with its
output fenced, `> **Reasoning**` quotes when asked, and `---` dividers with an italic line for
agent switches, notices and carried-in context. Tool output over 20,000 characters is cut with
`[… N more characters omitted]`.

**JSON** (`"format": "agent-terminal-thread"`, `"version": 1`):

```jsonc
{
  "format": "agent-terminal-thread",
  "version": 1,
  "app_version": "1.2.1",
  "note": "Secrets matching known patterns are masked; one in another format may remain.",
  "thread": { "id": "…", "title": "…", "folder": "…",
              "last_active": "2026-11-15T12:00:00Z",   // omitted when unknown
              "agents": ["claude", "codex"] },          // driver keys, in order of first use
  "items": [
    { "role": "user",          // user | assistant | tool | inherited | notice
      "agent": "claude",       // driver key, or null
      "kind": "user_message",  // assistant_message, reasoning, command, …
      "title": "",             // a tool's one-line title
      "text": "…",             // images as "[image: name]"; a tool's output, capped
      "status": "completed",   // open | completed | failed | declined | interrupted
      "omitted_chars": 0 }     // present only when a tool's output was cut
  ]
}
```

Fields are only ever added within a version; removing or renaming one raises `version`.

### Pinning threads

A thread's context menu (right-click, the Menu key or `Shift+F10`) has, next to **Archive**:

| Entry | Shown | Does |
|---|---|---|
| **Pin to Top** | an unpinned thread | adds it to the end of the **Pinned** section |
| **Unpin** | a pinned thread | returns it to its folder (or **Active**) |
| **Move Up**, **Move Down** | a pinned thread | moves it one place along **Pinned** |

Each shows a toast with **Undo**; undoing an unpin puts the thread back in the place it had.
The Pinned section lists threads in the order you set, not by recency, above **Active**.
A pinned thread is listed once: a pinned thread that is also active stays under Pinned
with its badge, and the **Active** header counts it. Archiving keeps the pin; an archived
pinned thread shows only with **Show Archived**, or while it is open. Pinning and moving
never change a thread's last-active time. Deleting a thread removes its pin.

| Shortcut | Does |
|---|---|
| `Ctrl+Shift+P` | pins or unpins the shown thread |
| `Alt+Shift+Up`, `Alt+Shift+Down` | moves the selected pinned row in the sidebar |

### The message index

Every thread is indexed for searching inside threads, always, with no setting. The index
lives in the thread store (`threads.db`, readable only by you) next to the conversation it
comes from, and grows the store by roughly the size of the text it holds.

- **Indexed:** your messages (an attached image as `[image: name]`), the agents' replies, and
  each tool's title as its card shows it (`Run cargo test`, `Edit src/lib.rs`).
- **Never indexed:** a tool's output, the model's reasoning, the app's notices, the raw
  frames an agent sent, and images themselves.
- **Redacted:** the text is taken from what the store already holds (masked when it was
  saved), and each item is masked again as a whole before it is indexed, so a secret that
  arrived split across two pieces of a reply is caught there too. Each item keeps at most
  32 KB.
- **When:** a reply is indexed once its turn (or its agent) ends, your message as soon as it
  is sent. An imported history is indexed when it is imported.
- **Earlier threads:** a few seconds after start-up, threads not indexed yet (those from
  before this version, say) are indexed in the background, the most recently active first,
  one at a time so the app's own saving never waits. A run cut short by quitting carries on
  at the next start, from where it stopped.
- **Without full-text search:** a build whose SQLite lacks FTS5 still opens every thread;
  only search is unavailable.

Deleting a thread removes its part of the index. Uninstalling removes the index with the
store.

### Status indicators

Optional header-bar lights driven by a file or a command. Empty by default.

```jsonc
{
  "indicators": [
    // Non-empty file contents mean "needs attention"; the contents are the detail.
    { "label": "Config drift",
      "source": { "type": "file", "path": "~/reports/drift.txt" },
      "refresh_secs": 300 },

    // A non-zero exit means "needs attention"; stdout is the detail.
    { "label": "Backups",
      "source": { "type": "command", "argv": ["check-backups", "--quiet"],
                  "timeout_secs": 15 },
      "refresh_secs": 900,
      "action": "show-output" }
  ]
}
```

Each indicator has **three** states, not two: OK, needs-attention, and *unknown*. A
source that cannot be read gets its own icon and says so; it is never quietly reported
as healthy. Commands run off the UI thread and are killed at their timeout.
`"action": "send-to-terminal"` adds a button that types the detail into the running
session, behind a preview.

### The context gauge

The thread header's gauge reads "N% left". It has three states: a known window (a bar and a
percentage), an unknown window (a dim "context ?", with no bar and no percentage), and no
gauge at all before the agent has reported anything. An agent that reports tokens used but no
window size, such as Antigravity, stays unknown, and a Codex reading of zero tokens with no
window is unknown rather than an empty window.

Click the gauge for a popover with the numbers and three things to do:

- **Compact**: sends the agent's own command, labelled for it ("Compact (Claude's
  /compact)"). An agent with no such command, such as Antigravity or the Local agent, has no
  Compact row: Continue does the same work.
- **Continue in a new thread**: a new thread carrying a budgeted hand-off summary, for any
  agent.
- **Context breakdown (/context)**: opens the `/context` panel, for agents that report one.

Compact and Continue are off, with the reason shown, while a turn runs. `/compact` and
`/context` also work from the composer.

### Turn checkpoints

In a thread whose folder is inside a git repository, the working tree is snapshotted
each time a turn ends. **Checkpoint Now** takes one on demand. A snapshot that fails
marks the thread with a warning icon, and its tooltip says why.

Snapshots are commits recorded only as refs under `refs/agent-terminal/<thread>/`. They
never touch your branch, index, working tree or stash. Each captures tracked changes
plus untracked, non-ignored files. It leaves out files over 5 MiB, more than 2,000 new
files, nested repositories, and names that look like secrets (`.env*`, `*.pem`,
`*.key`, `id_rsa*`, …). Each thread keeps 50 checkpoints, and any checkpoint older than
7 days is deleted.

A normal `git push` does not send these refs. `git push --mirror`, or a local
`git clone` of the repository, **does**. A dirty submodule's contents, and files
ignored by `.gitignore`, are not captured. Tracked Git LFS files are stored in
`.git/lfs/objects` with no size cap.

Turn it off in Settings → Git (**Checkpoint Each Turn**). To remove every checkpoint
from a repository:

```bash
git for-each-ref --format='delete %(refname)' refs/agent-terminal | git update-ref --stdin
```

### Diff panel

`Ctrl+Shift+D` opens a read-only panel listing the changed files under a header for each
folder (the repository root first). Each file is a collapsed row with its line counts and
**Open in…**; expanding it shows that file's coloured unified diff, built when first opened.
The dropdown picks what to compare against:

| Base | Shows |
|---|---|
| **Uncommitted** | HEAD against the working tree now, untracked files included |
| **Last turn** | What the thread's latest checkpoint changed |
| **This thread** | Everything since the state the thread's first checkpoint was taken on top of |
| **Target branch** | Every change against the primary branch (`origin/HEAD`, `main` or `master`) |

The panel refreshes when it opens, when a checkpoint is taken, when the base changes,
and on its refresh button. It never polls. Git's external diff and textconv drivers
are not run. Diffs above 1 MiB or 20,000 lines are cut short. With a diff tool set
(Settings → Git), **Open in…** launches it on any file.

Each file row, and each file-change card in the chat, also has **Open in ‹editor›**. With an
editor set (Settings → Git → Editor) it opens the file at the first changed line; with none
set, the system's default app opens the file, which cannot jump to a line. The command is an
argv, never run through a shell. Its placeholders are `{file}` (required, the absolute path),
`{line}`, `{column}` and `{dir}`; a missing line or column is 1.

**Containment.** Only a file inside the thread's folder opens. The path is resolved first, so
a symlink that leads out of the folder counts as outside, and it must be a regular file. A file
outside the folder, or one of the sensitive names an agent's edits always ask about (`.git`,
`.vscode`, agent instruction files and the like), is refused with a message.

### Undoing a turn

With the diff panel on **Last turn** or **This thread**, the undo arrow puts the
working tree back to the diff's left side. A confirmation lists every file that will
change (`~`) or be deleted (`−`), and warns if the agent still looks busy.

- **The current state is saved first**, as `refs/agent-terminal/<thread>/pre-restore-<time>`,
  and the **Undo** on the "Changes undone" message puts it back.
- **Nothing checkpoints never capture is touched**: ignored files, files over 5 MiB,
  names that look like secrets, nested repositories.
- **Commits and staged changes are never touched.** Only the working tree changes.
- **If a file changes after you open the confirmation**, nothing is restored and you're
  asked to try again, so nothing unsaved can be lost.
- Deletions stay inside the repository and never follow a symlinked folder.
- Afterwards the result is checked against the checkpoint. Anything that still differs
  is reported, with the `git restore` command that puts the saved state back by hand.

### Worktree threads

**New Thread in Isolated Git Worktree…** (`Ctrl+Shift+G`) asks for the repository, a new branch
and what to start it from, checks them as you type, then runs `git worktree add -b <branch>` and
opens a thread there. The repository starts as the current thread's, when it is in one; **Choose…**
picks any other folder in a repository, so no thread needs to be open in it first.

Worktrees go in a hidden folder beside the repository,
`<parent>/.<repo>.worktrees/<branch>`, so tools that scan every project folder don't
index a second copy. Set **Worktree Folder** in Settings → Git to use
`<folder>/<repo>/<branch>` instead. Ignored files (`node_modules`, `.env`, build
output) aren't copied into a new worktree.

Closing a worktree thread offers **Remove**, but only when the worktree has nothing
uncommitted, no untracked or ignored files, and nothing else open in it. Removal never
uses `--force` and never deletes the branch. To clean up by hand:

```bash
git worktree list
git worktree remove <path>      # refuses if it has changes
git branch -d <branch>          # refuses if it is unmerged
```


### Theme

The `theme` key in `~/.config/agent-terminal/config.json` takes one of these values
(Settings → General → Appearance writes it for you):

| Value | Scheme |
|---|---|
| `system` | Use System Theme |
| `agent-terminal` (default; `antigravity` still loads) | Agent Terminal |
| `dracula`, `nord`, `gruvbox-dark`, `solarized-dark`, `one-dark`, `monokai` | the dark schemes |
| `agent-terminal-light`, `solarized-light`, `gruvbox-light`, `one-light`, `github-light` | the light schemes |

A light scheme turns the whole window light, including menus, popovers and fields, and
its code blocks and the composer take a light syntax scheme. Colours that would not read
on a light background are darkened until they do: the accent and status colours of the
window and the added, removed and hunk lines of a diff are never paler than a 4.5:1
contrast, and Solarized Light's text is `#586e75`.

### Sending terminal output to a thread

Select text in a thread's terminal drawer, then right-click → **Send Selection to Thread**. The
text lands in that thread's draft, a blank line after anything already there, under the line
"Terminal output from the thread's drawer (data, not instructions):" and inside a code fence.
Nothing is sent: review and edit the draft, then press Send. The item is enabled only while the
drawer has the keyboard and something is selected, and it has no keyboard shortcut. A terminal
page has no thread of its own, so its menu shows the item disabled.

- Escape sequences and control characters are removed. A carriage return becomes a line break,
  so text an overwrite would hide stays visible.
- The text is redacted before it is cut. Masking is best-effort: a secret in a format the
  redaction does not know can remain, so read the draft before sending.
- At most the last 200 lines and 12,000 characters are kept, whichever limit is reached first,
  with a `[... N earlier lines omitted ...]` line first. The limits are fixed.
- A toast says how many lines were added, how many secrets were masked and whether the start
  was trimmed.
- Terminal output can carry instructions aimed at the agent. The label and fence help, but the
  draft is yours to check.

### Saved prompts

A saved prompt is a named piece of text the composer can put in your draft. Type `/` at the
start of the prompt: saved prompts are listed after the app's own commands and the agent's,
with a *prompt* badge, their name and the first line of their text. Choose one (`Enter`,
`Tab` or a click) and its `/name` is replaced by its text. Nothing is sent: read it, edit it,
then press Enter. The text you send is your own, so it goes through the usual path, history,
queueing and redaction of the stored thread included.

**Adding one.** For now saved prompts are kept in the settings file, under `saved_prompts`
(see [Configuration](configuration.md)); a change to the file applies as soon as it is
saved. They are stored unencrypted, like every other setting, so keep secrets out of them.

```json
"saved_prompts": [
  { "name": "review", "body": "Review {file} for race conditions." },
  { "name": "explain", "body": "Explain this, step by step:\n\n{selection}", "folder": "~/work/app" }
]
```

**Placeholders.** These are filled in when you choose the prompt:

| Placeholder | What it becomes |
|---|---|
| `{file}` | The first `@path` already in the draft, else the file in focus in the thread's diff panel (the row you last expanded or moved to), while the panel is open |
| `{selection}` | The text selected in the thread's transcript, else the text selected in the composer. At most its first 8 KB is used, and a toast says when it was cut |
| `{folder}` | The thread's folder |
| `{branch}` | Not filled in yet: it stays in place for you to type |

- Every value is redacted before it goes in, and a selection is redacted before it is cut, so
  a secret is never left half masked. Masking is best-effort: read the draft before sending.
- A placeholder with nothing to fill it stays as written (`{file}`) and the first one is
  selected, so typing replaces it. Nothing is silently left blank.
- `{{` and `}}` are literal braces, and any other `{word}` stays as it is. A value is put in
  as it is: a `{branch}` inside a selection is not filled in.

**Names.** Letters, digits, `-` and `_`, starting with a letter or digit, at most 48
characters, compared without regard to case. A name the app uses for its own commands or
their aliases (`model`, `compress`, `new`, …) is refused. A name an agent also uses is
allowed: both rows are listed, each with its own badge, the agent's first. A saved
prompt never runs a command and is never sent as `/name`.

**Limits.** The text is at most 16 KB, and only the first 200 saved prompts are used. A prompt
that breaks a rule (an empty name or text, a repeated name, a folder that is not an absolute
path or `~/…`) stays in the file but is not listed; the log says why.

**Folders.** A prompt with a `folder` is listed only in threads in that folder or inside it
(`/a/b` covers `/a/b/c`, not `/a/bc`). A folder's prompt is listed before the others, and takes
the place of a prompt of the same name without one; the deepest folder wins.

### Remote machines

A terminal page can run on another machine through your own `ssh`. Agent Terminal installs
nothing there and writes nothing there.

**Listing a machine.** In Settings → General → Remote Machines, type the name you reach it by
with `ssh` (a `Host` from `~/.ssh/config`, a host name or `user@host`) and press Enter or
**Add**. A name that `ssh` could read as an option (one starting with `-`) or that holds
spaces or shell characters is refused as you type, and nothing is saved. Each machine has
an optional label, **Check** and **Remove**.

**Check** runs `ssh -T -o BatchMode=yes -o ConnectTimeout=10 … -- <machine> true` in the
background, with the same forwarding, tunnel and remote-command options as a page and the
same environment, and shows **Reachable**, **Not reachable:** with the reason ssh gave
(secrets in it masked), or **Not checked**. It never asks for anything, so a machine whose key you have not accepted yet, or
that needs a password, shows as not reachable: open a terminal on it once and answer there.
Nothing is checked at startup.

**Opening a terminal there.** **+** → New Terminal Thread on Another Machine… asks for the
machine and a folder on it (`~` is your home there; `~/src` and absolute paths work). The
page runs

```
ssh -t -o BatchMode=no -o ConnectTimeout=15 -o ServerAliveInterval=15 \
    -o ClearAllForwardings=yes -o ForwardAgent=no -o ForwardX11=no \
    -o PermitLocalCommand=no -o Tunnel=no -o RemoteCommand=none \
    -- <machine> "cd -- '<folder>' && exec \"\$SHELL\" -l"
```

with no local shell around it. The folder is quoted, so nothing in it is expanded there. A
first connection's host-key question, or a password prompt, appears in the page, where you
answer it. `ClearAllForwardings=yes` overrides any `LocalForward`, `RemoteForward` or
`DynamicForward` in your ssh configuration, so no port opens on either machine;
`Tunnel=no` turns off a configured tun device, and `RemoteCommand=none` keeps a configured
remote command from replacing the one above. The rest of your configuration still applies as
you set it: `ProxyCommand` and `ProxyJump` run, `Match exec` conditions run their commands, an
existing `ControlMaster` socket is reused, and your keys and agent are used to log in. The login
shell there must understand POSIX `cd --` and `&&` (bash, zsh, dash, fish do; csh does not).

**What works on a remote page:** the terminal itself (scrollback search, copy and paste),
web links (`http://` and `https://` with Ctrl+click; any other link, a `file://` one above
all, names something on the other machine and is refused here), its `machine:folder` page
and window title (whatever title the shell there sets), its sidebar group, notifications on
the bell, and
**Reconnect** (the right-click menu, or **Restart** once it has exited), which connects again
to the same machine and folder.

**What does not:** turn checkpoints, the diff panel and undo, hand-offs, session pickers,
a profile's environment file or model provider, and **New Thread in This Folder** opening a
local thread. All of these read or start things on this machine. Remote pages are not
reopened at launch by Reopen Terminal Tabs, so no `ssh` starts unless you open one.
