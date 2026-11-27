# Agent Terminal: Developer Guidelines

Architectural mandates, standards, and workflows for this codebase.

## 🏗️ Architectural Mandates

- **Standalone Philosophy**: a single, standalone binary. All assets (SVG, icons)
  are embedded via `include_str!`/`include_bytes!`. Do NOT introduce external
  runtime dependencies.
- **Bundled icons**: All icons are bundled (`assets/icons`, GResource); never rely on the
  system icon theme for a name; the icon test (`tests/icons.rs`) guards it. See the rendering
  note in `THIRD_PARTY.md` before editing an SVG there.
- **Accessibility**: F6/Shift+F6 (`window/imp/regions.rs`, matched in the window's capture
  phase, since a `GtkPaned` binds F6 and would take it in the bubble phase; never an
  application accelerator; left to the focused widget when that is in a terminal, so a
  terminal program keeps F6) and Ctrl+F6/Ctrl+Shift+F6 (`win.focus-next-region`/
  `-previous-region`) cycle the shown regions: sidebar (Navigation "Threads", only while
  shown), transcript (Region), composer (Region), terminal (a thread's drawer box or a terminal
  page's terminal and search bar), docked diff panel; none while a dialog is open or the window
  closes, and leaving an overlaid sidebar hides it once another region has the focus (never
  when none took it). A new pane needs a role, a label and an entry there. Display tests press
  real keys through `testutil::XKeyboard` (XTest, libXtst loaded with `dlopen`; a test without
  it says so and skips on a developer machine, and fails on CI, where `libxtst6` is installed). The text scale (`font_scale`) is the whole app's: one provider,
  `style::set_text_scale` (`window { font-size: N% }`), so app CSS font sizes are relative, never
  `pt`/`px`; it is held to `style::TEXT_SCALE_RANGE` wherever it comes in (the setting, a zoom,
  `TerminalConfig::normalize` on every load and reload). Every transcript row's root is a
  `ListItem` (`cards::list_item`; a plan's expander sits inside one) named by `cards::row_label`
  (every `Body` variant has a name) and busy per `cards::row_busy`; a new `Body` variant must
  name itself there. A filled `button.suggested-action` rings outside its fill
  (`outline-offset: 2px`), in the global stylesheet (`main.rs`'s `GLOBAL_CSS`), so every dialog
  and page has it. Custom sidebar, diff and Settings controls keep explicit focus rings.
  Desktop high contrast temporarily uses System colours without changing the saved theme;
  `palette::concrete` resolves that override for terminal and diff consumers too. Secondary
  text uses the desktop foreground in high contrast, with light/dark 7:1 regression checks.
  A running menu animation stops at its target when reduced motion is enabled.
- **Native GTK4/Libadwaita**: a native Rust binary. Minimum system libraries are
  set by the Cargo version features, currently GTK 4.10 (`v4_10`, for
  `FileDialog`), libadwaita 1.5 (`v1_5`, for `AlertDialog`/`PreferencesDialog`)
  and VTE 0.72 (`v0_72`, for `text_range_format`). Raising any of them is a
  deliberate decision, not a side effect.
- **No redundant GTK crates**: `glib`, `gio` and `gdk4` are reached through the
  `gtk4` re-exports (`gtk4::glib`, …). Do not add them as direct dependencies —
  that only creates a second place for versions to drift.
- **Chat-first**: the primary surface is a chat thread (`src/chat/`) that
  drives Claude, agy, Codex or the Local agent (a model on the user's own server, run by
  the app itself) through a structured adapter (`agent-core`) over a gio
  process transport (`src/agent_proc.rs`). Every new-session action opens a
  thread. Terminal pages remain for any profile and for CLIs with no
  adapter.
- **PTY Bridge**: terminal pages and a thread's drawer use `vte4`, running the
  configured CLI or the user's `$SHELL`.
- **PTY Isolation**: sessions inherit the user's interactive environment (`-ic`)
  and full environment variables so tooling (nvm, aliases) stays available.
  `TERM`/`COLORTERM` are forced so the CLI renders its full TUI. The block built from them
  (minus `clear_env`, plus the env file) is the child's whole environment
  (`spawn_with_exact_env`, `VTE_SPAWN_NO_PARENT_ENVV`): with VTE's default flags every variable
  removed from it was added back from the app's own.
- **`exec`, and nothing before it**: the startup command is `exec <cli>` so the
  CLI owns the controlling terminal. Never emit banner or `clear` output into the
  TTY beforehand — it disrupts the CLI's initial terminal handshake. Anything
  resembling a startup script must contribute *environment*, merged into the
  spawn environment, never bytes written to the terminal. The one text before `exec` is a
  terminal tab's model provider (`utils::apply_provider_env`): the user's rc runs after the
  spawn environment and may export the vendor's variables again, so POSIX `unset`/`export`
  builtins put the provider's back. It names variables only, never a value: each value comes
  in the spawn environment under a private `__AT_PROVIDER_` name that is copied and then
  unset. The key is not among them: it is in the tab's key file. A shell that is not POSIX
  (fish, csh) refuses such a tab.
- **Threads over a hidden tab view**: the window is an `adw::OverlaySplitView`
  whose sidebar lists the store's threads. Its content is an `adw::TabView` with
  no tab bar: one page per open thread or terminal. Each page is tracked in an
  explicit `TabState` registry (`src/window/imp.rs`); a thread page has
  `chat: Some(ChatTab)` and its `terminal` is the drawer's shell. Never look a
  page up by walking the widget tree. A page's directory is fixed for its
  lifetime. A thread is built (view replayed from the store, then its
  `ChatSession` started with `view.sink()`) only when it is first shown.
- **Module layout**: a Cargo workspace. The root package is the GTK app, so
  `cargo build --release` and `cargo deb` at the root still produce the one
  shipped binary; `crates/*` are path libraries with no GTK.
  - `crates/agent-core` (pure: no GTK, no process or file I/O): `redact`
    (secret masking applied to every hand-off brief, every injected handoff
    and every persisted event), `event` (the canonical provider-neutral event
    stream), `caps` (per-agent capability matrix), `adapter` (the sans-I/O
    adapter contract), `git_command` (a `git`/`gh` command line read as its operation for the
    approval card: a reading aid, never a gate; the card still shows the command),
    `agent_command` (a shell command that launches an agent CLI, `cross-agent`, `claude -p`,
    `codex exec` or `agy -p`, read from a command line or a shell tool's input, so it can be shown
    as a sub-agent; a reading aid too, though a delegated child's authority refuses what it
    recognises), `provider` (a profile's local or self-hosted model server, Ollama, LM Studio or
    LiteLLM: what Claude and Codex are started with to use it, and its model list read) and the
    per-agent adapters and pure policies built on it, `local_agent` among them (the Local
    agent's runner arguments, tools, mode gate, request body and streamed-reply reader),
    `local_context` (its instruction settings, precedence, prompt composition and the
    whole-request budget planner) and `local_mcp` (its MCP server definitions, tool identity
    and aliases, the mode gate and the revision-bound approval card) and `sensitive` (the
    file and folder names an edit or read of always asks about, shared by agy's hook policy and
    the Local agent's gate) and `export` (a thread as Markdown or JSON: agent labels and
    dividers, capped fenced tool output, a final redaction pass; `agent-kit`'s `export` reads,
    renders and writes it `0600` and atomically) and `remote` (machines reached with the user's
    `ssh`: host validation and `listed`, the remote folder, POSIX quoting and the exact `ssh`
    argv of a terminal page, of the reachability check and of a remote thread's agent
    (`ssh_chat_argv`), the probe and folder-check command lines and their parsers,
    `ThreadFolder`, which reads a stored `cwd` as `Local` or `Remote` (`ssh://<alias><abs path>`,
    an unreadable `ssh://` value an error, never a local path), `chat_cli` (Claude and Codex
    only), `approval_options` (Allow once and Deny) and `Subsystem::ALL`/`support`, what a remote
    thread has of each part) and `search_query` (the sidebar search's one
    grammar, `SearchQuery`: plain words, and `#tag` words that filter by tag, ANDed; `compile`
    makes only the plain words an FTS5 query: quoted phrases, the last a prefix, never a syntax
    error) and `thread_tag` (a thread's tags: one lowercased word of at most 32 characters, at
    most 8 a thread, refused, never masked, when redaction would change it; and `TagColour`, a
    tag's optional colour as a theme token)
    and `saved_prompts` (the user's saved prompts: name, body and folder rules, the typeahead's
    items, and the one placeholder resolver, `parse`/`resolve`/`Values`, which redacts every
    value before it caps it) and `commit_message` (a diff-panel commit's message check, its
    draft prompt and the shown command line) and `context_meter` (the header gauge's three
    states and wording, the `ContextWarning` threshold setting, the `WarnLatch` that offers
    once per crossing, and the popover's and the offer's actions) and `review` (comments on
    diff lines composed into one review message: each quote redacted whole before it is
    capped, then fenced; notes and paths redacted, control characters out of paths; the whole
    within the hand-off budget, the comments past it left for the next review; `locate`, a
    new-side comment's lines in the working file at send time: exact, moved once, gone, or
    `Unchecked` with its reason, never "still there" for a file not read) and
    `terminal_excerpt` (terminal text for a thread's draft: control bytes stripped, redacted
    before it is cut to 200 lines and 12,000 characters, fenced under a label naming its
    `Source`, and `last_output_range`, the last-output heuristic, which says when it fell
    back) and `compare`
    (two to four transcripts read side by side, whatever made them: each column a list of
    `Entry`s from a `Side`, a thread and the sequence number after which it counts; `table`
    lines them up by turn, a turn being a prompt and what follows it, with an `Earlier` row for
    what came before a column's first prompt; summaries and the text cap) and `fork_compare`
    (a fork's `ForkLink`, its source's id and last stored seq at the fork and its baseline's tree
    and commit ids, read leniently: a link from an earlier build has no baseline, and a baseline
    that is not two object ids is dropped, never the link; and
    `pairings`: a thread pairs with its source, its forks and its source's other forks, a fork of
    a fork with its own source only, never a thread that is gone; `Pairing::bases`: a fork and
    its source share the fork's baseline, two siblings each have their own; Keep This One's
    record, `ForkLink::kept` (`Kept`: the thread kept and the one archived, read leniently, so
    anything but two distinct ids reads as none, never losing the link), written by `settle` on
    the fork's link, both for two siblings (`SettleRefused` for no pair, a pair settled already,
    or a link holding another pair's record, which is never overwritten), cleared by `unsettle`,
    and read by `Pairing::kept` only when it names exactly that pair; `branch_slug`, a fork
    worktree's branch words), `ask_several` (parallel multi-agent group validation,
    compare-group metadata, target resolution and kept-answer handoff brief), `schedule`
    (one-shot and repeating schedule state machine, rule evaluation, missed-run policies,
    unattended/budget auto-pause rules and local-clock formatting) and `a11y_announce`
    (what a screen reader is told of an event, given whether the user was looking and whether a
    desktop notification already said it: an exhaustive match, so a new `Event` variant must
    decide; each part redacted, cut short, never stored, through `plain`, which transcript rows'
    names use too; what waits is bounded, `Pending`, and read as at most three items and "And N
    more." within a character ceiling (room for the count kept only when there is more than
    one); the window shows it in its off-screen live regions,
    `window/imp/announce.rs`: one `Status` label, polite, set after a 200 ms pause so a burst is
    one update, and one `Alert` label for requests, set at idle, each only by its text (the
    label's own accessible name), only while `announce_events` is on and never once the window
    is closing, what waits then included).
  - `crates/agent-kit` (GTK-free; blocking process and file I/O): `git`
    (git plumbing for turn checkpoints and diffs: shells out, but its parsers
    and filters are pure; `diff_trees` is the one tree-to-tree diff, capped, driver-free;
    `git/fork.rs` is a fork's baseline: `take_fork_baseline` snapshots and commits the source's
    working tree as a checkpoint does, `keep_fork_baseline` points
    `refs/agent-terminal/fork/<thread>` at it once the fork is stored, `drop_fork_baseline`
    deletes it with the fork only while it still points there, and `fork_changes` diffs a fresh
    snapshot of a side's folder against it; checkpoint pruning never takes a fork ref;
    `lay_in_fork_worktree` lays a baseline's tree into a fresh fork worktree as uncommitted
    changes only, `read-tree -u --reset` then `reset -q` there, the paths whose names look like
    secrets and those the source's index marks assume-unchanged or skip-worktree taking HEAD's
    content (a submodule's gitlink too) through a private index with the snapshot's config
    overrides, so nothing of the app's lands on a branch and no such file is copied; what it
    laid in, recorded as a commit (`LaidIn`), is the fork's baseline and what its ref keeps,
    never the raw snapshot; it writes only a worktree of its own (its canonical toplevel the
    worktree, its git dir not the source's), and sets the HEAD-kept paths in a bounded number of
    git processes, `with_entries_from`: an `ls-tree` per chunk and one `update-index -z
    --index-info` on stdin; those paths go in `ForkLink::not_compared`, which `fork_changes`
    leaves out of both sides' diffs and the Changes tab counts), `diff` (diff bases, numstat, line classification,
    truncation, the hunk line map, a selection's comment span and a comment's anchor in a
    later diff: exact, moved once, or gone, never a guess; `card_files`/`card_comment_span`, a
    file-change card's diff of several files cut by its headers, one file a comment, and an
    `@@ edit @@` fragment refused for having no numbers), `review_check` (a review's comments
    looked for in their working files at send time, blocking, run off the main thread: a file
    read only inside the thread's folder, every link followed so one that leads out is outside,
    opened component-by-component with `O_NOFOLLOW`, regular, at most 4 MiB, text; a missing
    one has lost its lines; `repo_path` names a card's file from the thread's repository or
    folder), `restore` (undo: pin, restore, contained deletion, check),
    `worktree` (worktree locations, branch checks, create and clean-only
    remove, and `removal_blocker`, why a worktree's removal is not offered: not registered on its
    branch, not clean, or git cannot say; `head_branch`, a folder's branch or why there is none,
    for a saved prompt's `{branch}`; a fork's own worktree: `plan_fork`, read-only, the first free
    `fork/<slug>-<n>`, its folder and the source's HEAD commit and branch, and `add_fork`, which
    refuses a source that moved to another commit or branch since, plans the name again once if
    it was taken, takes the fork's baseline, puts the branch at the source's HEAD commit with
    `add` and lays the baseline in with `lay_in_fork_worktree`, so the fork starts with the
    source's uncommitted work, uncommitted, and never a secret-looking file; `remove` refuses
    uncommitted and untracked files but deletes ignored ones, so every caller runs
    `removal_blocker` in the same step), `commit` (the diff panel's commit of the ticked paths
    or selected diff hunks via a temporary git index: its context, arguments and run), `merge`
    (a worktree branch—or closed worker branch—merged back into its base via Merge commit,
    Squash and commit or Rebase and merge: the read-only `preflight`/`preflight_branch`, the one
    `commands_for` list the preview shows and the merge runs, and the abort-only
    `merge_with_method`), `github` (a branch's pull-request state, the compare URL, the unforced push; it also gives
    a thread's subtitle its branch, shown only for a repository on GitHub),
    `handoff` (hand-off briefs, session readers, quota detection),
    `sessions` (`SessionFormat`, session IDs, transcript lookup and listing),
    `paths` (`~` expansion), `provider_models` (a model server's model list), `local_agent` and
    `local_tools` (the Local agent's runner: its chat client, loop, transcript and tools),
    `local_instructions` (its bounded instruction-file discovery and reads), `mcp_transport`
    (bounded JSON-RPC framing shared with the delegation bridge), `local_mcp` (`client`: the
    `rmcp` client on one runtime thread per session, discovery and calls; `process`: the exact
    child environment, credential masks and scrubbed stderr; `import`: the pure Codex
    `config.toml` reader), `mcp_guardian` (the `--local-mcp-guardian` role that owns each MCP
    process group), `attachments` (image normalising under decode limits, metadata dropped by
    encoding again, and the content-addressed `0600` blob store under
    `$XDG_DATA_HOME/agent-terminal/attachments`, read back only when its hash matches, swept of
    blobs no event refers to), `remote` (a remote thread's probe of its agent and folder check, each
    one batch `ssh` through `exec::run_command`, 15 s; a probe that succeeded kept in memory per
    machine and command until quit, a failure never kept) and `exec` (child processes with a
    timeout).
  - `agent-kit`'s `store` is the SQLite thread store (threads, provider
    threads, scrubbed events, app meta such as the open-thread list).
    `store_writer` is the one thread that writes it: the app's main thread
    queues every write there through a `StoreHandle` (never writing in place on
    a file store), reads that need their own writes wait on its barrier (and
    are not made at all if it does not answer: never read stale rows), its queue
    is bounded, and failures (refused writes included) are counted and shown.
    Split-secret heals use the in-memory tails of `heal_tail` only when they can
    vouch for every row of the item, else the SQL scan; redaction must never be
    weaker than the scan's.
    Schema v11 adds `threads.pin_rank` and the `thread_tags`, `search_items`, `search_state`,
    `schedules`, `spend_ledger` and `review_drafts` tables (empty until their features fill them,
    redacting text on write); two objects are made after open, never in a migration:
    `Store::ensure_search_fts` (the FTS5 index, by the writer before its first job; no FTS5 is
    `SearchIndex::Unavailable`, never a failed open) and `Store::ensure_spend_index`
    (`events_turns`, on first use).
    `store/search.rs` fills the search tables: `Store::index_thread` is the only way in (queued
    on the writer after a turn's or session's end, a user message and an import; in place on an
    inline store), folding the stored, scrubbed rows after the thread's watermark with the
    transcript fold (`TranscriptFold`) into user messages, replies and tool titles (never tool
    output, reasoning, notices or `raw`), redacted whole before the upsert (`ON CONFLICT DO
    UPDATE`); the watermark never passes an item that may still change. `search_backfill`
    indexes, from start-up, every thread `Store::search_pending` names, one writer job at a
    time, and is stopped before the writer shuts down (`close_store_writer`). A delegated
    worker's hidden backing thread is never indexed, listed as pending or found. `Store::search` (queries compiled by `agent_core::search_query`, which no input can
    make fail; its `#tag` terms never reach the `MATCH`, they keep the threads holding every
    one) is a read, run only through `store_job`.
    Pins and tags never touch `updated_at`, and a hidden backing thread is never pinned or
    tagged. `Store::set_tags` replaces a thread's tags in one write, checked again by
    `thread_tag::tag_set` (a refused set leaves the old one); `ThreadSummary::tags` comes in the
    same summary query. Tag colours are the `meta` key `tag_colours` (tag to `TagColour` token,
    never a literal colour; `set_tag_colours`), pruned of tags no thread holds whenever tags
    are set or a thread deleted. The sidebar draws a tag's colour only as a `tag-<token>` class
    over the `@at_*` names (`palette::tag_chip_css`), so `ThemeChoice::System` brings the
    desktop's; chip text is `@at_fg_strong` on the label itself, and `TAG_CHIP_TINT` is what
    `tag_chip_text_reads_on_every_tint` lets every scheme read at AA on every row state.
    `store/spend.rs` is the spending history: `Store::spend_rows` reads live threads' completed
    turns through `events_turns` (the query repeats its WHERE term with the literal
    `'turn_completed'`) plus `spend_ledger`, attributing a delegated worker's turns to its root
    thread's folder; it is a read, run only through `store_job`, after `ensure_spend_index`
    through the writer. `delete_thread` folds the deleted threads' turns (hidden worker threads
    included, read before their delegation rows go) into `spend_ledger` by each turn's own local
    day (`local_day`), inside the delete, so totals never drop and no turn is counted twice. The
    writer records `spend_cost_from_seq` once at its start (`ensure_spend_cost_marker`): a cost
    stored at or below it was Claude's running total, so it is withheld (tokens only), never
    summed; until the marker exists no cost is trusted (an in-memory store records it when made,
    and the Spending dialog's first write tries again). `spend_rows` reads the marker, the live
    turns and the ledger in one read transaction, so a delete committed meanwhile is never
    counted twice. The sums are `agent_core::spend` (`report`, `Period`/`Group` with guarded `ALL`
    arrays); kinds of spend (metered, list price, unknown billing) are never added together,
    and a turn with no cost is "not reported", never $0. Each `TurnCompleted` carries `model`
    and `billing` (serde-defaulted, so older stored turns read as unknown): the Claude adapter
    says what the process said of itself (`spend::claude_billing`: a key source other than
    `none`, or a cloud other than `firstParty`, is metered; signed in without a key on a plan
    is list price; less is unknown), and the session (`stamp_turn`) applies the profile it was
    launched on (`spend::turn_billing`: free to run is free, a model server is metered; set by
    `AgentLaunch::free`/`set_launched_free` and `launched_server`, a delegated worker's too).
    A free turn's reported cost is never summed. `Store::spend_budget_check` (a write, through
    the writer) sums today's and this month's real money only (`spend::real_money`: metered
    turns, in whole micro-dollars, the 80 % test `spent * 5 >= limit * 4`; Claude's extra
    usage is never counted, its unit being unconfirmed, and is shown only while switched on)
    and records each level warned of (80 %, 100 %) in `meta` (`spend_warned:day:<day>`,
    `spend_warned:month:<yyyy-mm>`, older keys removed) in the same transaction, so a level
    is warned of once a period across windows and restarts; it never stops a turn. A
    delegated worker's committed events reach the window through `AppServices::committed`
    (its sink reaches none), so its metered turn starts the check too.
    `store/review.rs` keeps a thread's unsent diff-review comments (`review_drafts`):
    `Store::replace_review_drafts` writes the whole list in one transaction (empty: sent or
    discarded), each quote redacted whole and each note redacted on write, queued on the writer
    in order so the last list wins; `Store::review_drafts` is read through `store_job` when a
    thread's diff panel is first bound to it, and nothing is written until that read has
    answered (a failed read leaves the stored list alone).
    `store/compare.rs` reads what a comparison needs: `ThreadAttributes::fork` is the `meta` key
    `thread_fork:<thread>` (in `thread_meta_keys`, so it goes with its thread; written only by
    Fork thread and `/fork`, never a compaction, a continuation or a switch, inside
    `create_thread_record`), `Store::transcript_at_fork` takes the hand-off's transcript and the
    fork point in one read, `Store::fork_pairings` names only threads that exist and are not
    hidden, and `Store::compare_columns` folds each side as a hand-off reads it (no reasoning,
    notices or inherited context) after its point, all in one read; `Store::fork_columns` reads
    those columns, each side's baseline and which one was kept together; each a read through
    `store_job`. `Store::keep_fork` (Keep This One, a write through the writer) archives the other
    thread, never deleting it, records the choice in the pair's links and, unless the kept
    thread's live session writes it right after, appends its notice (scrubbed like any event) in
    one transaction that checks everything again, answering `KeepOutcome::Refused` (nothing
    written) for no pair, a thread archived already, a settled pair or a link holding another
    pair; it returns the archived thread's worktree and who else works in it.
    `Store::worktree_occupant` is that question: any thread but one, archived ones and delegated
    workers' hidden ones included, its folder at or below the path, links resolved.
    `Store::undo_keep_fork` brings it back and clears the record.
  - The app (`src/`): `config.rs` (persisted settings + migration, re-exporting
    `SessionFormat`; per-agent defaults live on the profiles), `theme.rs`
    (colour schemes, infallible `RGBA::new`), `utils.rs` (detection and
    `SystemProbe::locate`, startup command, path resolution, indicators; it
    re-exports the kit helpers the window uses), `window/imp.rs` (GTK UI),
    `window/imp/threads.rs` (sidebar, thread pages, drawer, terminal
    pages; the sidebar draws from a thread list loaded off the main thread
    and every store read or write on the UI path goes through `store_job`),
    `window/imp/thread_menu.rs` (the sidebar's context menu and its actions;
    the menu itself is data in `window/sidebar_model.rs`), `window/imp/closing.rs` (closing a
    window waits for its agents' real exits with the window up, never by blocking at quit; past
    150 ms it shows the closing screen, drawn by `window/imp/pack_animation.rs`),
    `window/imp/agents_prefs.rs` (Settings → Agents; `agents_prefs/local_context.rs` and
    `local_mcp.rs` are the Local Instructions and Tools group: instruction files, budgets, MCP
    server definitions, discovery and the Codex import), `window/imp/spending.rs` (the Spending dialog: reads and sums in one `store_job`, then
    only draws; Settings → General → Spending, whose amounts are refused inline unless
    `spend::parse_budget` accepts them; and the budget check, started by a metered turn or a
    new extra-usage reading, run once at a time, its toasts what the writer recorded as due), `window/imp/remote.rs` (Settings →
    General → Remote Machines, the New Terminal Thread on Another Machine dialog and New Thread
    on Another Machine, which probes the agent and checks the folder off the main thread on
    Create),
    `window/imp/prompts_prefs.rs` (Settings → General → Saved Prompts: each field saved when
    committed and only when `saved_prompts::name_problem`/`validate_body`/`folder_problem` accept
    it, through `TerminalConfig::set_saved_prompt_*`; Add takes a whole valid prompt
    (`candidate_problem`); an invalid entry already in the file is listed with its problem and
    kept, never dropped; Delete offers Undo, which puts it back exactly, or is refused when the
    name now clashes or the list is full; a reload of the file that changes the prompts redraws
    an open group, after pending in-app saves are written),
    `window/imp/threads/message_search.rs` (the sidebar's "In Messages" section, behind the
    toggle beside its search entry: `Store::search` through `store_job` after typing pauses, a
    generation counter so only the newest search draws, "Search is unavailable" never shown as
    no match, a hit opened only for a thread in the loaded list, held as the page's
    `pending_jump`, which showing the page (`ensure_built`, in place of its move to the newest
    messages) or its build takes and passes to `ChatView::show_item`; a jump waits for its row
    to be laid out, never falling back to the end; its snippets go through
    `sidebar_model::snippet_markup`, which escapes every character and makes only the match
    markers bold), `window/sidebar_model.rs` (pure sidebar logic: sections, the `SearchQuery`
    filter on titles, folders and tags, tag chips and the Tags dialog's choices),
    `window/imp/commit.rs` (the diff panel's commit dialog and partial/whole staging flow;
    Use Last Reply records the original selection and transcript baseline, folds a bounded
    suffix through the store, requires a completed matching reply, and rechecks it on click),
    `window/imp/merge.rs` (worktree-tab, thread-menu and delegated worker-branch merge previews,
    base-checkout and worktree blockers, Ask Agent / Commit First follow-ups, method choice and
    result dialogs), `window/imp/ask_several.rs` (Ask Several parallel group creation,
    Compare Group view, participant status/actions and persisted budgeted answer adoption;
    Keep reserves the source through its committed write, and unsent answer withdrawal only
    clears the same fence. Sidebar comparison parents are loaded with summaries, while
    pinned and active rows keep their priority),
    `window/schedule_model.rs` / `window/imp/schedules.rs` (Scheduled Prompts editor, manager
    dialog and 60-second background tick scheduler with safe-mode gating and unattended/budget
    pauses; cached header/sidebar status distinguishes unreadable from absent schedules,
    current real-money warnings pause automatic work, and explicit Run Now leaves that
    pause intact), `window/compare.rs` (the
    compare window: an `adw::Window` of its own, never a page or a `TabState`, that only reads;
    it holds itself until closed or destroyed with its parent, so no caller keeps it;
    a spinner until its first read, a generation so only the newest read draws, Refresh, replies
    as guarded markdown; its Changes tab, shown once a caller begins a read of it, has a
    generation of its own and draws each side's diff with the diff panel's read-only file list,
    `diff_panel::read_only_files`; a footer under the columns shows each one's `KeepState`, its
    Keep This One calling the caller back with the column, `connect_keep`; it knows nothing of
    forks), `window/imp/fork_compare.rs`
    (Compare with… reads any two visible threads, including archived ones, from their first
    message; it offers neither fork changes nor Keep, and hidden delegated backing threads
    never enter its picker or columns. Compare with Fork…, `/compare` and the fork toast's
    Compare: the pairings and the picker,
    then each column read through `store_job` and each side's changes since its baseline through
    `off_main`; two sides in one folder are one diff under a note saying their changes cannot be
    told apart; and the fork's baseline: taken by Fork thread off the main thread, unless
    checkpoints are off, right after the fork point is read, its ref kept once the fork is
    stored and dropped when the fork is deleted, through its main checkout when it was a fork in
    its own worktree (`stored_baseline`: the worktree may be gone by then); Fork in Isolated
    Worktree… and `/fork worktree`,
    which plan the worktree off the main thread, ask, then fork into it as a worktree thread,
    taking the snapshot whatever the checkpoint setting, while plain `/fork` forks in place as
    before, a failure after the worktree is made naming the worktree and branch left behind
    (`NewThread::left_behind`, a `LeftBehind` guard: said with every failure shown and logged
    by any path that drops it before the thread opens); and Keep This One: both buttons disabled
    at the click (a `Reread` guard reads the window again on every path, a declined stop question
    included), the
    click, a confirmation over the compare window, Cancel the default, `Store::keep_fork` (a
    refusal said and the pair read again), the other thread's page closed only once that has
    landed, Undo (`KeepDone`), and the clean-only removal of the archived thread's worktree,
    offered only when no other thread and no open tab is in it and `removal_blocker` finds
    nothing, else a toast saying why, taken back by Undo whether shown or not; its Remove
    (`remove_kept_worktree`) checks, in one `store_job`, the open tabs' folders resolved
    (`worktree::any_inside`), that the thread is still archived, `worktree_occupant`,
    `removal_blocker` and only then `remove`; the page Keep closes (`close_row_for_keep`) makes
    no offer of its own, and the generic Remove of a closed worktree tab checks the tabs,
    `worktree_occupant` and `removal_blocker` the same way; it never merges),
    `window/diff_panel.rs` (the
    diff panel's widgets, its file tick boxes, "Commit…", "Open a PR" and "Merge into …" bars;
    it pops out into
    its own window, keeping its
    place, hidden, as its identity, and docks back when that window closes or its page leaves;
    `diff_panel/review.rs` is its comments on diff lines: the row's Comment button and
    Ctrl+Alt+M, the marks and list under a file's diff, re-anchoring on each diff shown, the
    comments the thread's file-change cards add (`ViewAction::ReviewComment`, named from the
    thread's folder—or repository—by
    the window off the main thread), and the review bar, whose Send Review
    first has the window run `review_check::check_in` off the main thread (panel comments against
    the panel's repository, card comments against the thread's folder/repository; the button a
    spinner, Send and
    Discard off; answers matched by comment id against the snapshotted anchored lines, so one
    deleted meanwhile is not sent, a panel refresh cannot shift asked lines, and one
    added meanwhile stays for the next review), then hands
    the one message to the panel's own thread with `ChatView::submit_review`, queued behind a
    running turn, never steered into it; `review_popover.rs` is the note editor and refusal
    popovers both use, parented to a box, never a text view, which would not let go of one it
    still holds when it goes),
    `main.rs` (app setup + logging, and the `agent_kit`
    imports that keep `crate::git::…` paths).
  - Chat: `chat/session.rs` (one thread's backend: adapter, process, store,
    switching, handoff), `chat/view/` (the GTK view; it only talks to
    `ChatBackend`; `view/images.rs` draws every image and the viewer, `view/tray.rs` the
    composer's staged images), `attachments.rs` (taking images in and loading them as textures,
    off the main thread), `agent_proc.rs` (gio process transport),
    `approval_server.rs` / `approval_hook.rs` (agy's approval socket and the
    `--approval-hook` client), `hook_config.rs` (is the hook installed in
    `~/.gemini/config/hooks.json`), `model_catalog.rs`, `account_status.rs`,
    `claude_probe.rs` / `codex_probe.rs` (prompt-less probes of Claude and
    `codex app-server`: models, usage and account), `probe.rs` (probe targets,
    coalescing, disconnectable listeners). Every probe and side process runs in
    the environment its agent's threads get (`AgentEnv`: profile env file,
    `clear_env`), so the indicator shows the account a thread would use.
  - Anything that needs `config.rs`, GTK or the command cache stays in the
    app; a crate never depends on the app.
- **Diffs and the external diff tool**: `agent-kit`'s `editdiff` (pure line differ and the
  Claude/agy/Codex edit readers), `filediff` (pre-turn baseline per turn, repo containment, private
  temp files under `$XDG_RUNTIME_DIR/agent-terminal/diff`) and `difftool` (config shape, presets,
  per-element placeholder substitution; never a shell). The app's `diff_tool.rs` launches it
  detached off the main thread. A file-change card compares the working file against the state
  recorded when its turn started (`window/imp/diffs.rs`), and falls back to the agent's own edit.
- **External editor**: argv only (`agent-kit`'s `editor`, started by the app's `editor_tool.rs`
  through the same detached launcher as the diff tool), placeholders filled per element, the
  program fixed in config. A path from the conversation (a read's card, a reply's link or
  `path:line` code: `editor::file_link`, `code_location`, `read_location`) is only parsed into a
  `Location`, never a command. Every transcript label's links go through
  `editor::link_action`: a local file (a path, or a `file:` URL in any case and slash form) opens
  in the editor, only `http(s)`/`mailto` reach the system's handler, and anything else is stopped,
  so no local path ever reaches GIO; a label of markdown outside a thread (release notes) uses
  `guard_links`. It is resolved with every link followed (`resolve_target`) and
  must be a regular file. A sensitive file is refused with a message, inside the folder or out.
  Inside the thread's folder it opens at once; outside it (a link leading out included) it opens
  only after an `adw::AlertDialog` naming the resolved file, Cancel the default, asked on every
  click and never remembered (`editor_tool::open_with`), and only if the path still resolves to
  that file when the editor starts (`contain`'s `confirmed`). With no editor set the file goes to
  `gtk::FileLauncher`, which cannot jump to a line. A diff-panel Ctrl+click opens the row's
  new-side line (`diff::new_line_at`) and a row with none opens nothing. Terminal editors are
  not offered (`EditorTool::terminal` stays unset).
- **Undo is itself undoable, and only touches what it pinned**: a restore
  always pins the current working tree first (even if unchanged), refuses to
  run if the tree moved since that pin, never touches HEAD or the index, never
  deletes outside the toplevel or through a symlinked directory, and leaves
  files checkpoints skip alone. `restore::tests` proves each of these.
- **Live GUI checks run in an isolated session**: use
  `dbus-run-session -- ./target/debug/agent-terminal`, or a Broadway display with its own
  D-Bus session and XDG homes, so a run never reaches a live instance or the user's settings.
  Clean up every process you start.
- **Worktrees are never removed by force**: removal is offered only for a
  clean worktree that no open tab is in, runs `git worktree remove` without
  `--force`, and never deletes the branch (a merge back neither: it gives the user the
  `git branch -D` command to run). Names reaching git are refused if
  they start with `-`, and paths follow `--`.
- **Uninstall removes everything the app put there, and only that**: `agent-terminal
  --uninstall` (also `make uninstall` and Settings → General) asks whether to remove the
  configuration and data (default yes); `agent_kit::uninstall` decides what that is. Its folders
  are removed only when real, owned by the user and named as the app's. Another tool's file is
  only reverted through what the app recorded (the agy registration journal, the Codex export
  header), never edited otherwise. In repositories only `refs/agent-terminal/*` and
  `<git dir>/agent-terminal` go, never a worktree or branch. It never runs while an instance
  does (the CLI refuses; Settings waits for `app.run()` to return). A package's maintainer
  scripts never touch a home folder: they only say where the data stays. A new place the app
  writes must be added to `uninstall::candidate_folders` or `plan`, with a test.
- **Checkpoints never touch the user's git state**: snapshots go through a
  private index file under the git dir, and are recorded only as refs under
  `refs/agent-terminal/`, created with an empty old value so none is
  overwritten. The user's index, HEAD, working tree and stash are left
  exactly as they were, except a commit the user confirms in the diff panel and a merge back
  from a worktree thread the user confirms after its preview;
  `agent-kit`'s `git::tests::repo` proves it, including with a split
  index and in a linked worktree. That commit (`agent-kit`'s `commit`, one of the two places the
  app writes the user's index or HEAD) shares no code path with checkpoints and never uses a
  private index: on an explicit click, after a dialog showing the exact commands, it stages and
  commits only the ticked paths (`git --literal-pathspecs add -A -- <paths>`, then `commit -m
  <message> -- <paths>`), so what the user staged elsewhere stays staged. Hooks, signing and
  the user's identity run as at any `git commit`: its arguments never carry `--no-verify`,
  `-n`, `--amend`, `--allow-empty`, `-a` or `--no-gpg-sign`, and the checkpoint identity is
  never substituted. It refuses during a merge, rebase, cherry-pick or revert, warns (never
  refuses) on the default branch or a detached HEAD, and asks again before a message that
  looks like a secret; failure text is redacted. Before staging it refuses a branch or HEAD
  that moved since the dialog read them (`Expected`), and a chosen path that is now a folder
  and not a submodule (`add -A` would take the folder whole). Names not valid UTF-8 cannot be
  ticked. Each step runs in a process group of its own (`exec::run_command_in_group`), killed
  whole on timeout before git is reaped, so no hook or helper outlives it. `commit::tests` prove these, and that
  `refs/agent-terminal/*` is untouched. The merge back (`agent-kit`'s `merge`, the other
  place) writes the base branch, index and working tree of the checkout that has the base
  checked out, and only there: on an explicit click, after a dialog that reads everything first
  (`preflight`: both tips, ahead and behind, a `git merge-tree --write-tree` prediction that is
  "unknown" when git cannot make it, the hooks that run and their folder, the linked worktrees)
  and shows the exact command, its settings and its folder (`commands`, the one list the merge
  runs), it runs `git merge --no-ff --no-edit -m "Merge branch '<branch>'" -- <commit>`: the
  exact commit the preview read (so no tag of the name, nor a commit made since, is taken), with
  `branch.<base>.mergeOptions` unset and `merge.ff`, `merge.log` and `merge.autoStash` false for
  that command, passed through git's configuration environment (`GIT_CONFIG_COUNT`, keys taken
  whole, so a branch named with `=` is no escape; `GIT_CONFIG_PARAMETERS` set empty), never with
  `--force`, `--no-verify`, `-X` or `-s`. Names with control, direction or invisible characters
  are refused (`precheck_name`). It refuses a worktree with uncommitted or untracked files, a base
  checkout with uncommitted changes, on another branch, detached or mid-operation, a predicted
  conflict, nothing to merge, this thread or anything in the base checkout working or about to
  (`window/imp/merge.rs`: open tabs in any window, paths resolved, other linked worktrees nested
  in it excluded, a thread's start, pending prompt or switch, a terminal page's or thread
  drawer's recent output or a foreground job other than its shell, live delegated sub-agents by
  their folder; an idle one only warns), another tab open in this worktree or one of this
  thread's sub-agents working there, and either tip, the base branch or the hooks that would run
  changed since the preview (read again just before it runs, and the tabs again at the click).
  Exit 0 counts only when nothing is in progress and HEAD is a new commit whose parents are the
  two shown. A merge git recorded as in progress (`MERGE_HEAD`) that stops part-way is undone
  with `git merge --abort`, and nothing else is ever undone: every other failure leaves the
  checkout exactly as git left it (nothing can tell what git wrote from the user's own changes
  made meanwhile), saying what state it is in and the commands the user can run, a discarding
  one said to discard; a left `index.lock` is never taken as restored. Failure text is redacted,
  the timeout says a prompting hook or signing cannot be answered from the app, and each step
  runs in its own process group like the commit. It never deletes the branch or the worktree.
  `merge::tests` prove these. Every git call strips inherited
  `GIT_DIR`-style variables and sets `GIT_OPTIONAL_LOCKS=0`. Untracked files
  whose names look like secrets are never captured. A fork's baseline is such a snapshot,
  recorded as `refs/agent-terminal/fork/<thread>` (empty old value), spared by checkpoint
  pruning and deleted with its fork; `git::fork::tests` prove it leaves the user's state alone
  and skips secret-looking files. A fork in its own worktree starts its new branch at the
  source's HEAD commit, never at that snapshot commit, and gets the snapshot only as uncommitted
  changes in the new worktree (`worktree::add_fork`, `git::lay_in_fork_worktree`): nothing the
  app authored lands on a branch that Open a PR or Merge into could publish.
  `worktree::tests::fork_worktree_*` prove it carries the uncommitted work, never a
  secret-looking (tracked or not), assume-unchanged, skip-worktree or ignored file, and leaves the
  user's index, HEAD and stash alone, and
  `store::compare::tests::keep_archives_the_other_and_never_removes_a_dirty_worktree` that Keep
  This One archives, never deletes, is checked again in its transaction, and never has a dirty
  worktree removed.
- **Hand-off briefs are the terminal's, not the CLI's**: a CLI out of quota
  cannot summarize itself, so briefs are built from disk. Every brief goes
  through `agent_core::redact::redact` and is written `0600` in a `0700` directory outside
  any project. Never add a brief source that skips either.
- **No feature flags for deployment specifics**: the crate has none.
  Deployment-specific checks are entries in the config-declared `indicators` list, so
  anyone gets the same capability and the shipped binary carries no site-specific knowledge.

## 🔒 Security & Robustness

- **agy runs only in dangerous permissive mode, and every risk of that is accepted**: headless
  agy has no working way to ask before it acts, so the app never binds it an approval socket
  (`bind_approval`, and a delegated agy in `child.rs`, both return `AGY_PERMISSIVE_ONLY`). Every
  agy process, thread or sub-agent, runs with `--dangerously-skip-permissions` alone
  (`OpenSession::forced_permissive`, no `--mode`). The thread is **forced** (`Inner::forced`,
  `Event::PermissionsForced`): the mode picker is locked on "Dangerous permissive (forced)", and
  the user's chosen mode is only kept and stored. Forced is not `Mode::Bypass`: it is never chosen,
  never stored, and never answers sub-agent cards (`allow_worker_requests`). With no socket, the
  hook proof and the canary never run. A delegated agy's own tool calls are not judged (its task
  mode and declared limits are not enforced); only its `cross_agent_*` bridge calls are. The
  adapter still strips the flag from profile arguments, with `--gemini_dir`/`--app_data_dir`, and
  the workspace scan (`agy_l2::scan`) still refuses planted agy configuration for delegation,
  since that guards the delegation capability. The approval-hook code that remains (approval
  server, `hook_config`, the canary) is the route any hooked process would take, and is
  exercised by tests only.
- **Dangerous permissive mode (`Mode::Bypass`) is confirmed, session-only and never a
  default**: picking it (dropdown or `/mode`) always shows the warning first, and Cancel changes
  nothing. No process starts in it (`ChatSession::with_env` applies `Mode::session_safe`), it is
  never stored as the thread's choice (`ModeChosen` keeps the last safe mode), a fork or handoff
  gets the safe mode, a switch to another agent ends it, and Settings never offers it. Natively:
  Claude `bypassPermissions` (entered by a resumed restart, since a live `setMode` is a no-op),
  Codex `never` + `dangerFullAccess`, agy the hook allowing everything; hookless agy refuses it
  (it already runs forced, which is not this mode: see the agy rule above).
  It never propagates to a delegated worker (its task mode stays inside the root's safe mode), but
  while the root is in it, the root's view answers each live worker approval that offers Allow
  (`allow_worker_requests`): once, keeping no rule, through the shelf's own route, so the worker's
  inherited authority is re-checked at the answer. Questions and forms still wait for the user.
  It stays last in the picker, below its divider. The session tests
  `dangerous_permissive_mode_*` prove these.
- **Every persisted or injected text is redacted**: the store scrubs every
  event (`scrub_envelope`) and title, and every handoff (cross-agent switch,
  fork, compact-by-handoff, brief) goes through `agent_core::redact` and the
  handoff budget. A new store write or injected prompt must take the same path. A saved
  prompt's placeholder values (`{file}`, `{selection}`, `{folder}`, `{branch}`) are redacted
  when they are given to `saved_prompts::Values` (a selection before it is cut to 8 KB),
  after their control, bidi and zero-width characters are taken out;
  the expanded text only fills the draft, never sends, and is then the user's own prompt.
  Terminal text sent to a thread ("Send Selection" or "Send Last Output", from a drawer or a
  terminal page, a remote one included) goes through `terminal_excerpt::format_excerpt`: only
  whole lines reach redaction, before any cut, never the tail of a PEM block whose BEGIN line
  the raw bound cut off (the rest of such a block is dropped), and it stays fenced under its "data, not
  instructions" label. It only fills a draft, never sends, and a terminal page's target is the
  thread last shown in its window (`last_chat_key`), never one opened for it; with none the
  actions are disabled.
  `{branch}` is read off the main thread (`ComposerHost::saved_branch_reader`, run by
  `gio::spawn_blocking`); the composer keeps the typed `/name` until it answers and uses the
  answer only if the draft is unchanged and no other prompt was chosen meanwhile
  (`saved_seq`). No repository, a detached HEAD or a failed read leaves `{branch}` to fill,
  with a toast saying why.
  A thread's tag is shown on every row, so one that redaction would change is refused (by the
  Tags dialog and again by `Store::set_tags`), never stored masked; tags are never injected
  into a prompt or a hand-off.
  A diff review (`agent_core::review::compose`) redacts every quote, note and path before it
  caps them, fences each quote (diff text the agent wrote is data, never instructions), stays
  within the hand-off budget and goes as one user message; its drafts are stored redacted.
  Only the thread's own transcript cards take comments (`Row::allow_comments`, called by the
  transcript alone): diffs of sub-agents in the inspector and of approvals take none, and the
  agent's own edit (`Shown::root` unset) takes none. The send-time check reads nothing into
  the message: only where the comment's own, already quoted lines are now.
- **Images are kept by ref, decoded under bounds, and never redacted, so never sent unasked**:
  an image a user attaches (picked, pasted, dropped) or an agent's tool returns is decoded under
  `agent_kit::attachments::normalise`'s limits (20 MB, 16 384 px a side, bounded allocation),
  resized to 2048 px, re-encoded (no metadata survives) and stored content-addressed (`0600`, in a
  `0700` `$XDG_DATA_HOME/agent-terminal/attachments`); only the re-encoded copy is ever shown or
  sent. A file Codex viewed (`imageView`) is read wherever it is, as a picked file is
  (`BlobStore::ingest_path`: regular only, under the size cap): Codex already gave it to its model,
  so showing it discloses nothing new. An image agy read is shown as the copy it gave its model:
  its transcript's `media` for the read's result step, kept only when it is a plain file directly
  in that conversation's `logs/media` folder (`agy_transcript::media_path`), read again at the
  step's DONE frame since stdout says nothing of it. Events, the store and hand-offs carry an `AttachmentRef`
  (its sha256 id, size, name),
  never bytes: `Event::ToolImage` (an adapter's image, base64 or a path) is transient, stored as a
  blob by the session and replaced by `Event::Attachments` before anything is persisted or shown,
  and the store refuses it; a `raw` frame over 256 KB is replaced by its size. Base64 never passes
  through `redact` (it would corrupt it). Because an image cannot be redacted, one reaches an agent
  only when the user attached it, with its thumbnail in view, and an agent that cannot take images
  (agy: its stream input is text only) keeps the message in the composer and says so, never
  sending the text alone. Hand-offs say `[image: name]`. The sweep (`threads::sweep_images`)
  removes a blob no event refers to once it is a day old, after a thread is deleted and at
  start-up, and only against the file store (an in-memory or demo store knows no thread's images).
  "Open in Image Viewer" hands another app a private copy under `$XDG_RUNTIME_DIR/agent-terminal/
  open`, removed at quit. No image is ever fetched from the network.
- **A root hears its sub-agents without polling, and never after a stop**: a child's finished
  run, failure or message to its root is posted in the root's thread as the app's notice and
  starts a turn with it (`delegation/wake.rs`, decided in `agent_core::delegation::wake`):
  redacted before it is cut to `WAKE_BUDGET`, coalesced, held while the root is busy, never sent
  again once a `cross_agent_wait`/`read` page showed it, a notice only after
  `MAX_CONSECUTIVE_WAKES` wakes without the user, and dropped once the user stops the root (until
  the user speaks) or its thread closes. A root in dangerous permissive mode is woken the same
  way (the cap on wakes without the user still applies).
- **Closing or losing one worker never leaves its siblings stale**: a `Close` or
  `GenerationExited` bumps the tree's authority epoch to fence what was in flight and revokes
  only that subtree; `commit_revoke` moves every other worker's token from the replaced epoch to
  the new one. Queued instructions and staged launches of siblings survive too: the store fails
  accepted work and cancels queued sends only inside the event's scope
  (`RevocationEffect::scope`) and carries the rest to the new epoch, `latch_revoke` drops staged
  bindings only in that scope, and work resumed after an `await` re-reads its epoch
  (`settled_epoch_for`), failing only when a revocation covered its own worker. Stop, a mode
  restriction and shutdown still cancel tree-wide. `closing_a_worker_leaves_its_siblings_current`
  proves it.
- **A parent's declared limit on a child is refused, never asked**: `cross_agent_spawn`'s optional
  `capabilities` (`edit`, `exec`, `network`) is all a child may do beyond reading. It only narrows
  the task mode (a plan child can hold `network` only, refused otherwise), a child of a restricted
  worker never holds more than it (none declared inherits it), and it is kept in the launch
  record (an unreadable one restores as none). A call outside it, or an unrecognised tool, is
  denied by `ChildAuthority::judge`, so neither the user's rules nor dangerous permissive mode can
  approve it. With nothing declared the task mode decides, and the user's shell and edit rules
  reach a writing child within it (`rule_may_apply`); a plan child has no network unless declared.
  `authority::tests::a_declared_limit_is_refused_and_no_rule_reaches_past_it` proves these.
  A Claude profile's optional `child_mcp_servers` (`agent_core::delegation::launch::ChildMcpServer`,
  default empty) may grant delegated Claude children explicit stdio MCP servers and exact
  callable tool names, snapshotted per process generation across the `--mcp-config` overlay
  (never `--tools`, never receiving the bridge token), `system/init` server read-back, and
  `ChildAuthority::judge`/`rule_may_apply` (operator-trusted in plan mode too, requiring
  `Capability::Network` when capabilities are declared; unlisted tools, extra servers, and
  other drivers remain denied).
  **Accepted exception:** every agy child runs forced into dangerous
  permissive mode like a thread does: none of its own tool calls reaches `judge` (no hook asks),
  so its task mode and declared limits are not enforced; only its `cross_agent_*` bridge calls
  stay judged. Its launch check accepts that shape (the skip flag with `hook_proven` false needs no
  hookless allow rules), but still refuses a workspace that plants agy configuration
  (`agy_l2::scan`), which guards the delegation capability, not the tool gate.
- **`show_image` shows the user an image file, and reads only what a read may without
  asking**: the bridge's ninth tool (`ToolName::ShowImage`, 1 to 5 paths, a caption each) needs
  no delegation (it is in `DEGRADED_TOOLS`) and is a read for `ChildAuthority::judge`, so a plan
  child, or one declared nothing, may call it. Each path is resolved once against the caller's
  working folder and judged as the Local agent's reads are (`local_tools::read_unasked`): a path
  that leads outside the folder, or to a sensitive or hard-linked file, would ask, and since a
  bridge call has no way to ask the user it is refused with that reason, never read. The rest is
  opened from the pinned folder without following a link and without blocking, read only when
  regular (a pipe or a device is refused), at most 20 MB, then decoded under the attachment
  bounds and stored off the main thread (`delegation/show_image.rs`). The images go on the
  call's own tool item as `Event::Attachments` with `Origin::AgentShown` (shown below its card;
  the caption, redacted, as the image's name): the item the call's invocation key names, else the
  thread's newest open `show_image` call (`correlation::show_image_item`), counted toward the
  turn's cap. The model is told one line per image (`Shown to the user: chart.png (1280×720)`, or
  why not), never the bytes. The agy registration adds its allow rule, but delegation never
  requires it (`agy_guard::IMAGE_ALLOW_RULE`), so an earlier registration still delegates.
- **A folder grant is session-only and never wider than its card**: "Allow for session" on an
  edit keeps the file's folder (`always_allow::session_folder`): inside the workspace, or at
  least two levels inside home outside it; never `/`, home or above, and nothing when home is
  unknown. Sensitive files inside it (git, hooks, agent config, instructions) still ask. The
  answer keeps the folder only if it is the one the card showed (`decide_as_shown`), and
  "Always allow" keeps one file. `always_allow::tests::session_edit_keeps_its_folder_and_nothing_more`
  proves each of these.
- **An idle thread out of sight is suspended, never ended**: after `idle_suspend_minutes`
  (default 15, 0 never; Settings → General → Threads) unseen and unheard from, the window's minute sweep
  (`suspend_idle_threads`) asks `ChatSession::suspend_if_idle`, which stops the agent process
  only when nothing would be lost (`Inner::suspend_blocker`: no turn, queue, approval, permission
  card, model change, handoff, rebind, Codex session grants, unresumable Codex thread, no
  conversation yet, Bypass, delegated worker, active provider workers; the window adds background
  work, delegated runs, a delegated sub-agent's open question and
  pending sends). A delegation root suspends only with its hooks' consent
  (`DelegationHooks::suspending`): a planned rotation (`rotate_before_stop`), so nothing is
  revoked, and the rebind around its resume posts no notice; it also refuses while sub-agents are
  alive. "A conversation" means a resumed start or a turn completed after a prompt (Claude's
  prompt-less init turn names no transcript). The stop is silent: no exit reaches the view or the
  store. A mode or same-agent model picked while suspended is kept for the resume (never Bypass);
  other controls say, unstored, that the agent is paused. The next prompt resumes the same native
  session (`ensure_alive`). A delegated worker is never stopped by that sweep; only its
  supervisor pauses it (`ChatSession::suspend_child_if_idle`, the same `suspend_blocker` checks
  with only the delegated-worker exclusion relaxed), when it has been idle for
  `idle_suspend_minutes` (`Supervisor::stop_idle_children`, 0 never) or when a start would pass
  `live_processes_app` (the least recently active idle worker goes first, its exit awaited; with
  none, the start is refused saying how many are idle and that `cross_agent_close` frees a
  slot, retryable while a run or a stop is in progress; a stop already under way is waited for,
  bounded). Admission reserves: an admitted start counts as a process until its own exists or it
  fails (`LaunchSlot`), and a worker an instruction is on its way to is never paused
  (`DeliveryHold`). The supervisor's own check comes first (no run, pending input, preparation, dispatch,
  queued delivery or open request, its process up and read back, and never with a live
  descendant), and its hooks consent with the same planned rotation, so nothing is revoked and
  the epoch is unchanged. Silent too: its next instruction resumes the same native session
  through `ensure_child`. `capacity_tests` (supervisor and session) prove these.
- **A thread stopped by its usage limit continues once it resets, unless told not to**: with
  `continue_after_limit` on (default; Settings → General → Threads), a root thread whose turn failed for
  quota (never a rate limit alone, which may be a tool's own 429, nor a usage reading, which may
  name a window the thread does not use) waits (`agent_core::limit_wait`): until the reset time a
  rate limit in that turn gave, plus a minute, or, with none, until a prompt-less usage probe
  taken after the limit shows every window that was exhausted when the wait started free
  (probed at 2, 5, 10, then every 15 minutes, or just after the latest reset those windows
  name; any reading taken after the limit counts, so it may continue without a new probe). A
  window still shown full a minute past its own reset time counts as free (readings are
  merged, so a window the agent stopped reporting keeps its last copy). Such a guess is never
  ruled out for good, since an abandoned window would then hold the thread forever: a refusal
  backs off each copy already past its reset for as long as it had been stale (at least 15
  minutes), so the gaps never shrink and, once a copy has been stale longer than that minimum,
  every failed guess at least doubles the next one (a few failed turns spread ever wider, never
  a loop), and a copy whose usage drops or that names another reset is new. A
  refusal during a wait counts the same way, and readings from before it, or a reset time it
  came after, no longer count. A refused continuation holds the thread whatever its windows say
  (every one shown free, or none reported): the thread keeps a record from one wait to the next
  (`LimitRecord`, kept by "Don't Continue" and a new wait, reset only when the agent works
  again or the thread switches agent), and each continuation refused before the agent works
  again waits at least 15 minutes, then at least double the last gap, up to 6 hours, before the
  next (`LimitWait::back_off`); a refusal of the user's own retry keeps the last gap, never
  shrinking it. A wait no reading can end (no reset time, and no window reported, a blocking
  window missing from the readings, or one still full that names no reset time) continues blind
  15 minutes after its last refusal, and only once every time a window still holding it names
  has passed; a refusal of that guess is a refused continuation like any other, so the guesses
  double up to 6 hours. A thread whose process was launched on a model server
  (`ChatSession::launched_server`, never the live Settings), and the Local agent, read no
  vendor windows (another thread's reading of the vendor says nothing of them) and are never
  probed for one: their waits end that way.
  Readings count from when windows last arrived (`updated_at`), never an account-only
  update. agy's `/usage` side command never raises `RateLimited`: a reading is not a refusal. The window's minute sweep (`continue_after_limits`)
  then calls `ChatSession::continue_after_limit`, which sends the app's continue prompt as a
  notice in the same native session, starting the process again if it is gone. A handoff still
  waiting for a prompt rides on it and is spent as a user prompt's would be; the prompt is never
  stored as the user's message. Never into a running turn or anything else a wake waits for
  (`TurnPurpose::LimitContinue`), never for a delegated worker, never for a root the user
  stopped (Stop ends its wait, delegation or not, and the session refuses a continuation until
  the user speaks again; the stop inside "Stop and Switch" is not one, and a switch's
  continuation ends a stop that held the root). While the thread waits, sub-agent news does not wake it (`TurnPurpose::Wake`); it
  arrives after the continuation. The wait ends when the agent works again (`limit_over`, the
  one test for the window and the session), the thread switches agent or moves its agent to or
  from a model server (`AgentSource`: the agent and the server its process was launched on,
  `ChatSession::launched_server`, never the live Settings; the hand-off included; once the new agent is up, `LimitRecord::moved_on`, so a switch that
  rolls back keeps it, in the window and the session alike), the
  user stops it, the user picks "Don't Continue" on its banner, or the setting
  goes off. "Continue in …" on the limit's banner (and a bare `/handoff` while the thread is
  stopped by its limit) switches and then continues the work in the new agent with the handoff,
  as a busy switch does. `limit_wait::tests`, the session tests
  `a_thread_continues_after_its_limit_*` and `continuation_tests` prove these.
- **The low-context offer only offers, once per crossing, and keeps nothing**: when a live
  reading first passes `context_warning` (Settings → General → Threads: 60, 75 or 90% used,
  or never; any other stored value loads as 75), the view shows a note above the composer
  (`chat/view/context_offer.rs`) with Compact (only where the agent has its own command) and
  Continue in a New Thread. A press goes through the gauge popover's own path
  (`Inner::run_meter_action`: `/compact`, else `ViewAction::CompactByHandoff`, which already
  redacts and budgets the hand-off), never while a turn runs, and nothing is sent unpressed.
  `context_meter::WarnLatch` re-arms only on a reading under the point or a cleared gauge (a
  reading with no window changes nothing). A process started on another profile or model
  server, even on the same agent and model id (`ChatBackend::launch_source`), clears the
  gauge. A press on an offer already taken down does nothing. Replayed history primes it without offering, as
  does a changed threshold, and a crossing made while set to never is not offered later. The
  latch and the note live in the open view only, never in the store. The Local agent's gauge
  is the runner's `UsageUpdated` after each request (`estimated: true`): the server's prompt
  tokens when reported, else the planner's estimate, against the profile's `context_tokens`
  budget, labelled an estimate; its `/context` answers from memory. agy reports no window, so
  it shows tokens used and is never offered. `context_offer_warns_once_and_acts_only_on_click`
  and `context_meter::tests` prove these.
- **Agent processes get a cleaned environment**: `clear_env` and any inherited
  approval-socket variables are removed (`SpawnSpec::unset`), then the profile's
  env file, then its model provider's variables, then the approval socket.
- **A profile on a model server never falls back to its vendor**: a profile's `provider` (Settings
  → Agents → Model Provider, Claude and Codex only) starts the agent on that server
  (`agent_core::provider`: Claude through `ANTHROPIC_BASE_URL`, with the vendor's own credential
  variables removed, all pinned again by a `--settings` overlay, since a settings file's `env`
  wins over the process environment; the overlay never holds the key. The credential sent is
  always the profile's key or the placeholder, and the key is never in Claude's environment
  (every child Claude starts inherits it, scrubbed or not): each process (thread, worker,
  terminal tab) gets a key file of its own (`agent_kit::provider_key`, `0600`, `O_EXCL` and
  `O_NOFOLLOW`, in `0700` folders under `$XDG_RUNTIME_DIR/agent-terminal/provider-keys`, never
  `/tmp`: without a runtime directory the start is refused), written off the main thread right
  before the spawn (`agent_proc::write_key_file`; `provider::CLAUDE_KEY_VAR` only carries the key
  there and is never exported, and the variable the key was read from is unset) and removed
  when that process exits (`SpawnSpec::key_file`; a tab's on `child-exited`), and at quit, once
  the agents are gone, whatever of this instance's is left (`remove_own_key_files`). Files are
  named for a random instance id (`<instance>-<n>.key`, a taken name skipped), and each instance
  holds an exclusive `flock` on its `<instance>.lock` while it runs (`provider_key::Instance`):
  start-up sweeps only the files of instances whose lock can be taken, and their locks, so no pid
  namespace or reused pid makes a live instance's file look stale. **Residual risk:** the agent's
  own tools run as the same user, and the file's path is in Claude's argv, so the agent can read
  its own key: the file keeps the key out of environment dumps and away from other users, not
  away from the agent. The overlay's `apiKeyHelper` is `cat` of that file (no key: a
  constant printing the placeholder), it blanks every other credential (`CLAUDE_UNSET`) and
  `BASH_ENV`/`ENV` (so a settings file runs no code in the helper's shell), and a settings
  file's helper or tokens are never used (seen live with Claude Code 2.1.309: the helper of the
  overlay beats theirs and the process's token). A start whose helper was not bound to a file is
  refused (`provider::needs_key_file`). A profile's own
  `--settings` is merged under it (`provider::with_profile_settings`: inline JSON only, a file
  refuses the start, never dropped). Codex through a `-c`
  provider of the app's own id). Every chat launch path
  goes through `profile_launch` (threads, switches, forks, delegated workers), and a terminal tab
  through `utils::with_provider_args` and `apply_provider_env`: a provider that cannot be used (a
  bad address, a key variable that is unset) refuses the start. Such an agent lists the server's
  models or none (never its vendor's), and its vendor's account is never probed. Claude's own
  model names (`ANTHROPIC_MODEL`, the `ANTHROPIC_DEFAULT_*_MODEL` aliases, the subagent and
  auto-mode models: `provider::CLAUDE_MODEL_VARS`) are unset, then bound to the process's model
  at every spawn (`bind_thread_model`, and in the overlay `bind_thread_model_args`), so background tasks and subagents stay on the server; an
  unbound one is never exported. A model is chosen by `start_model`: the profile's own, else the
  server's first chat model (never the CLI's own setting, which names a vendor model); with none
  known, nothing starts (thread, switch, worker or terminal tab), and a terminal tab is given its
  model on the command line (`provider::model_args`). A model change on a server is a resumed
  restart (`binds_thread_model`): Claude Code refuses a live `set_model` to an unknown id. The
  catalogue keeps a list per source any enabled profile uses (`ModelCatalog::models_for`,
  `other_profile_sources`), a server keyed with the profile's environment file
  (`model_catalog::ServerSource`), since another file may hold another key: a thread's retirement judgement and a delegated worker's effort check
  use the list of the source its own profile runs (`profile_source`). Its picker shows one group
  per source (`picker_specs`, `ModelCatalog::list_state`): each agent's vendor and every enabled
  profile on a model server (every Local server) under its own name, kind and address, saying
  when it lists nothing (checking, not reachable, no chat model). A row of another profile's group
  moves the thread there (`ChatBackend::switch_profile`, `SwitchTarget`): always a new process on
  that profile continuing from a handoff, even on the same agent. The process carries the profile
  it was launched on (`AgentLaunch::profile`, `ChatSession::launched_profile`) and only its own
  start binds the thread to it (`launched_binding`): a refused or rolled-back move binds nothing,
  and the thread stays on its own profile. Until then the thread's profile is the running
  process's (`ChatTab::running_profile_id`) for the picker, a pick, a restart and a fork, and a
  rollback returns to the profile the previous process ran on. An API
  key is never stored: the profile names the variable holding it,
  Codex is handed only that name, and the key the model list needs is read before a launch
  removes the vendor's variables (`probe::ProviderKey`, never printed).
- **The Local agent is the app's own runner, gated like the CLIs**: `Driver::Local` threads run
  `agent-terminal --local-agent` (`agent_kit::local_agent`), whose pipes carry canonical
  `Command`/`Envelope` JSON lines (`agent_core::local_agent::LocalAdapter` is a codec). Each
  server is its own profile (`TerminalConfig::local_servers`), ready only when it lists a chat
  model (`AgentAvailability::server_ready`). Its tools (`agent_kit::local_tools`) pass
  `local_agent::gate` first: Plan offers and runs no writing tool, Ask asks before every write
  and command, Accept edits asks before commands and before any write outside the thread's
  folder (links followed, a dangling one counted outside), of a sensitive file (the agy hook's
  Accept edits list: `agent_core::sensitive`, and a home's dotfiles when the folder is that
  home or above it) or of a file with a second hard link (`local_tools::must_ask`). A mode
  change is applied as it arrives, in the middle of a turn too: the next gate and the next
  request use it, and a card open when it comes expires and its call is gated again. Reads
  follow the agy hook's read policy, except that a link staying inside the folder is judged
  where it leads: a read outside the folder, of a sensitive file (the same list, a file with a
  second hard link, and a home's dotfiles when the folder is that home or above it) or a search
  from a home or above asks in every mode, Plan included (`local_tools::read_must_ask`). Homes
  are `$HOME` and the account's own; with neither known, every dot-named path and every search
  asks. A call's path is resolved once (`local_tools::target`): that is what is judged, what its
  card names (where it leads, when that is not the path itself) and what runs
  (`local_tools::run_judged`). Resolved again just before it runs, a path that leads anywhere
  else is refused, an unapproved call that now must ask is refused, and what runs is opened
  from the pinned folder (from `/` for an approved call outside it) one component at a time
  without following a link (`open_inside`, `write_target`), so a folder swapped for a link
  meanwhile fails it, and one judged inside never lands outside. A search walks open folders
  (read with `fdopendir`, no `/proc` needed; opened only when their turn comes, so the open
  folders never outnumber the depth), never follows a link, and never reads a sensitive or
  hard-linked file or a home's dotfiles, even approved: inside the folder as reads judge them,
  outside it by the whole resolved path wherever the search started. What it withheld and what
  it could not open or read whole (too large) are counted in its result, never silently
  dropped (generated trees such as `.git` and `target` aside). A command's group is killed
  before its shell is reaped (never when someone else reaped it), so the group id is never one
  reused by another process. The runner keeps no approval: the app's own scoped rules answer
  the cards they cover. A write is whole or nothing: written to a new file beside its target
  (named `.agent-terminal-write.<name>.<pid>-<n>.tmp`, removed on every failure), which then
  replaces it (`renameat`), keeping its permissions, owner and group; a link there is never
  followed nor written through, a pipe never opened, and an unapproved one with a second hard
  link is refused (an approved one's card says only that name gets the new content). A file
  with one link that cannot be replaced (its folder not writable, or a mount of its own), or
  whose owner or group the new file cannot take, is written in place instead: opened first
  only as a reference (`O_PATH`), checked to be the regular file judged with one link, then
  reopened for writing through `/proc/self/fd` and checked again (without `/proc`, refused).
  One with a second link whose group cannot be kept is replaced readable by its owner only. An edit is made only to UTF-8 text. Files are opened without blocking and read only
  when regular, at most a fixed size. Commands run in their own process group, killed on Stop
  or timeout, their output capped and never waited on past a short grace. Stop ends a
  request's connection (`ChatClient::cancel`). A call from a reply that was cut off or broke
  off never runs, and every call of an interrupted reply gets an answer: one a Stop ended while
  it ran keeps what it gave back (a writing one is said to have an unknown outcome), never "not
  run". History is sent and kept in whole exchanges. Its conversation is a `0600` file under
  `$XDG_STATE_HOME/agent-terminal/local`, which uninstall removes with the state folder. The
  runner makes itself non-dumpable first thing (`PR_SET_DUMPABLE` 0; it refuses to start
  otherwise): its `/proc/<pid>/environ` and `mem` are closed to every other process of the user,
  its commands included, and non-root ptrace and core dumps are refused, since it holds the
  server key and the delegation capability. Removing the key variable once read only edits
  libc's copy (so commands do not inherit it); the kernel's copy of the starting environment
  stays, which non-dumpability is what keeps unreadable. **Residual risk:** non-dumpability
  closes the runner, not where the key came from: a key kept in the profile's environment file,
  or exported by the user's shell, stays readable there by the agent's commands, which run as the
  same user.
- **The Local agent delegates through its own client, never a bridge process**: its binding is
  the open binding (`launch::prepare_with`), which `LocalAdapter` takes through
  `Adapter::bind_launch` into exactly one startup frame (`Command::ConfigureLocal`'s `bridge`;
  never argv, never an environment, a start nobody staged carries none). The runner connects
  `agent_kit::delegation_client` with it, so the capability lives only in its memory: no
  command it runs, nor any MCP server, ever holds it (a user definition can neither run the
  bridge nor be named as it). The supervisor accepts a Local hello only from that generation's
  own process: the kernel's peer pid and the hello's must both be the runner's
  (`ops::local_peer_ok`), with one live connection per token; the client reconnects at most once
  (the supervisor does not count reconnects, so the pin is what bounds them). The
  `cross_agent_*` tools are the app's own (never MCP aliases),
  offered only while the client is connected (`cross_agent_capabilities` alone while degraded),
  with no card in any mode, a plan included: the supervisor judges each call, as for every
  other agent. Each call is shown first under a key the runner mints
  (`InvocationKey::Local`), with exactly the arguments sent, and the supervisor denies one not
  observed so (as for agy); arguments holding a credential the runner holds are never sent. A
  delegated child's frame also carries its policy (`LocalChildPolicy`): the runner asks the app
  before every write and command in every mode (`local_agent::child_gate`), so
  `ChildAuthority::judge` sees each one (its Accept edits is answered by the app inside the
  lease, `local_child_accept_edit`); its card carries `resolved_path`, the target the runner
  resolved and will write, which is judged with the path named (a missing one asks), and the
  runner refuses to act if the path leads elsewhere when it runs. It is offered only what its mode and declared capabilities
  allow, never widens its mode, refuses a frame with a user MCP server or without its binding,
  fails a turn whose model cannot call tools, and reports the policy it applied before its
  session starts; the child takes its task only once that is read back (`Readback`). A spawn
  on a model its server does not list, or says cannot call tools, is refused; a requested
  effort is ignored with a warning. Route admission waits on the four Local gates
  (`LocalBridgeBinding`, `LocalPeerPin`, `LocalAttestation`, `LocalChildSupervision`), proven
  by `local.proof.json`. A delegation root's new Local settings restart it only as a planned
  rotation the supervisor agreed to: while it has live sub-agents or busy work
  (`DelegationHooks::holds_restart`), or the supervisor refuses the rotation
  (`DelegationHooks::suspending`), they wait for a later prompt; a stop it did not agree to
  would revoke the root's tree. The one exception is a narrowing of its MCP authority, which
  stops the root at once, live sub-agents and all (its exit revokes them), and says so.
  `tests/local_delegation.rs` and `supervisor::tests::local_tests` prove these on the real
  runner. **Residual risk (not closed):** a process able to trace the runner (after
  non-dumpability, only root or one with `CAP_SYS_PTRACE`) can read its memory, the capability
  and the server key included, and drive it outright: the peer pin does not help against that,
  since such a process can act from inside the runner itself. Core dumps are closed by
  non-dumpability. Local attestation, and the child's policy read-back, are the runner vouching
  for itself: they prove consistency, not independence; a compromised runner can report
  whatever it likes. The model server is outside supervision: it receives the task, file
  contents and results, and a proxy may forward them to a cloud model or run tools of its own
  that no gate sees.
- **The Local agent's instructions and MCP tools are the user's, configured in the app, never
  found at launch**: the runner takes one validated `LocalLaunchContext` on stdin
  (`Command::ConfigureLocal`, before any prompt; never argv). Instruction files
  (`agent_kit::local_instructions`) are read once per turn, in order: the native
  `$XDG_CONFIG_HOME/agent-terminal/AGENTS.md`, the configured global files, then each ancestor
  of the canonical thread folder from `/` down (`AGENTS.override.md`, else `AGENTS.md`, else
  `CLAUDE.md` when the fallback is on). Symlinks are followed, but a file is opened without
  blocking, must be regular UTF-8 and is read at most to its cap; a configured file that is
  missing, oversize or over the static budget blocks the turn with its path and reason, never
  truncated. The composed system message is redacted, and no instruction can change the gate,
  the approvals or the environment. Every request is planned whole
  (`local_context::plan_request`): policy, instructions, schemas, newest whole exchanges and
  the response reserve; a call batch runs only when every result's room is reserved first, for
  whichever mode the next request is planned in (`reserve_batch_for_any_mode`).
  MCP servers (`local_defaults.mcp_servers`, selected per profile) are stdio child processes
  started only at a turn or by Settings → Discover tools, never by opening Settings or
  importing. Each runs under a guardian (`--local-mcp-guardian`) in its own process group,
  with a minimal exact environment (`PATH`, `HOME`, locale, `TMPDIR`, `XDG_RUNTIME_DIR`, plus
  the definition's references and literals; never a control variable), bounded frames (1 MiB),
  server requests (128 between responses, each refused; notifications are never kept: past
  128 between responses they are counted as dropped, and only past a rate of 2,000 or 8 MiB of
  them within 10 s does the connection end, so a stream cannot spin its reader while a server
  that logs now and then stays connected for as long as it idles),
  discovery (256 tools a server, 1,024 a session), schemas (64 KiB, compiled with a linear
  regex engine, no external `$ref`), arguments (64 KiB) and stderr tail (64 KiB). A turn's
  registry is frozen at its start: the model sees short aliases (`mcp_<hash>`), routed only
  through that registry, never a model-supplied server or tool name. Eligibility comes before
  approval: enabled in Settings, selected, allowlisted by exact name, advertised this turn,
  valid arguments, then the mode (`local_mcp::gate`: Plan runs only a tool the user declared
  read-only; Accept edits grants MCP nothing; Bypass skips only the prompt), then the policy
  (the server's for its reviewed allowlist, a tool override only narrows it, Deny always
  wins; server annotations grant nothing). A card shows the server, the original tool name
  and exactly the arguments that run, secrets masked in place
  (`agent_core::redact::structured`; a value under a name sensitive only through `KEY` or
  `TOKEN`, such as `sort_key` or `max_tokens`, is masked only when it looks like a credential:
  `redact_keyed_shown`); a call whose card cannot be shown faithfully is refused. Discovered
  names, descriptions and schemas are masked only for the exact resolved credentials, so the
  model sees the schema its validator compiled; a tool whose input schema holds one is left out.
  A pending approval is bound to its request id, the runner generation and the connection it
  was raised on: a disconnection or reconnection expires the card, and a late answer never
  authorises a replacement call. A session or saved grant is bound to the server definition's
  and the tool schema's content revisions, not to a connection: it survives a reconnection
  that advertises the same content, and a changed definition or schema asks again
  (`tool_content_revisions_survive_reconnect_and_unchanged_notifications_but_track_raw_metadata`
  and `disconnect_expires_pending_cards_and_late_answers_never_authorise_reconnection` prove
  the two). Stop, session end, EOF or a narrowing setting ends every server
  of the session, idle ones included, the guardian killing the group on the runner's death
  through a private lifetime pipe (it reaps the leader only after killing its group, so the group
  id is never one reused by another process); the next turn starts a fresh generation. A thread's MCP
  panel toggle never re-enables a definition Settings has disabled. Resolved credential
  values are masked exactly, whole and by fragment, in arguments, results, errors, stderr,
  events, the transcript and the store, before truncation. `tests/local_mcp.rs` proves these
  with fixture servers; `local_mcp_real_stdio` runs the official filesystem server when
  `LOCAL_MCP_TEST_COMMAND` names an installed copy.
- **Localhost bound**: interacts with the local shell, and with other machines only through
  the user's own `ssh`; exposes no ports. MCP servers for the Local agent are local child
  processes on pipes, never a network transport. The outbound connections are four. The user's
  `ssh` (`agent_core::remote`): only to a machine listed in Settings → General → Remote Machines
  (`remote_hosts`, its alias validated: `[A-Za-z0-9._@:-]`, no leading `-`, after `--`),
  started only by a user action (Check, opening a page or thread on that machine, Reconnect)
  or by a page or thread the user created there, never at startup (remote pages are never
  saved for Reopen Terminal Tabs), always as an argv with no local shell string, and always
  with `ClearAllForwardings=yes`, `ForwardAgent=no`, `ForwardX11=no`, `PermitLocalCommand=no`,
  `Tunnel=no` and `RemoteCommand=none`, so no `-L`/`-R`/`-D` forwarding or tun device from the
  user's ssh configuration applies and nothing listens on either side. What ssh writes is the
  other side's text: Check's reason is redacted before it is cut or shown or logged
  (`remote::check_outcome`), a remote page keeps `alias:folder` as its page and window title
  whatever the remote shell sets, and it opens only `http(s)://` links
  (`remote::remote_link_allowed`), never a `file://` or another scheme's handler. A terminal page runs `ssh -t` with `BatchMode=no`
  (the user answers a host-key or password prompt in the page itself); anything without a
  terminal (Check, and any chat process) runs with `BatchMode=yes`, so it never waits on a
  prompt nobody sees. The remote command is one POSIX line, `cd -- '<folder>' && exec
  <command>`, every user value single-quoted, writing nothing before `exec`. Nothing is ever
  installed or written on the other machine, and nothing local but the config, so uninstall is
  unchanged. A remote page has no checkpoints, diff panel, hand-off, session pickers, profile
  env file or provider: they read or start things on this machine. A remote chat thread (Claude
  or Codex only; agy and the Local agent are refused by `remote::chat_cli` before any process)
  keeps `ssh://<alias><abs path>` as its `cwd` for life (no schema change; every boundary that
  takes a thread's folder reads it with `ThreadFolder`, and `is_remote_cwd` never lets one reach
  local git, files or folder resolution; a remote page's `TabState::dir` is that `ssh://` value,
  and `$HOME` is only the drawer's local `ssh` working folder). Its agent is
  `ChatSession::remote`: `remote_spec` wraps every process as `ssh_chat_argv` (`cd -- '<dir>' &&
  exec env PATH='<probed>' '<cli>' …`, `BatchMode=yes`) with no local cwd, no environment but
  the app's minus `clear_env` and every agent-only variable, and no profile, provider, key file,
  approval socket or delegation binding; the adapter gets an empty cwd. Only a POSIX login shell
  (`remote::POSIX_SHELLS`; the probe, the folder check and `ssh_chat_argv` all refuse any other,
  since `sh_quote`'s quoting holds only there; `$SHELL` is not trusted alone: the probe's first
  command, parsed by the login shell itself, prints `sh_quote` of a canary holding `'` and `\\`,
  and `parse_probe` refuses unless it comes back exactly, as fish's does not) and a CLI path
  with no `=` are accepted, and no
  argument with a control or invisible character (`plain_arg`; a remote-reported session id or
  model holding one is refused at ingestion, and `AgentProcess::spawn` refuses a NUL). Every
  `ssh` to another machine starts in `ssh_env()` (no agent-only variable). Its machine must
  still be listed before every process (`HostCheck`, `remote::listed`, in `start_process`), as
  before every remote terminal page or drawer (`remote_page_argv`: a toast's "Open a Terminal
  There", Reconnect, Restart), and Settings forgets a machine's probe when it is removed or
  renamed, and refuses either while a running remote thread, an open drawer or a terminal page
  is on it, naming them (`remote_machine_in_use`, `in_use_refusal`). It is never reopened at
  launch (left out of the open-thread layout and refused at restore), so its agent starts only
  when the user creates or opens it, when the user sends a message after a failed start
  (`retry_remote_with_message`; showing the page never retries), or when a crash restart
  follows a run the user started (ssh's exit 255 says "Could not reach <alias>" with ssh's
  redacted reason, both it and whether a terminal would help read only from ssh's own lines,
  `remote::ssh_own_line`, never the remote CLI's output on the same stderr; a host-key or login
  failure is not retried and the window offers a terminal page there). Dangerous permissive
  mode is refused (`Inner::set_mode`, the view, and a safe mode forced in `launch_spec`; a
  `ModeChanged { Bypass }` the remote agent reports is rewritten to the session-safe mode in
  `Inner::emit`, said once, and the safe mode sent back to the agent), and so
  is a switch to another agent or profile (`Inner::switch` and every window entry,
  `refuse_remote_switch`). It has no checkpoints (`ThreadDiffs` takes no baseline, so no ref is
  made), diff panel, undo, worktree, fork baseline, hand-off brief, `/handoff`, session pickers,
  sub-agents, usage-limit wait or continuation, model-retirement check, vendor probe or sign-in
  banner; a card's diff is the agent's own (`filediff::agent_edit_only`), file links and images
  Codex viewed are never read here, its transcript hands the system only
  `remote::remote_link_allowed` links (and so does every label in no thread's view, the compare
  window's replies and release notes included, since it cannot tell which machine wrote it),
  Codex takes no images (`remote::takes_images`, steered
  input too, and before its session starts), and its cards keep Allow once and Deny
  (`remote::approval_options`, no rule offered, recorded or applied, nothing auto-answered in
  any mode; a Codex permission request is Deny only, and an Allow sent for one is answered
  Deny). Its drawer is `ssh -t` to the folder. An older build reads such a `cwd` as a relative
  local folder: its agent fails to start for want of that folder, unless a folder of that
  relative name exists where it was started (said in `docs/reference.md`).
  The `remote_tests` session tests prove these with a recording fake `ssh` (`set_test_ssh`); an
  unset one is `false`. They assert on the spawn's own `SpawnSpec` (what is unset, added and
  where it runs), never on variables the test process happened to inherit. **Accepted risks:** a remote agent acts with the remote account's
  authority, under weaker approval scoping (Allow once is all a card can grant, and Codex's
  Accept edits asks for every edit); Stop is the agent's own interrupt over its input, so a tool
  it started there may keep running, and closing the thread ends only the local `ssh`; the
  user's ssh configuration runs as configured; and a compromised remote machine can feed the
  app arbitrary agent events, which the existing bounds, redaction and the store's scrubbing
  still apply to. The user's ssh
  configuration (ProxyCommand, ControlMaster) runs as the user set it. The update check (`agent_kit::update`): a single HTTPS GET of the
  repository's latest GitHub release, off the main thread, with a timeout. It runs once per
  process a few seconds after startup unless Settings → "Check for Updates at Startup" is off
  (`check_updates_on_startup`), and when the user picks "Check for Updates…". Tests never reach
  it. And a model server's model list (`agent_kit::provider_models`): one GET of
  `<base>/v1/models` at the address the user configured for a profile's model provider, off the
  main thread, with a timeout, redirects not followed, never through a proxy (an
  `HTTP(S)_PROXY` in the environment is ignored), made only while a profile has a provider
  (when the models are listed, as the vendor's list is otherwise; for each enabled Local server
  at every agent scan and on Scan Now). For a Local server of a kind that says what each model
  can do, the catalogue and Scan Now also ask it (`provider_models::fetch_info`): Ollama's
  `POST /api/show` per model (at most 50, five seconds each), LM Studio's `GET /api/v1/models`;
  a failure leaves a model to its name. Its tests use a loopback listener of their own. And the
  Local agent's chat requests (`agent_kit::local_agent::HttpChat`): streamed POSTs of
  `<base>/v1/chat/completions` to the address the user configured for that server, only while
  one of its threads runs a turn, redirects not followed, never through a proxy, the key sent
  only as a bearer token read from the variable the profile names. Never add any other request. Two user actions reach GitHub without the app making a
  request: the diff panel's "Open a PR" hands GitHub's compare URL (built from the remote,
  credentials dropped: `agent_kit::github`) to the browser, and "Push & Open PR" runs one
  `git push` (never forced, after a confirmation that shows the command) through the user's own
  git and credentials. Neither runs unasked. The Support menu item (`support.rs`) likewise hands
  a fixed sponsorship URL to the browser on a user action and makes no request itself.
- **Model tags have three layers and one writer**: a model's tier, tags and best-for come from
  the user's file (`model-tags.user.json`, `TerminalConfig::user_model_tags_path`), then the
  shared file (`model-tags.json`, authored elsewhere, read only), then the built-in list
  (`agent_core::model_tags`; `lookup_for` ranks layer first, then an entry scoped to the
  model's profile over an unscoped one). The app alone writes the user's file, atomically and
  `0600` (`agent_kit::user_tags_file`, edits in `agent_core::user_tags`), only from Settings →
  Agents → Model Tags and `agent-terminal tags`, and `uninstall` lists it (`Inputs::user_tag_files`).
  No agent tool changes tags (the delegation tools only read them); the file is the user's, so
  any process with the user's shell can still run the command or edit it. The built-in
  list names no local model. Capabilities report each profile's `cost`
  (`config::profile_is_free`; a free one reports usage `ok`, load 0) and each model's
  `tags_source`. A Local server with a `headroom_probe` reports what the probe read instead
  (`local_headroom`: the user's command, exec'd with no shell and a minimal environment, off the
  main thread, bounded by its time limit; `low` under the threshold, `unknown` without a fresh
  reading, never `ok` by default), also written to `usage.json` under `local`.
- **Never block the main thread**: anything that shells out — CLI detection above
  all, which may run `$SHELL -ic` and source the user's rc — goes through
  `gio::spawn_blocking`. A frozen window cannot even repaint its own spinner.
- **Graceful error handling**: avoid `.unwrap()`/`.expect()` in runtime paths.
  Surface failures to the user rather than dying.
- **A dead session is not a reason to destroy UI**: a non-zero exit keeps its tab,
  its scrollback and its error message. Only a clean exit closes a tab.
- **A thread the user did not stop keeps its agent**: a chat agent that exits with a failure
  nobody asked for is started again, resuming its session, after 2, 10 and 30 s
  (`restart_after_crash`; never after an interrupt, for a delegated worker, while the thread waits
  for its usage limit, or into anything that owns the next start), and past that the thread says
  so. A mode or model picked while the agent is stopped starts it in that choice
  (`startable_now`), as a message does: "The agent is not running" is never the answer to
  something the app can do itself. A delegation root's sub-agents failing never stops the root:
  `fail_worker` revokes its tree and leaves it running unbound, said in the thread. A root
  launched on another agent than its supervisor record names is judged as the agent it runs
  (`Hooks::binding` corrects the record and starts it unbound).
- **Reference integrity**: GTK signal handlers use weak references
  (`glib::clone!` with `#[weak]`) to avoid reference cycles.
- **Config writes are atomic**: write to a temp file, `sync_all`, then rename.
  Never write over a live config in place.
- **Observability**: logging goes to the systemd journal (identifier
  `agent-terminal`) and to stderr when attached to a terminal, with a panic hook.
  Keep meaningful coverage on spawn, detection, config and tab lifecycle paths.

## 🧭 Navigating with Graphify

A Graphify knowledge graph of the repo (code symbols from the AST, docs from a pass on a
local Ollama) lives in the git-ignored `graphify-out/`. Reach for it before a broad grep:
the `graphify` MCP tools, or the CLI from the repo root:

- `graphify explain "crates/agent-core/src/redact.rs::redact()"`: a symbol's callers and
  callees with file and line. The strongest tool; start here.
- `graphify path "ChatSession" "crates_agent_kit_src_store_store"`: how two symbols connect.
- `graphify affected "<symbol>"`: what depends on a symbol, before you change it.
- `graphify query "<question>"`: keyword-seeded BFS; broad and noisy, so prefer the above.
- `graphify-out/GRAPH_REPORT.md`: the communities and hub nodes at a glance.

Short names are often ambiguous (`Store` matches four nodes); the CLI exits 3 and lists the
candidates, so retry with `<path>::<symbol>` or the node id it prints. Edges tagged
`[INFERRED]` are guesses; `[EXTRACTED]` ones come from the AST.

The graph is a snapshot: confirm what it says in the source before editing. Refresh it with
`$HOME/scripts/graphify-build.sh "$PWD"` from the repo root (add
`--code-only` for an AST-only pass with no LLM).

## 🧪 Testing Strategy

- **Logic separation**: keep pure logic in `agent-core`, and GTK-free git and
  session logic in `agent-kit` (`git`, `diff`, `restore`, `worktree`,
  `handoff`, `sessions`); the app's `src/utils.rs` and `src/config.rs` hold the
  rest. All of it is decoupled from GTK so it is unit-testable without a
  display. `agent-kit`'s repository tests run real git in temp repos with a
  hermetic config (CI has no git identity) and skip themselves when git is
  absent. `window/imp.rs` is covered by a construction smoke test plus tests
  for any pure helpers in it.
- **Guard hand-maintained arrays**: `CliClient::ALL`, `ThemeChoice::ALL` and
  `DiffBase::ALL` drive dropdowns by index in both directions. All have exhaustive-match
  tests so adding a variant fails to compile until the array is updated. Any new
  such array needs the same treatment.
- **Update tests alongside** any change to detection, startup-command,
  path-resolution, config-serialization or migration logic.

## 🛠️ Development Workflow

1. **Environment**: `make deps`.
2. **Build & run**: `cargo build`; `make start-local`.
3. **Running a local build**: GApplication is single-instance, keyed on the app
   ID. If the packaged build is already running, launching your build hands the
   activation to *that* process and yours exits immediately — the journal then
   shows a plausible startup that has nothing to do with your code. Always use
   `dbus-run-session -- ./target/debug/agent-terminal`. Do not kill the running
   instance to work around this; it is someone's live session.
4. **Linting**: `cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings`.
   CI enforces fmt, clippy, and tests under Xvfb.
5. **Testing**: `make test` while working; `make check` is what CI and a release run (lint,
   the suite, the `#[ignore]`d display tests one at a time); `make coverage` holds
   `COVERAGE_FLOOR`. Never run a test script by hand. GTK tests refuse to run outside the make
   targets' private display (`testutil::require_virtual_display`, the `AGENT_TERMINAL_TEST_DISPLAY`
   marker). Mutation testing (`cargo mutants`) runs
   in CI only (`mutants.yml`), never on a developer machine. Mutation tests run only in
   resource-limited CI because mutants may allocate without bound. Details: `tests/qa/README.md`.
6. **Installation**: do **NOT** run `make install` on developer/daily-driver
   machines. Deployment is the distribution packages built by the release workflow.
   A manual `make install` shadows the packaged install in PATH, so the machine silently keeps
   running a stale build. If you need it for isolated testing, run
   `make uninstall UNINSTALL_ARGS=--keep-data` immediately afterwards: the data is per user,
   shared with the packaged app, and without a terminal the uninstall refuses to guess.

## 📦 Packaging

Packaging is `cargo-deb` driven by `[package.metadata.deb]`, both locally
(`make package`) and in the release workflow. Runtime dependencies come from `$auto`,
which derives them from the built ELF — do **not** hand-list them alongside it;
that previously produced duplicate entries with weaker bounds and, before the
switch, omitted `libadwaita-1-0` entirely from every shipped package.

**Releases** (`.github/workflows/release.yml`): a `v*` tag builds the `.deb` (Ubuntu 24.04,
cargo-deb), the `.rpm` (Fedora 42, cargo-generate-rpm, `[package.metadata.generate-rpm]`) and the
Arch package (`packaging/arch/PKGBUILD`, makepkg), each by `packaging/build-<kind>.sh` in a clean
container of its distribution, and publishes them with `SHA256SUMS` as a GitHub Release. The tag
must equal `v` + the Cargo.toml version, and `docs/releases/<tag>.md` must exist (the release
notes): the workflow refuses otherwise. Run it by hand (workflow_dispatch) for a dry run that
builds everything and publishes nothing. The same scripts build locally:
`docker run --rm -v "$PWD":/src:ro -v /tmp/out:/out -e OUT=/out fedora:42 bash /src/packaging/build-rpm.sh`.
Keep the three asset lists (deb, rpm metadata, PKGBUILD) in step.

**Epoch 1** on every package keeps each release newer than packages installed from an earlier
numbering: the deb version is given whole (`1:<version>-1`, `--deb-version`, because cargo-deb has no
epoch field) in `make package` and `packaging/build-deb.sh`; the rpm has `epoch = 1` in
`[package.metadata.generate-rpm]`; the PKGBUILD has `epoch=1`. Keep the three in step.

## 🤖 AI Contribution Rules

- Plans, reports, reviews, QA evidence and investigation write-ups never go in
  this repo; they live outside it. `docs/` holds only release notes, screenshots and user
  documentation. Commit messages, PR descriptions and code comments never cite plans, reviews,
  tracking-board topics or finding numbers: state the fact itself. `make lint` enforces the `docs/`
  allowlist.
- Always prioritize the **Standalone Philosophy**.
- Colours are user-selectable (`ThemeChoice` in `config.rs`, palettes in
  `theme.rs`, app-wide names in `palette.rs`); add new schemes there and update the
  guard test. The default is the app's own palette (background `#181425`,
  foreground `#c8c8ff`). `ThemeChoice::System` brings no colours: `palette.rs`
  aliases every `@at_*` name to the desktop theme's named colours, leaves
  libadwaita's own alone, does not force dark, and reads values back (for VTE,
  diffs, and Pango markup parsed from then on) when the desktop's style changes.
  Never give it a literal colour. A bundled light scheme forces libadwaita light, a dark one
  dark. `ThemeChoice::Automatic` has no colours of its own either: it applies the member of
  `theme_light`/`theme_dark` (`ThemePair`, each slot only its polarity, a wrong one loading as
  the slot's default) matching `StyleManager::is_dark`, never forces the colour scheme, and
  swaps members through the same watch as System (`palette::desktop_changed`); every lookup by
  choice resolves it first (`palette::concrete`, `palette::in_effect`). Every bundled scheme's
  outlines, accent, state colours and focus ring are lifted to 3:1 on every surface
  (`Chrome::with_visible_controls`, `every_palettes_controls_are_seen`); `@at_focus` is the one
  focus-ring colour, libadwaita's ring included.
- If you modify startup/detection logic, update and run the tests in `utils.rs`.
- Deployment-specific behaviour belongs in config, not in the binary. Anything
  that reads a path or runs a command particular to one machine should be an
  `indicators` entry rather than new Rust.
- An indicator-style check has three states, never two. An unreadable indicator source
  must report unknown, never healthy.
- A dialog closes on a press outside it, as Escape closes it (an alert answers its close
  response; a dialog that refuses to close is asked, not forced). Every window that shows dialogs
  calls `dialogs::close_dialogs_on_outside_click`, which hooks each dialog as it becomes the
  window's `visible-dialog`; a new such window calls it too.
- Settings apply as they are made, never on dialog close: a switch or dropdown at
  once, a text entry when committed (Enter, its apply button, or focus leaving it),
  and never with a half-typed value. Rows that can hold an invalid
  value must validate inline and refuse to persist rather than silently falling
  back at spawn time.
