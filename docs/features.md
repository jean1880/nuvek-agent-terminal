# Features

### Switching agents mid-conversation

A thread belongs to the work, not to an agent. At any point you can hand it to another one:

- **Pick a model from any agent** in the header chip (or type `/model`). The list covers
  available models from Claude, Antigravity and Codex, with search and effort levels. The same
  agent keeps its native conversation, updating in place or restarting and resuming
  as needed. Choosing another agent carries the conversation into a new session.
- **Switch during active work.** Confirm **Switch and Continue** to interrupt the
  running turn and automatically continue the unfinished task with the selected model.
  Pending approvals and questions are cancelled; the new turn can ask again if needed.
- **Switch while idle.** The session stays idle and uses the selected model for your
  next message. No continuation prompt is sent.
- **See whether the change was accepted.** The header shows the requested model as
  **pending** until the backend acknowledges it. Codex applies model choices on the
  next turn, so an idle selection can remain pending until you send a message.
  Rejected changes show an error and preserve the last accepted choice.
- **The conversation comes with it.** When changing agents, the continuation or your
  next message carries the thread so far, within a token budget: all of it when it
  fits, otherwise the first request and the most recent
  work, and the notice says which ("40 of 112 messages"). It is redacted (keys and tokens
  masked), tool output is shortened and fenced off so it cannot pass for your words, and
  reasoning is not carried. An undelivered handoff survives a restart until the next
  message. A divider marks where the thread changed hands, and each reply is labelled
  with the agent that wrote it.
- **Switch back whenever you like.** Returning to an agent works the same way: a fresh
  session from a summary of the thread, including what the other agent did in between (the
  divider says "Back in Claude · new session from a summary").
- **Safe by default.** A switch while the agent is working asks first. If the new agent
  cannot start (it fails to launch, or exits before its session begins), the thread goes
  back to the old one. Your chosen
  mode carries over and survives a reopen, and an automatic hand-off never lands on
  Antigravity without its approval hook while the thread does not allow unasked edits.
- **Out of quota? Keep going.** When an agent hits its limit, the thread offers
  **Continue in <other agent>**.
- **Fork instead of switching**: `/fork` starts a new thread with this one's history, so
  two agents can take the same problem in different directions.
- **Compare forks side by side**: **Compare with Fork…** in a thread's context menu, `/compare`,
  or **Compare** on the toast after a fork opens a window with both conversations since the
  fork, lined up turn by turn, and, in a git repository, a **Changes** tab with what changed in
  each thread's folder since the fork (two threads sharing one folder show one diff, since
  their changes cannot be told apart).

### Multi-agent environment review

- **Start from the folder you are working in.** Choose **Review Multi-Agent
  Environment…** from the window menu, or a thread's context menu to review that
  thread's folder. Confirm **Start Review** to open a new agent session in that exact
  folder, including nested project folders.
- **A [bundled review skill](../assets/skills/multi-agent-environment-review/SKILL.md).**
  The session receives the skill and review instructions
  automatically. It inspects relevant project instructions, agent configuration,
  skills, MCP registrations, permissions, delegation and recovery practices.
  The instructions request a read-only review, with no installations, configuration
  changes or MCP server probes. The selected agent's usual permissions and quota apply.
- **A consistent report in chat.** Seven sections cover scope and evidence, summary,
  scorecard, agent/MCP matrix, skill suggestions, ordered improvements and limits.
  Eight aspects receive evidence-based ratings from **0–5** or **unknown**, with an
  overall score and evidence coverage. Recommendations explain what to change, how
  to verify it, how to undo it and what approval is needed. Configured tools are
  distinguished from tools with demonstrated runtime results.

### Chat threads

- **A sidebar of threads**, grouped by folder and searchable. **Pin to Top** (or
  `Ctrl+Shift+P` for the shown thread) lifts a thread into a **Pinned** section above
  everything else, in the order you choose: **Move Up** and **Move Down**, or
  `Alt+Shift+Up` and `Alt+Shift+Down` on the selected row. A pinned thread keeps its pin
  when archived (it shows again with **Show Archived**, or while it is open), and pinning
  never changes when a thread was last active, so the folder groups stay in place.
  **Tags…** gives a thread tags ("review", "spike"), each with an optional colour from the
  colour scheme; up to two show as chips on its row, and clicking one filters the sidebar by
  it. `#review` in the search keeps only the threads tagged `review`, and several `#` words
  must all match. See [tagging threads](reference.md#tagging-threads).
  Threads that are working,
  waiting for an approval, rate limited or running background work sit above the folders in
  an **Active** section, most urgent first, each naming its folder; a pinned thread that is
  also active is listed under Pinned only, keeps its badge, and is counted in the Active
  header. Each row shows the
  agent, the title, how long ago it was active, and a badge: working, needs approval,
  rate limited or unread. `F9` or `Ctrl+Shift+B` shows or hides it. It overlays the
  thread when the window is narrow. When hidden, its toggle still signals unread
  activity or a thread needing your attention.
- **Search inside threads.** The speech-bubble toggle beside the sidebar's search entry
  (**Also Search Messages**) looks for what you type in what the threads said too: below the
  matching threads, an **In Messages** section lists up to 50 matches, best first, each with
  its thread's title, who said it, how long ago and the words around the match. Pick one to
  open its thread at that message. Archived threads' matches show only with **Show
  Archived**, tagged "Archived". While earlier threads are still being indexed it says so,
  and the list fills in as they are. A message older than what a thread shows when opened
  opens the thread, with a note that it is further back. See
  [searching messages](reference.md#searching-messages).
- **What every thread said is indexed** for searching inside threads: your messages, the
  agents' replies and tool titles, redacted, never a tool's output or the model's reasoning.
  Threads from before are indexed in the background after start-up. See
  [the message index](reference.md#the-message-index).
- **Spending history.** **Spending…** in the header menu shows what the agents' completed
  turns cost, by day (the last 30), week (the last 12) or month (the last 12), grouped by
  folder, agent, profile or model, with each group's tokens. A turn that reported no cost
  says "Not reported", never $0, and sub-agents' turns count under the thread that started
  them. Deleted threads keep counting. Each turn records how it was paid for, so metered
  spend and a plan's list price stay apart, and Claude's extra usage, while switched on,
  shows beside its plan windows (for reference only: its unit is not confirmed). An optional daily or monthly warning (Settings → General → Spending) counts real
  money only. See [spending](reference.md#spending).
- **Your existing sessions.** Recent Claude and agy sessions appear as threads.
  Opening one imports its history and resumes it. A Codex thread resumes its own Codex
  thread; if Codex no longer has it, the next prompt continues in a new Codex thread with
  the conversation carried over.
- **A native transcript**: markdown, code blocks, tool cards, file-change cards with
  their diffs, inline approvals, a plan panel, sub-agent progress, questions with
  answers to pick, and a composer with typeahead.
- **Sub-agents stay docked.** A card above the composer lists the sub-agents at work, and
  keeps the finished ones (with how each ended) until your next prompt or **Clear
  finished**. `Ctrl+Shift+A` opens or closes every sub-agent card in the thread.
- **Delegate between providers.** Opt profiles in from Settings → Agents to give a
  parent a uniform delegation API. Supervised children use the worker dock and
  inspector, with provider/model evidence, scoped approvals, messages and diffs.
  Writing children use isolated git worktrees. Only reviewed routes are admitted. Codex
  0.161.0 is admitted as a parent and as a child, with the caveat
  `codex_native_workers_unsupervised`: as a child it still starts its own built-in
  sub-agents, which run outside Agent Terminal's depth, mode and worktree limits. The Local
  agent is admitted as a parent and as a child too, its every change and command checked as
  a child. See the [delegation guide](delegation/user-guide.md).
- **Pending approvals stay within reach.** A panel above the composer keeps approval
  controls available while you scroll, with extra choices in a popover when the
  agent supports them. Pending questions offer a jump back to their answer card.
- **Talk while it works.** A message sent while a turn is running goes straight into that
  turn for Claude and Codex. For Antigravity (and for slash commands) it is queued instead,
  pinned at the bottom of the transcript, and goes out when the turn ends. `Up` in an empty
  composer takes the last queued one back for editing.
- **Images in the conversation.** Attach a screenshot or any PNG, JPEG, WebP or GIF with the
  paperclip, by pasting it (`Ctrl+V`; text on the clipboard still pastes as text) or by dropping
  it anywhere on the thread: up to five per message, shown in the composer before you send them,
  then inside your message. Claude, Codex and the Local agent (with a model that reads images)
  receive them; Antigravity takes text only, so the message stays in the composer and says so.
  Images an agent looks at (a file it reads, a screenshot from a tool) appear as thumbnails on
  that step's card, and an image an agent chooses to show you appears under its card. Click any
  image to view it at full size, step through the others, copy it, save it or open it in your
  image viewer. Images are resized to 2048 pixels and stripped of their metadata before they are
  stored or sent, are kept only on your machine, and are removed a day after no thread uses them.
- **Saved prompts.** Instructions you repeat ("review {file} for race conditions") are listed
  after `/` in the composer, marked *prompt*. Choosing one puts its text in the draft, with
  `{file}`, `{branch}`, `{selection}` and `{folder}` filled in from the thread, and the first
  placeholder it could not fill selected; nothing is sent until you press Enter. It works the
  same for every agent. Add, edit, copy and delete them in Settings → General → Saved Prompts,
  and limit one to a folder; see the [reference](reference.md#saved-prompts).
- **Quick replies.** When a Codex or Antigravity reply ends with a question and a short
  list of choices, the choices appear under it; one click puts the answer in the composer,
  ready to send or edit. Turn them off from the replies themselves or in Settings → General.
- **One model picker for every agent.** See
  [Switching agents](#switching-agents-mid-conversation). Models that agy serves on
  Google's quota are marked *via Antigravity*.
- **Usage at a glance.** Each agent's quota windows sit in the thread header and at the
  foot of the sidebar, and update every turn.
- **Context left at a glance.** The header's gauge shows how much of the context window
  remains ("37% left"), with the tokens used and left in its tooltip. The bar turns amber at
  the warning point (75% used unless you change it; sooner if the agent auto-compacts
  earlier) and red at 90%. An agent that has not reported its window size shows a dim
  "context ?" instead of a guess. Click the gauge to compact, continue in a new thread, or
  open the `/context` breakdown; see the [reference](reference.md#the-context-gauge).
- **An offer before the context fills.** The first time a thread passes the warning point, a
  note above the message box offers to compact it or continue in a new thread. Nothing is sent
  until you press one; it does not come back until the context drops below the point (after a
  compaction, say) and crosses it again. Set the point, or turn the offer off, in Settings →
  General → Threads.
- **The Local agent's gauge.** A Local thread's gauge measures against the context budget
  set for its profile, not the model's own window, so it reads "~37% left" and says it is an
  estimate.
- `/rewind` undoes the last turn's changes.
- **Export a thread.** Right-click a thread and pick **Export…** to save the whole conversation
  as a Markdown or JSON file, wherever you choose. Each message carries the agent that wrote it, a
  divider marks every agent switch and notice, and each tool shows its title and, if you leave
  **Include tool output** on, its output (cut at 20,000 characters per tool, with a note).
  **Include reasoning** is off by default. Images appear only as `[image: name]`. Secrets
  matching known patterns are masked; one in another format may remain. The file is written
  privately (readable only by you) and atomically. The entry is greyed out for a thread with no
  messages.

### Approvals and rules

- **One card for every agent.** **Allow** (once), **Allow for session**, **Always allow**,
  **Deny** and **Stop turn** mean the same thing for Claude, Antigravity and Codex. Deny
  lets the agent see the refusal and continue; Stop turn ends the turn.
- **Rules live in Agent Terminal**, in `~/.local/state/agent-terminal/approval-rules.json`,
  and answer matching requests from every agent. Always rules match across agents by kind:
  a shell command, one file inside the workspace, or one MCP tool. Session rules end when
  the thread switches agent, and the switch notice says how many ended.
- **Automatic Pattern Approvals** (Settings → Agents, on by default) decides whether saved
  **Always allow** rules approve matching requests without a card. Switched off, every
  request asks, except what you allowed for the session, **Always allow** is not offered, and
  an answer of Always on a card already showing allows that one call and keeps nothing. Your
  saved rules stay in the file either way. Rules you exported into an agent's own settings are
  applied by that agent, not by this switch.
- **Your rules mean what you write.** `git *` covers every git command and `cargo test *`
  every `cargo test`. When a rule allows more than reading (a program that runs code, an
  option such as `git -c`, a file outside the workspace), a notice says so when it is kept.
  A command with shell syntax (`;`, `&&`, `|`, `$(`, redirection, …) never matches a rule,
  and a secret is never written into one.
- **Allow for session on a read covers the next read.** For read-only programs (`ls`,
  `cat`, `grep`, `rg`, `git status`/`log`/`diff`, …) the session grant keeps the program
  for the workspace, as long as every path it names stays inside it and away from `.git`,
  secret-looking files and home dotfiles.
- **Allow for session on an edit covers its folder.** The grant keeps the edited file's
  folder and everything below it, and the card names the folder before you choose. It stays
  inside the workspace (or at least two levels inside your home folder), never covers `/` or
  your home folder, and sensitive files inside it still ask. **Always allow** keeps the one
  file.
- **Answer from the keyboard.** While an approval waits in the focused thread:
  `Ctrl+Enter` allows, `Ctrl+Shift+Enter` allows for the session, `Ctrl+Alt+Enter` always
  allows, `Ctrl+Backspace` denies and `Ctrl+.` stops the turn. A Codex permission request
  can only be denied or stopped from the keyboard.
- **Import and export.** Settings → Agents imports the rules your agents already have
  (Claude, Antigravity, Codex's `~/.codex/rules` and any permission files you list) and
  exports Agent Terminal's global rules back. **View Differences…** shows what is not yet
  in step. **Manage Rules…** lists the saved rules, and removes any you no longer want.
- **Codex requests.** When Codex asks for extra filesystem or network access, the card
  lists exactly what it asks for. A session grant is offered only for places Codex cannot
  rename later, and never covers the filesystem root, your home folder or a project's
  settings. A header chip shows extra access while it lasts. Tool-server forms and sign-in
  links become cards too; nothing is sent or opened until you choose.
- **Secrets are masked** in approval cards, tool cards, questions and notices, not only in
  the store.

### Git safety net

- **Turn checkpoints**: in a git repository, the working tree is snapshotted into
  hidden refs when a turn ends. Your branch, index and stash stay as they were.
  See [Turn checkpoints](reference.md#turn-checkpoints).
- **Fork baselines**: a fork snapshots its source's working tree the same way, under its own
  hidden ref, so the compare window can show each side's changes since the fork. It goes with
  the fork. See [Changes since the fork](reference.md#changes-since-the-fork).
- **Diff panel** (`Ctrl+Shift+D`): uncommitted work, the last turn, everything since
  the thread opened, or the target branch, with files grouped under their folder and each
  file's diff on its own row. Open any file in your own diff tool, or in your editor at the
  first changed line or, with Ctrl+click, at any line of its diff. A read's card and the file
  links in a reply (`src/a.rs:12`) open in the editor too; a file outside the thread's folder
  is asked about first, every time.
  Tick the files you want and **Commit…** them through your own git, hooks and signing
  included, after seeing the exact commands. Select lines and **Comment** on them
  (`Ctrl+Alt+M`), in the panel or on a file change in the thread, keep collecting notes while
  you read, then **Send Review** to the thread's agent as one message with exact paths, lines
  and quoted code, each comment's lines checked against the file first, so lines that moved
  or went are said to have. See
  [Diff panel](reference.md#diff-panel), [Committing](reference.md#committing) and
  [Reviewing a diff](reference.md#reviewing-a-diff).
- **Undo a turn**, which saves the current state first, so the undo can itself be undone.
  See [Undoing a turn](reference.md#undoing-a-turn).
- **Worktrees** (`Ctrl+Shift+G`): a new branch in its own checkout, so an agent can
  work without touching yours. See [Worktree threads](reference.md#worktree-threads).
  When the work is done, **Merge into <base>…** in its Changes panel merges the branch back
  after showing the exact command, refusing anything half-done, and undoes a merge that stops
  part-way. The branch is always kept. See [Merging back](reference.md#merging-back).

### Terminals

- **Terminal drawer** (`` Ctrl+` ``): your `$SHELL` in the thread's folder, under the
  chat.
- **Send Selection / Last Output to Thread**: right-click a drawer or a terminal page and the
  selected text, or the last command's output, is added to a thread's draft as a labelled,
  redacted, size-capped block: a drawer's own thread, or from a terminal page the thread last
  shown, which comes into view. You review it before sending; nothing goes on its own. Details
  in the [reference](reference.md).
- **Terminal threads**: **+** → New Terminal Thread runs any configured CLI in a full
  terminal page, for tools with no chat support (a script, the Gemini CLI). These have
  scrollback search (`Ctrl+Shift+F`), Sixel images and clickable links.
- **A dead session keeps its page.** A CLI that exits with an error leaves its
  scrollback and the error in place, with **Restart** and **Close**.
- **Remote machines**: list the machines you reach with `ssh` in Settings → General → Remote
  Machines, then **+** → New Terminal Thread on Another Machine… opens a terminal page in a
  folder there (`~` works), titled `machine:folder`. It is your own `ssh`, with your own
  configuration and keys, never with port, agent or X11 forwarding, and nothing connects until
  you ask. See [Remote machines](reference.md#remote-machines).

### Appearance and behaviour

- **Themes**, in Settings → General → Appearance:
  - **Agent Terminal** (default), Dracula, Nord, Gruvbox Dark, Solarized Dark, One Dark
    and Monokai.
  - **Agent Terminal Light**, Solarized Light, Gruvbox Light, One Light and GitHub
    Light turn the whole window light, code blocks included.
  - Each applies live to the whole app: threads, terminals and diffs.
  - **Use System Theme** has no colours of its own. It takes them from your desktop's
    GTK theme (light or dark, its accent colour, any `~/.config/gtk-4.0/gtk.css`) and
    follows it as it changes. Terminals get the desktop's text and background with
    VTE's standard colours.
  - **Match Desktop: Light and Dark** switches between two of the bundled schemes as your
    desktop switches between light and dark, live. Pick them in the **Light Theme** and
    **Dark Theme** rows that appear below it (Agent Terminal Light and Agent Terminal at
    first); each lists only the schemes of its kind.
- **Notifications** when a thread you aren't looking at finishes, needs your approval,
  or runs out of quota. For a terminal thread, the quota notification has a **Continue in…**
  button; a chat thread offers it in the thread itself.
- **Settings apply as you change them**, to every window, and are saved to
  `~/.config/agent-terminal/config.json`. Hand edits to that file are picked up while
  the app runs.
- **Status indicators**: optional header lights driven by a file or a command. See
  [Status indicators](reference.md#status-indicators).
- **Updates**: ☰ → **Check for Updates…** asks GitHub for the latest release and shows its
  notes, with a Download button. The same check runs once a few seconds after startup (turn
  it off in Settings → Startup); a newer release puts a dot on the menu button. It is the
  app's only connection to the internet.

