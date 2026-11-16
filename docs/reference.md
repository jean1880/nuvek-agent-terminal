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

### Tagging threads

Tags group threads across folders ("review", "spike", "blocked"). A thread's context menu has,
after the pin entries:

| Entry | Shown | Does |
|---|---|---|
| **Tags…** | always | opens the Tags dialog |
| **Clear Tags** | a thread with tags | removes them all, with a toast offering **Undo** |

The **Tags** dialog lists every tag any thread has, each with a box to tick for this thread
and a colour, and an entry for new tags, separated by spaces, commas or new lines. **Save**
writes them; **Cancel**, `Escape` or a press outside the dialog changes nothing.

- **What a tag is:** one word of at most 32 characters, stored lowercased, with a leading
  `#` dropped (`#Review` is `review`). Whitespace, commas and control characters are refused,
  and so is anything that looks like a secret (a key or a token): a tag is shown on every row,
  so it is never kept masked. A thread has at most 8 tags. What would be refused says why
  under the entry, and **Save** stays off until it is fixed.
- **Colours:** **No colour**, **Accent**, **Green**, **Yellow** or **Red**. A colour belongs to
  the tag, on every thread that has it, and is drawn in the colour scheme's own shade of it
  (the desktop's, with the **System** scheme), tinting the chip; its text stays readable in
  every scheme. A colour is forgotten once no thread has its tag (the **Undo** of **Clear
  Tags** brings it back).
- **On a row:** up to two tags show as chips after the title, and `+N` counts the rest; its
  tooltip lists them all, and a screen reader reads every tag in the row's description.
  Clicking a chip (or `Enter` on it) adds `#tag` to the sidebar's search and moves the
  keyboard there, at the end of what it holds.
- **Kept:** archiving keeps a thread's tags; deleting it removes them. Tagging never changes a
  thread's last-active time, so the folder groups stay in place. Tags are never sent to an
  agent or put in a hand-off.

### Searching the sidebar

The sidebar's search entry matches what you type, case-insensitively, in each thread's title,
folder and tags. A word starting with `#` is a tag filter instead: `#review` keeps only the
threads tagged `review` (the whole tag, in any case), and several are all required
(`#review #blocked`). Words and tag filters combine: `#review deploy` lists the threads
tagged `review` whose title, folder or tags hold "deploy". A lone `#` filters nothing yet.
The filter applies to the **Pinned** and **Active** sections too; terminal pages have no tags,
so a tag filter hides them.

With **Also Search Messages** on, only the words are looked for in messages, and the tag
filters keep the matches to the threads that have every one of those tags; tag filters alone
search no message.

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

### Searching messages

The toggle beside the sidebar's search entry (**Also Search Messages**; reach it with `Tab`
from the entry) adds what the threads said to the search. It has no shortcut of its own, and
`Ctrl+Shift+F` stays the terminal's scrollback search.

- **What you type:** words, matched whole and in any order; the last word also matches as
  the start of a longer one (`deplo` finds "deploy"). Quotes, `*`, `-` and words such as
  `NEAR` or `OR` are taken as plain text, never as search syntax. A `#tag` word is never
  looked for: it keeps the matches to threads with that tag
  ([searching the sidebar](#searching-the-sidebar)).
- **When it runs:** under half a second after you stop typing, off the main thread, so the
  window never waits on it. Only the newest search is shown.
- **What it shows:** up to 50 matches, best first: the thread's title, "Archived" for an
  archived thread (listed only with **Show Archived**), how long ago it was said, who said it
  (**You**, **Agent** or **Tool**, a tool by its title) and the words around the match, the
  matched ones in bold. The text is shown as it was written; nothing in it is read as
  formatting. A thread's folder is the match's tooltip.
- **Its states:** "No messages match." when nothing does; "Indexing N threads…" while earlier
  threads are still being indexed, when the matches may be partial (the search runs again
  every few seconds until the indexing is done, and pauses while the sidebar is hidden);
  "Loading threads…" before the thread list has loaded; "Search is unavailable" when the
  index cannot be read, never "no match". The matches follow the thread list: a thread
  deleted or renamed meanwhile is searched for again.
- **Opening a match** opens its thread (or brings it forward, in whichever window has it)
  and scrolls to the message, opening the card it is in. A thread shows its newest 50,000
  events; a match older than that opens the thread with a note that the message is further
  back than it shows.

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

### Spending

**Spending…** (header menu) sums the completed turns stored on this computer. Pick **Day**,
**Week** or **Month** for the last 30 days, 12 weeks or 12 months, and **Group by** folder,
agent, profile or model. Each row shows its amount, a bar against the largest row, and its
tokens and turns; below the groups, the same totals by day, week or month. **Plan usage**
repeats the quota windows the agents last reported.

What each agent reports:

- **Claude** reports what each turn cost. On an API key or a metered model server that is real
  money; on a plan it is the list price of the turn, not a charge. Until the app records how a
  turn was billed, a cost reads as "reported".
- **Codex, Antigravity and the Local agent** report tokens only. Their rows say "Not
  reported" and show their tokens. The app never estimates dollars from a price list of its
  own.

The kinds of spend are never added into one number, and "Not reported" is never shown as $0.
A sub-agent's turns count under the thread that started it, in that thread's folder, and the
notes give their share. A deleted thread's turns are kept, summed by day, so totals never drop
when you delete a thread. Claude's turns stored before costs were recorded per turn carried
the process's running total, so their cost is left out (their tokens still count) and a note
says how many there were.

The first time the dialog opens it builds an index over completed turns, once, in the
background. Nothing here makes a request or leaves this computer.

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

`Ctrl+Shift+D` opens a panel listing the changed files under a header for each
folder (the repository root first). It only reads, except for the commit you confirm from
it ([Committing](#committing)). Each file is a collapsed row with its line counts and
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
`{line}`, `{column}` and `{dir}`; a missing line or column is 1. **Ctrl+click** a line of a
file's diff to open the file at that line: an added or unchanged line is its own line, a
removed line or a hunk header the nearest line of the new file. A plain click still selects
text, and a line with no place in the new file (the file header, a deleted file) opens nothing.

Other places open a file in the editor the same way:

- A **read's card** (Read, `view_file`, the Local agent's `read_file`) has **Open in
  ‹editor›** for the file it read, at the line it started from. A search's card has none.
- **Links in a reply**: a link to a path (`[parser](src/parse.rs#L12)`), a `file://` link,
  and inline code that names a file and line (`` `src/parse.rs:12` ``, `` `a.rs:12:3` ``) or a
  path with a folder (`` `src/parse.rs` ``). Web links still open in the browser.

**Containment.** The path is resolved first, every symlink followed, and must lead to a
regular file. A file inside the thread's folder opens at once. A file outside it is asked
about first, naming the file the path leads to, on every click: no answer is remembered, and
Cancel is the default. If the path leads somewhere else by the time the editor starts, nothing
opens. One of the sensitive names an agent's edits always ask about (`.git`, `.vscode`, agent
instruction files and the like) is refused with a message, inside the folder or out. A path
from the conversation is only ever read as a location: it never chooses the program or adds an
argument.

### Committing

On **Uncommitted**, each file row has a tick box, all ticked at first, and the **Commit…**
bar under the diff counts them; its own box ticks or unticks every file. What you untick
stays unticked across refreshes, and a file that appears later starts ticked. The bar is
hidden on the other bases and when there is nothing to commit, and is shown but off, with
the reason, while a merge, rebase, cherry-pick or revert is half done or the PR bar's push
runs.

**Commit…** opens a dialog with the branch, the files, a box for the message (a subject
line, a blank line, then the body) and the exact commands it runs:

```
git --literal-pathspecs add -A -- <files>
git --literal-pathspecs commit -m '<message>' -- <files>
```

- **Only the ticked files are committed.** Anything else you had staged yourself stays
  staged and is not part of the commit; the dialog says how many such files there are.
  Pathspecs are literal, so a file named `*` names only itself. A rename commits both its
  old and its new path.
- **Your hooks and signing run, and nothing is skipped or amended.** The app never passes
  `--no-verify`, `--amend`, `--allow-empty`, `-a` or `--no-gpg-sign`. Your own `user.name`
  and `user.email` are used; when one is missing git's message is shown and nothing is
  committed. A hook or signing step that waits for input cannot be answered from here and
  fails (after two minutes at most, when git is stopped together with every hook and helper
  it started); commit in the drawer's terminal instead.
- **A failing hook** leaves HEAD where it was, with git's and the hook's own words shown
  (anything secret-shaped in them masked). The dialog then says that the ticked files stay
  staged as they are on disk, replacing any part of them you had staged before, and how to
  unstage one: `git restore --staged -- <file>`, or `git rm --cached -- <file>` before the
  first commit. Either leaves the file on disk.
- **If the branch or its latest commit changed** while the dialog was open, nothing is
  staged or committed, and you're asked to open Commit… again.
- **A file replaced by a folder** is refused before anything is staged: staging that path
  would take in everything in the folder. Commit that change in a terminal. A file whose name
  is not valid UTF-8 cannot be ticked, for the same reason: it would name another file.
- **These checks are made just before staging.** A change another program makes while git
  itself is staging and committing (moving HEAD, or swapping a file for a folder) is not
  detected, just as when you run git by hand.
- **On the default branch or a detached HEAD** the dialog warns you, and the commit is
  still yours to make.
- **A message that looks like it holds a key or token** is committed exactly as typed, but
  only after a second confirmation.
- **Files the panel does not list** (ignored, secret-looking names, untracked files over
  5 MiB) are never part of a commit made here.
- **Draft Message** (in a thread) closes the dialog and puts a prompt in the composer asking
  the agent for a message for the ticked files. It names the files and sends no diff; you
  review it and send it like any message. Paste the reply into the dialog when you reopen it.
  The message you were writing is kept for the thread while it is open, and cleared by a
  commit.

After a commit the panel refreshes, and the PR bar counts the new commit to push. Undo
never touches commits ([Undoing a turn](#undoing-a-turn)).

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
| `{selection}` | The text selected in the thread's transcript, else the text selected in the composer. At most its first 8 KB is used, and a toast says when it was cut. One run of text with no space or line break, too long to check for secrets whole, is left out, and a toast says so |
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
