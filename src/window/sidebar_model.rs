//! The thread sidebar's pure logic: which rows show, grouped and ordered how, with which badge
//! and relative time. No GTK, so it is tested without a display.

use agent_core::adapter::{Driver, Mode};
pub use agent_core::search_query::SearchQuery;
pub use agent_core::thread_tag::TagColour;

/// What a sidebar row opens.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RowKey {
    /// A chat thread in the store.
    Thread(String),
    /// A 2.x terminal page, by its tab key.
    Terminal(u64),
}

/// A row's state badge, in priority order: only the most urgent one shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Badge {
    NeedsApproval,
    RateLimited,
    Running,
    /// The main agent is idle but background work (sub-agents, background commands) still runs:
    /// the thread is not finished.
    Background,
    Unread,
}

impl Badge {
    pub fn css_class(self) -> &'static str {
        match self {
            Badge::NeedsApproval => "badge-approval",
            Badge::RateLimited => "badge-limited",
            Badge::Running => "badge-running",
            Badge::Background => "badge-background",
            Badge::Unread => "badge-unread",
        }
    }

    pub fn tooltip(self) -> &'static str {
        match self {
            Badge::NeedsApproval => "Needs your approval",
            Badge::RateLimited => "Rate limited",
            Badge::Running => "Working",
            Badge::Background => "Background tasks running",
            Badge::Unread => "New activity",
        }
    }
}

/// The badge for a thread's state, or none. `background`: background tasks still run.
pub fn badge_for(
    running: bool,
    background: bool,
    approval: bool,
    rate_limited: bool,
    unread: bool,
) -> Option<Badge> {
    [
        (approval, Badge::NeedsApproval),
        (rate_limited, Badge::RateLimited),
        (running, Badge::Running),
        (background, Badge::Background),
        (unread, Badge::Unread),
    ]
    .into_iter()
    .find_map(|(on, badge)| on.then_some(badge))
}

/// Most task names a tooltip lists, and the most characters of each.
const TOOLTIP_TASKS: usize = 5;
const TOOLTIP_TASK_CHARS: usize = 60;

/// A row's tooltip for its background work: "N background tasks running" and their names (the
/// first few, each cut short). Empty when nothing runs.
pub fn background_tooltip(descriptions: &[String]) -> String {
    let head = match descriptions.len() {
        0 => return String::new(),
        1 => "1 background task running".to_owned(),
        n => format!("{n} background tasks running"),
    };
    let names: Vec<String> = descriptions
        .iter()
        .filter(|d| !d.is_empty())
        .take(TOOLTIP_TASKS)
        .map(|d| match d.char_indices().nth(TOOLTIP_TASK_CHARS) {
            Some((cut, _)) => format!("{}…", &d[..cut]),
            None => d.clone(),
        })
        .collect();
    if names.is_empty() {
        head
    } else {
        format!("{head}: {}", names.join(", "))
    }
}

/// Whether `row` glows: it waits for an approval and you are not looking at it (another thread
/// is shown, or the window is not focused).
pub fn needs_attention(row: &SidebarRow, shown: Option<&RowKey>, window_focused: bool) -> bool {
    row.badge == Some(Badge::NeedsApproval) && (shown != Some(&row.key) || !window_focused)
}

/// Whether closing a thread would cut work short: a turn running, background work (sub-agents,
/// background commands) still going, or an answer awaited.
pub fn is_busy(running: bool, background: bool, approval: bool) -> bool {
    running || background || approval
}

/// Approvals, questions and forms that delegated sub-agents wait on, per sub-agent, so the
/// root thread asks for the user as it would for its own. A resolution names its request when it
/// has one; without one, everything that sub-agent waits on is withdrawn.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ChildAsks {
    asks: std::collections::HashSet<(String, String)>,
}

impl ChildAsks {
    pub fn requested(&mut self, worker: &str, request: Option<&str>) {
        self.asks
            .insert((worker.to_owned(), request.unwrap_or_default().to_owned()));
    }

    pub fn resolved(&mut self, worker: &str, request: Option<&str>) {
        match request {
            Some(request) => {
                self.asks.remove(&(worker.to_owned(), request.to_owned()));
                // A request recorded without its id is withdrawn by any resolution.
                self.asks.remove(&(worker.to_owned(), String::new()));
            }
            None => self.worker_ended(worker),
        }
    }

    /// The sub-agent's turn or process ended: nothing it asked can still be answered.
    pub fn worker_ended(&mut self, worker: &str) {
        self.asks.retain(|(w, _)| w != worker);
    }

    /// Keeps only what the listed (still active) sub-agents wait on.
    pub fn retain_workers(&mut self, active: &[&str]) {
        self.asks.retain(|(w, _)| active.contains(&w.as_str()));
    }

    pub fn any(&self) -> bool {
        !self.asks.is_empty()
    }
}

/// Why a switch must ask before it stops this thread's work. `delegated`: sub-agents the
/// thread started through delegation are still at work.
pub fn busy_reason(
    running: bool,
    background: usize,
    delegated: bool,
    approval: bool,
) -> Option<String> {
    if !is_busy(running, background > 0 || delegated, approval) {
        return None;
    }
    let mut reasons = Vec::new();
    if running {
        reasons.push("The running turn will be interrupted".to_owned());
    }
    match background {
        0 => {}
        1 => reasons.push("1 background task will stop".to_owned()),
        n => reasons.push(format!("{n} background tasks will stop")),
    }
    if delegated {
        reasons.push("Its sub-agents' work will stop".to_owned());
    }
    if approval {
        reasons
            .push("The approval or question waiting for your answer will be cancelled".to_owned());
    }
    Some(format!("{}.", reasons.join(". ")))
}

/// Where an open thread's agent process is, as the sidebar shows it. Presentation only, derived
/// by [`thread_activity`] from state the window and session already hold; nothing decides on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadActivity {
    /// The page is being built, or its agent is starting (or resuming) and has not reported its
    /// session yet: the row spins.
    Starting,
    /// The agent is up.
    Live,
    /// The agent was stopped to save memory; the next message resumes it. The row says "Paused".
    Paused,
}

impl ThreadActivity {
    /// The row's words for it, if it shows any.
    pub fn tooltip(self) -> &'static str {
        match self {
            Self::Starting => "Starting the agent",
            Self::Live => "Agent running",
            Self::Paused => "Paused to save memory; resumes when you send",
        }
    }
}

/// The activity of a thread's agent. `preparing`: the window is building the page or starting
/// its session; `alive`, `starting` and `suspended`: what the session says of its process (all
/// false with no session). `None`: no agent to speak of (a closed or unbuilt page, an agent that
/// is not installed, a process that exited).
pub fn thread_activity(
    preparing: bool,
    alive: bool,
    starting: bool,
    suspended: bool,
) -> Option<ThreadActivity> {
    if suspended && !alive {
        Some(ThreadActivity::Paused)
    } else if preparing || (alive && starting) {
        Some(ThreadActivity::Starting)
    } else if alive {
        Some(ThreadActivity::Live)
    } else {
        None
    }
}

/// The empty state's offer to reopen the `n` threads open when the app last closed, or `None`
/// when there are none to reopen.
pub fn reopen_offer(n: usize) -> Option<String> {
    match n {
        0 => None,
        1 => Some("Reopen 1 thread from your last session".to_owned()),
        n => Some(format!("Reopen {n} threads from your last session")),
    }
}

/// What the sidebar says when it lists nothing: the title and hint for an empty list, for a
/// search that matched nothing, or for a `#tag` filter no thread passes.
pub fn empty_sidebar_text(query: &SearchQuery) -> (&'static str, &'static str) {
    if query.is_empty() {
        (
            "No Threads Yet",
            "Start one with Ctrl+Shift+T or the + button.",
        )
    } else if !query.tags.is_empty() {
        (
            "No Matching Threads",
            "No thread holds every tag searched for. Tag one from its menu, or show archived \
             threads.",
        )
    } else {
        (
            "No Matching Threads",
            "Try other words, or show archived threads.",
        )
    }
}

/// Most tag chips a sidebar row shows; the rest are counted (`+N`).
pub const MAX_CHIPS: usize = 2;

/// A tag as a row shows it: its name and its colour, if it has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowTag {
    pub name: String,
    pub colour: Option<TagColour>,
}

/// `tags` with their colours from `colours`, in the order given.
pub fn row_tags(
    tags: &[String],
    colours: &std::collections::BTreeMap<String, TagColour>,
) -> Vec<RowTag> {
    tags.iter()
        .map(|name| RowTag {
            name: name.clone(),
            colour: colours.get(name).copied(),
        })
        .collect()
}

/// The chips a row draws ([`MAX_CHIPS`] at most) and how many tags it leaves to `+N`.
pub fn shown_chips(tags: &[RowTag]) -> (&[RowTag], usize) {
    let shown = tags.len().min(MAX_CHIPS);
    (&tags[..shown], tags.len() - shown)
}

/// Every tag of a row, for its tooltip and screen-reader description: `#a #b #c`.
pub fn tags_text(tags: &[RowTag]) -> String {
    tags.iter()
        .map(|t| format!("#{}", t.name))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The search text after a click on the chip of `tag`: `#tag` added to what is typed, unless
/// it is already one of its tag terms.
pub fn with_tag_term(current: &str, tag: &str) -> String {
    if SearchQuery::parse(current).tags.iter().any(|t| t == tag) {
        return current.to_owned();
    }
    match current.trim_end() {
        "" => format!("#{tag}"),
        typed => format!("{typed} #{tag}"),
    }
}

/// The Tags dialog's rows: every tag a thread holds (`known`, with how many hold it) and the
/// thread's own (`current`), each once and sorted, ticked when the thread holds it.
pub fn tag_choices(known: &[(String, usize)], current: &[String]) -> Vec<(String, bool)> {
    let mut names: Vec<String> = known.iter().map(|(t, _)| t.clone()).collect();
    names.extend(current.iter().cloned());
    names.sort();
    names.dedup();
    names
        .into_iter()
        .map(|t| {
            let held = current.contains(&t);
            (t, held)
        })
        .collect()
}

/// What the Tags dialog's colour dropdown lists: "No colour", then every [`TagColour`].
pub fn colour_choices() -> Vec<&'static str> {
    std::iter::once("No colour")
        .chain(TagColour::ALL.iter().map(|c| c.label()))
        .collect()
}

/// The colour at dropdown position `index` ([`colour_choices`]); `None` for "No colour" or one
/// out of range.
pub fn colour_at(index: u32) -> Option<TagColour> {
    (index as usize)
        .checked_sub(1)
        .and_then(|i| TagColour::ALL.get(i).copied())
}

/// The dropdown position of `colour` ([`colour_choices`]).
pub fn colour_index(colour: Option<TagColour>) -> u32 {
    colour
        .and_then(|c| TagColour::ALL.iter().position(|x| *x == c))
        .map_or(0, |i| i as u32 + 1)
}

/// The thread's tags once the Tags dialog is saved: the ticked rows and what is typed in its
/// entry (commas, spaces or new lines between tags), checked as the store checks them.
pub fn chosen_tags<'a>(
    ticked: impl IntoIterator<Item = &'a str>,
    typed: &str,
) -> Result<Vec<String>, agent_core::thread_tag::TagError> {
    let mut all: Vec<String> = ticked.into_iter().map(str::to_owned).collect();
    all.extend(agent_core::thread_tag::parse_tags(typed)?);
    agent_core::thread_tag::tag_set(&all)
}

/// Most message hits the sidebar asks the index for.
pub const MESSAGE_HITS: usize = 50;
/// How long typing must pause before the message search runs (ms).
pub const MESSAGE_SEARCH_DEBOUNCE_MS: u64 = 250;
/// How often a search shown while threads are still being indexed runs again (ms), so what the
/// index takes in meanwhile shows up.
pub const MESSAGE_SEARCH_REFRESH_MS: u64 = 3_000;

/// What the last message search found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageSearch {
    /// The index could not be read (no full-text search in this SQLite, or the read failed):
    /// never shown as "no match".
    Unavailable,
    /// The thread list has not loaded yet, so no hit can be named: never "no match" either.
    /// The search runs again once it has.
    Loading,
    /// This many hits (0: no match).
    Found(usize),
}

/// "1 thread", "3 threads".
fn threads_count(n: usize) -> String {
    match n {
        1 => "1 thread".to_owned(),
        n => format!("{n} threads"),
    }
}

/// The line under "In Messages": why there is nothing to show, or that the index is still being
/// built (`indexing` threads to go), so the hits may be partial. `None` when the hits say it all.
pub fn message_status(found: MessageSearch, indexing: usize) -> Option<String> {
    let building = (indexing > 0).then(|| format!("Indexing {}…", threads_count(indexing)));
    match (found, building) {
        (MessageSearch::Unavailable, _) => {
            Some("Search is unavailable: the message index could not be read.".to_owned())
        }
        (MessageSearch::Loading, _) => Some("Loading threads…".to_owned()),
        (MessageSearch::Found(0), None) => Some("No messages match.".to_owned()),
        (MessageSearch::Found(0), Some(building)) => {
            Some(format!("No messages match yet. {building}"))
        }
        (MessageSearch::Found(_), Some(building)) => {
            Some(format!("{building} These results may be partial."))
        }
        (MessageSearch::Found(_), None) => None,
    }
}

/// Who said what a hit found, as its row names them.
pub fn hit_role_label(role: &str) -> &'static str {
    match role {
        "user" => "You",
        "assistant" => "Agent",
        "tool" => "Tool",
        _ => "Message",
    }
}

/// A snippet character as it is shown: line breaks and other control characters as spaces (a
/// snippet is one run of text, and some control characters are not valid in markup).
fn shown_char(c: char) -> char {
    if c.is_control() {
        ' '
    } else {
        c
    }
}

/// A hit's snippet as plain text: the match markers dropped.
pub fn snippet_plain(snippet: &str) -> String {
    use agent_kit::store::{SNIPPET_CLOSE, SNIPPET_OPEN};
    snippet
        .chars()
        .filter(|c| *c != SNIPPET_OPEN && *c != SNIPPET_CLOSE)
        .map(shown_char)
        .collect()
}

/// A hit's snippet as Pango markup: every character of the text escaped, so nothing the thread
/// said is read as markup, and only the match markers made emphasis. A stray close marker is
/// dropped and an open one left unclosed is closed at the end, so the markup always parses.
pub fn snippet_markup(snippet: &str) -> String {
    use agent_kit::store::{SNIPPET_CLOSE, SNIPPET_OPEN};
    let mut out = String::with_capacity(snippet.len() + 16);
    let mut open = false;
    for c in snippet.chars() {
        match c {
            SNIPPET_OPEN if !open => {
                out.push_str("<b>");
                open = true;
            }
            SNIPPET_CLOSE if open => {
                out.push_str("</b>");
                open = false;
            }
            SNIPPET_OPEN | SNIPPET_CLOSE => {}
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(shown_char(c)),
        }
    }
    if open {
        out.push_str("</b>");
    }
    out
}

/// One row of the sidebar's "In Messages" section.
#[derive(Debug, Clone, PartialEq)]
pub struct MessageHitRow {
    pub thread: String,
    /// The transcript item the hit is in: what opening it brings into view.
    pub item: String,
    pub title: String,
    pub folder: String,
    /// `user`, `assistant` or `tool`.
    pub role: String,
    /// When the item began (epoch milliseconds).
    pub at: i64,
    /// With the match markers ([`snippet_markup`], [`snippet_plain`]).
    pub snippet: String,
    pub archived: bool,
}

impl MessageHitRow {
    /// What a screen reader says for the row: who, in which thread, when, and the text.
    pub fn accessible_label(&self, now_ms: i64) -> String {
        let age = match relative_time(now_ms, self.at) {
            now if now == "now" => "just now".to_owned(),
            age => format!("{age} ago"),
        };
        let archived = if self.archived { ", archived" } else { "" };
        format!(
            "{} in {}{archived}, {age}: {}",
            hit_role_label(&self.role),
            self.title,
            snippet_plain(&self.snippet).trim()
        )
    }
}

/// The rows for `hits`, in the order the index ranked them, titled from the loaded thread list
/// `threads`. A hit whose thread is not in it (deleted since) is left out.
pub fn message_hit_rows(
    hits: Vec<agent_kit::store::SearchHit>,
    threads: &[agent_kit::store::ThreadSummary],
) -> Vec<MessageHitRow> {
    hits.into_iter()
        .filter_map(|hit| {
            let summary = threads.iter().find(|t| t.id == hit.thread_id)?;
            Some(MessageHitRow {
                title: display_title(&summary.title).to_owned(),
                folder: summary.cwd.clone(),
                thread: hit.thread_id,
                item: hit.item_id,
                role: hit.role,
                at: hit.at,
                snippet: hit.snippet,
                archived: hit.archived,
            })
        })
        .collect()
}

/// The toast after the agents' own recent sessions were listed as threads: how many, and from
/// which agents (each named once, in the order first met). `None` when none were added.
pub fn sessions_added_toast(drivers: &[Driver]) -> Option<String> {
    let mut names: Vec<&str> = Vec::new();
    for d in drivers {
        let name = d.info().label;
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let agents = match names.as_slice() {
        [] => return None,
        [one] => (*one).to_owned(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    };
    Some(match drivers.len() {
        1 => format!("Added 1 recent session from {agents}"),
        n => format!("Added {n} recent sessions from {agents}"),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenState {
    Closed,
    Here,
    Elsewhere,
}

impl OpenState {
    pub fn description(self) -> &'static str {
        match self {
            Self::Closed => "Closed",
            Self::Here => "Open",
            Self::Elsewhere => "Open in another window",
        }
    }
}

/// Whether a finished turn means the thread is finished: nothing left in the background. A
/// turn that ends with background work still going is not announced; the turn Claude runs to
/// read that work's result is.
pub fn turn_finishes_thread(background: bool) -> bool {
    !background
}

/// Whether a finish held back by [`turn_finishes_thread`] is due now: the background work is
/// gone and no turn runs. Claude usually runs a turn to read the results (which clears the hold
/// and finishes the thread itself), but not always: a failed or stopped turn, or a background
/// command it does not report on, leaves only this.
pub fn held_finish_due(held: bool, running: bool, background: bool) -> bool {
    held && !running && !background
}

/// The tooltip of a spinning row: its badge's, plus what runs in the background.
pub fn row_tooltip(badge: Badge, background: &[String]) -> String {
    match (badge, background.is_empty()) {
        (Badge::Background, true) => badge.tooltip().to_owned(),
        (Badge::Background, false) => background_tooltip(background),
        (_, true) => badge.tooltip().to_owned(),
        (_, false) => format!("{}; {}", badge.tooltip(), background_tooltip(background)),
    }
}

/// The window-close question's title for `n` busy threads.
pub fn stop_heading(n: usize) -> String {
    match n {
        1 => "Stop 1 running thread and close?".to_owned(),
        n => format!("Stop {n} running threads and close?"),
    }
}

/// What happened to a thread that may need the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attention {
    /// An approval or a question is waiting.
    Asked,
    /// An approval ran out (agy denies after its deadline): the card is gone.
    Expired,
    /// A turn finished.
    TurnDone,
}

/// How the window reacts to an [`Attention`] event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reaction {
    /// Leave the thread unread (a sidebar mark that outlives the event).
    pub unread: bool,
    /// Raise a desktop notification.
    pub notify: bool,
}

/// The reaction to `event` on a thread that is `shown` (selected) in a window that is
/// `focused`. Looking at it means both; anything else needs a trace. A request for the user is
/// always announced when they are not looking, a finished turn only when `notify_on_bell`.
pub fn react(event: Attention, shown: bool, focused: bool, notify_on_bell: bool) -> Reaction {
    let looking = shown && focused;
    match event {
        // The glow covers a shown thread, so unread only marks what is out of view.
        Attention::Asked => Reaction {
            unread: !shown,
            notify: !looking,
        },
        Attention::Expired => Reaction {
            unread: !looking,
            notify: false,
        },
        Attention::TurnDone => Reaction {
            unread: !looking,
            notify: !looking && notify_on_bell,
        },
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarRow {
    pub key: RowKey,
    pub title: String,
    /// The working directory; rows are grouped by it.
    pub folder: String,
    /// Last activity, milliseconds since the epoch.
    pub updated_ms: i64,
    /// `None`: a terminal page.
    pub driver: Option<Driver>,
    pub badge: Option<Badge>,
    /// Where an open thread's agent process is ([`thread_activity`]); `None` for the rest.
    pub activity: Option<ThreadActivity>,
    /// What runs in the background (sub-agents, background commands), for the row's tooltip.
    pub background: Vec<String>,
    /// Where the thread's page is open, if anywhere.
    pub open: OpenState,
    /// Archived threads are hidden unless the sidebar's "Show Archived" toggle is on.
    pub archived: bool,
    /// A pinned thread's place in the Pinned section (0 is first); `None` when not pinned.
    pub pin_rank: Option<i64>,
    /// A terminal page on another machine; its `folder` is `alias:folder`.
    pub remote: bool,
    /// The thread's tags, sorted, with their colours.
    pub tags: Vec<RowTag>,
}

/// Whether `row` passes `query`: its plain text (case-insensitive) is in the title, the folder
/// or a tag, and the row holds every one of its `#tag` terms.
fn row_matches(row: &SidebarRow, text: &str, tags: &[String]) -> bool {
    let text_ok = text.is_empty()
        || row.title.to_lowercase().contains(text)
        || row.folder.to_lowercase().contains(text)
        || row.tags.iter().any(|t| t.name.contains(text));
    text_ok
        && tags
            .iter()
            .all(|want| row.tags.iter().any(|t| t.name == *want))
}

/// Rows matching `query` ([`row_matches`]), grouped by folder. Groups are ordered by their
/// newest row and rows newest first, so the folder being worked in is on top. Archived rows are
/// left out unless `show_archived` (or the thread is open in this window).
pub fn group_rows(
    rows: Vec<SidebarRow>,
    query: &SearchQuery,
    show_archived: bool,
) -> Vec<(String, Vec<SidebarRow>)> {
    let text = query.text().to_lowercase();
    let mut groups: Vec<(String, Vec<SidebarRow>)> = Vec::new();
    for row in rows.into_iter().filter(|r| {
        (show_archived || !r.archived || r.open != OpenState::Closed)
            && row_matches(r, &text, &query.tags)
    }) {
        match groups.iter_mut().find(|(f, _)| *f == row.folder) {
            Some((_, list)) => list.push(row),
            None => groups.push((row.folder.clone(), vec![row])),
        }
    }
    for (_, list) in &mut groups {
        list.sort_by_key(|r| std::cmp::Reverse(r.updated_ms));
    }
    groups.sort_by(|a, b| {
        let newest = |g: &[SidebarRow]| g.first().map_or(i64::MIN, |r| r.updated_ms);
        newest(&b.1).cmp(&newest(&a.1))
    });
    groups
}

/// Whether a row is live work: a turn running, background work going, an approval waiting or a
/// rate limit holding it. These sit in the sidebar's own "Active" section.
pub fn is_active(row: &SidebarRow) -> bool {
    matches!(
        row.badge,
        Some(Badge::NeedsApproval | Badge::RateLimited | Badge::Running | Badge::Background)
    )
}

/// The sidebar's three sections ([`sidebar_sections`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SidebarSections {
    /// The pinned threads, in the order the user gave them. A pinned thread is listed here only,
    /// whatever its badge, and leaves its folder group.
    pub pinned: Vec<SidebarRow>,
    /// The other active threads ([`is_active`]), most urgent first.
    pub active: Vec<SidebarRow>,
    /// Every remaining thread, grouped by folder.
    pub groups: Vec<(String, Vec<SidebarRow>)>,
}

impl SidebarSections {
    /// How many pinned threads are active: they are not in the active section, so its header
    /// counts them.
    pub fn pinned_active(&self) -> usize {
        self.pinned.iter().filter(|r| is_active(r)).count()
    }
}

/// The sidebar's three sections: the pinned threads in the user's order; then the active threads
/// ([`is_active`]) on their own, most urgent first (an approval, a rate limit, then working, then
/// background work) and newest within each; then every other thread grouped by folder as
/// [`group_rows`] does, ordered by the newest of what is left. The same search and archive filter
/// applies to all three (an archived pinned thread shows only with `show_archived` or while it is
/// open), and a thread is listed once: pinned first, else active, else in its folder.
pub fn sidebar_sections(
    rows: Vec<SidebarRow>,
    query: &SearchQuery,
    show_archived: bool,
) -> SidebarSections {
    let mut pinned = Vec::new();
    let mut active = Vec::new();
    let mut groups: Vec<(String, Vec<SidebarRow>)> = group_rows(rows, query, show_archived)
        .into_iter()
        .filter_map(|(folder, list)| {
            let (pins, list): (Vec<_>, Vec<_>) =
                list.into_iter().partition(|r| r.pin_rank.is_some());
            pinned.extend(pins);
            let (live, rest): (Vec<_>, Vec<_>) = list.into_iter().partition(is_active);
            active.extend(live);
            (!rest.is_empty()).then_some((folder, rest))
        })
        .collect();
    // A row that left a group may have been what put it on top.
    groups.sort_by_key(|(_, list)| {
        std::cmp::Reverse(list.first().map_or(i64::MIN, |r| r.updated_ms))
    });
    active.sort_by(|a, b| a.badge.cmp(&b.badge).then(b.updated_ms.cmp(&a.updated_ms)));
    pinned.sort_by_key(|r| (r.pin_rank, std::cmp::Reverse(r.updated_ms)));
    SidebarSections {
        pinned,
        active,
        groups,
    }
}

/// A folder group as the sidebar draws it: its header, and the rows under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderGroup {
    pub folder: String,
    /// The rows drawn under the header: all of them when expanded, else only the shown row.
    pub rows: Vec<SidebarRow>,
    pub expanded: bool,
    /// Rows the collapsed header hides.
    pub hidden: usize,
}

/// Folds the folder groups: each is collapsed unless the user expanded it (`expanded`) or a
/// search is on (its matches must be seen). A collapsed group still draws the `selected` row,
/// so the thread on screen never vanishes from the list.
pub fn fold_groups(
    groups: Vec<(String, Vec<SidebarRow>)>,
    expanded: &std::collections::HashSet<String>,
    searching: bool,
    selected: Option<&RowKey>,
) -> Vec<FolderGroup> {
    groups
        .into_iter()
        .map(|(folder, rows)| {
            let open = searching || expanded.contains(&folder);
            let total = rows.len();
            let rows: Vec<SidebarRow> = if open {
                rows
            } else {
                rows.into_iter()
                    .filter(|r| Some(&r.key) == selected)
                    .collect()
            };
            FolderGroup {
                hidden: total - rows.len(),
                folder,
                rows,
                expanded: open,
            }
        })
        .collect()
}

/// Everything one sidebar redraw depends on, apart from the clock: the sections, which row is
/// shown, whether the window has focus (it decides which rows glow) and the search text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarModel {
    pub pinned: Vec<SidebarRow>,
    /// Pinned threads that are also active, counted in the active header.
    pub pinned_active: usize,
    pub active: Vec<SidebarRow>,
    pub groups: Vec<FolderGroup>,
    pub selected: Option<RowKey>,
    pub focused: bool,
    pub query: String,
}

impl SidebarModel {
    /// The rows in the order the list draws them: the pinned section, the active section, then
    /// each folder group.
    pub fn rows(&self) -> impl Iterator<Item = &SidebarRow> {
        self.pinned
            .iter()
            .chain(self.active.iter())
            .chain(self.groups.iter().flat_map(|g| g.rows.iter()))
    }
}

/// How much of the list must be redrawn to go from the last model to the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Redraw {
    /// The same rows in the same order: keep every widget (only the age labels, which follow the
    /// clock, are brought up to date).
    InPlace,
    /// The rows are the same but the shown row or the window focus changed: keep the widgets,
    /// restyle the glow and the selection.
    Restyle,
    /// The rows (or the search) changed, or nothing is drawn yet: rebuild the list.
    Rebuild,
}

/// What the next redraw has to do, given the model that is on screen.
pub fn redraw_for(drawn: Option<&SidebarModel>, next: &SidebarModel) -> Redraw {
    let Some(drawn) = drawn else {
        return Redraw::Rebuild;
    };
    if drawn.pinned != next.pinned
        || drawn.active != next.active
        || drawn.groups != next.groups
        || drawn.query != next.query
    {
        Redraw::Rebuild
    } else if drawn.selected != next.selected || drawn.focused != next.focused {
        Redraw::Restyle
    } else {
        Redraw::InPlace
    }
}

/// A compact age: `now`, `5m`, `3h`, `2d`, `6w`.
pub fn relative_time(now_ms: i64, then_ms: i64) -> String {
    let secs = (now_ms - then_ms).max(0) / 1000;
    match secs {
        s if s < 60 => "now".to_owned(),
        s if s < 3_600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3_600),
        s if s < 14 * 86_400 => format!("{}d", s / 86_400),
        s => format!("{}w", s / (7 * 86_400)),
    }
}

/// A thread title from its first message: the first non-blank line, at most 60 characters.
/// The name a thread is shown under: its title, or "New thread" while it has none.
pub fn display_title(title: &str) -> &str {
    if title.is_empty() {
        "New thread"
    } else {
        title
    }
}

/// The title of a thread whose first message is an image alone: its file name, else "Image".
/// The first message with words replaces it.
pub fn image_title(image: &agent_core::attachment::AttachmentRef) -> String {
    image
        .name
        .as_deref()
        .map(thread_title)
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "Image".to_owned())
}

pub fn thread_title(text: &str) -> String {
    const MAX: usize = 60;
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default();
    if line.chars().count() <= MAX {
        return line.to_owned();
    }
    let cut: String = line.chars().take(MAX - 1).collect();
    format!("{}…", cut.trim_end())
}

/// What a resumed native session becomes. A Claude or agy session is resumed as a chat
/// thread whose provider thread carries the native id; any other CLI's session can only be
/// resumed in a terminal page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeAs {
    Chat { driver: Driver, native_id: String },
    Terminal,
}

pub fn resume_as(driver: Option<Driver>, session_id: &str) -> ResumeAs {
    match driver {
        Some(driver) => ResumeAs::Chat {
            driver,
            native_id: session_id.to_owned(),
        },
        None => ResumeAs::Terminal,
    }
}

/// The store's provider-thread model, where `"default"` means "the CLI's own default".
pub fn stored_model(model: Option<&str>) -> Option<String> {
    model
        .filter(|m| !m.is_empty() && *m != "default")
        .map(str::to_owned)
}

/// The agent a handoff with no named target goes to: the next one after `from` in
/// [`Driver::ALL`] order (wrapping) that `usable` accepts. `None` when there is no other usable
/// agent.
pub fn handoff_target(from: Driver, usable: impl Fn(Driver) -> bool) -> Option<Driver> {
    let all = Driver::ALL;
    let at = all.iter().position(|d| *d == from)?;
    (1..all.len())
        .map(|step| all[(at + step) % all.len()])
        .find(|d| usable(*d))
}

/// Whether a thread whose user chose `wanted` (unknown: `None`) forbids edits nobody approved.
/// Only Accept edits allows them; Ask and Plan (read-only, as far as the user is concerned) both
/// forbid them, and an unknown mode is treated as forbidding them, the safe guess.
pub fn forbids_unasked_edits(wanted: Option<Mode>) -> bool {
    wanted != Some(Mode::AcceptEdits)
}

/// The agent an automatic handoff from `from` lands on (a rate-limit or unavailable banner, a
/// bare `/handoff`), and whether it would edit files unasked. While `agy_cannot_ask` (the thread
/// forbids unasked edits, see [`forbids_unasked_edits`], and agy's approval hook is not proven),
/// agy is skipped: it would apply edits on its own, in every mode, Plan included. It is still
/// returned, flagged, when no other agent is usable, so the caller can say so and confirm rather
/// than offer nothing.
pub fn safe_handoff_target(
    from: Driver,
    usable: impl Fn(Driver) -> bool,
    agy_cannot_ask: bool,
) -> Option<(Driver, bool)> {
    let unasked = |d: Driver| agy_cannot_ask && d == Driver::Agy;
    handoff_target(from, |d| usable(d) && !unasked(d))
        .map(|d| (d, false))
        .or_else(|| handoff_target(from, &usable).map(|d| (d, unasked(d))))
}

/// Whether `event` shows a rate-limited agent answering again, so its banner (and its
/// "Continue in …") can go: the same test that ends a thread's limit wait, in the window and in
/// its session.
pub use agent_core::limit_wait::limit_over;

/// The wait banner's title while a thread waits for `driver`'s usage limit to reset; `clock`:
/// the reset's local time, when known.
pub fn limit_wait_text(driver: Driver, clock: Option<&str>) -> String {
    let label = driver.info().label;
    match clock {
        Some(clock) => format!("Continues here after {label}'s limit resets at {clock}"),
        None => format!("Continues here once {label} has usage again"),
    }
}

/// The "Continue in a new thread with…" button for a target from [`safe_handoff_target`].
pub fn continue_label(target: Driver, unasked: bool) -> String {
    if unasked {
        format!("Continue in {} (edits without asking)", target.info().label)
    } else {
        format!("Continue in {}", target.info().label)
    }
}

// ---------------------------------------------------------------------------------------------
// The thread context menu
// ---------------------------------------------------------------------------------------------

/// What a thread-menu entry does. Encoded into one string target of the `win.thread-menu`
/// action ([`ThreadAction::encode`]), so the menu holds data and no closures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadAction {
    /// A NEW thread on `driver` (same folder), seeded with this thread's handoff.
    ContinueIn {
        thread: String,
        driver: Driver,
        model: Option<String>,
        effort: Option<String>,
    },
    /// This thread, in place, moved to `driver`/`model`/`effort` like the header's picker.
    SwitchTo {
        thread: String,
        driver: Driver,
        model: Option<String>,
        effort: Option<String>,
    },
    Fork(String),
    /// Opens a window reading the thread beside a fork of it, its source or a sibling fork.
    CompareForks(String),
    Compact(String),
    Rename(String),
    Archive(String),
    Unarchive(String),
    Delete(String),
    OpenFolder(String),
    /// Writes the thread to a file the user picks (Markdown or JSON).
    Export(String),
    ReviewEnvironment(String),
    CopyId(String),
    /// Pins the thread at the end of the Pinned section.
    Pin(String),
    Unpin(String),
    /// Moves a pinned thread up (negative) or down (positive) the Pinned section.
    PinMove(String, i8),
    /// Opens the Tags dialog.
    EditTags(String),
    /// Removes every tag of the thread (with Undo).
    ClearTags(String),
}

/// Separates the fields of an encoded [`ThreadAction`] (never part of an id, model or effort).
const SEP: char = '\u{1f}';

impl ThreadAction {
    /// `kind SEP thread [SEP driver SEP model SEP effort]`; an absent model or effort is empty.
    pub fn encode(&self) -> String {
        let simple = |kind: &str, thread: &str| format!("{kind}{SEP}{thread}");
        let full = |kind: &str,
                    thread: &str,
                    driver: Driver,
                    model: &Option<String>,
                    effort: &Option<String>| {
            format!(
                "{kind}{SEP}{thread}{SEP}{}{SEP}{}{SEP}{}",
                driver.info().key,
                model.as_deref().unwrap_or(""),
                effort.as_deref().unwrap_or("")
            )
        };
        match self {
            Self::ContinueIn {
                thread,
                driver,
                model,
                effort,
            } => full("continue", thread, *driver, model, effort),
            Self::SwitchTo {
                thread,
                driver,
                model,
                effort,
            } => full("switch", thread, *driver, model, effort),
            Self::Fork(t) => simple("fork", t),
            Self::CompareForks(t) => simple("compare-forks", t),
            Self::Compact(t) => simple("compact", t),
            Self::Rename(t) => simple("rename", t),
            Self::Archive(t) => simple("archive", t),
            Self::Unarchive(t) => simple("unarchive", t),
            Self::Delete(t) => simple("delete", t),
            Self::OpenFolder(t) => simple("folder", t),
            Self::Export(t) => simple("export", t),
            Self::ReviewEnvironment(t) => simple("review-environment", t),
            Self::CopyId(t) => simple("copy-id", t),
            Self::Pin(t) => simple("pin", t),
            Self::Unpin(t) => simple("unpin", t),
            Self::PinMove(t, delta) => format!("pin-move{SEP}{t}{SEP}{delta}"),
            Self::EditTags(t) => simple("tags", t),
            Self::ClearTags(t) => simple("clear-tags", t),
        }
    }

    /// The inverse of [`Self::encode`]; `None` for anything else.
    pub fn decode(text: &str) -> Option<Self> {
        let mut parts = text.split(SEP);
        let kind = parts.next()?;
        let thread = parts.next().filter(|t| !t.is_empty())?.to_owned();
        let rest: Vec<&str> = parts.collect();
        let non_empty = |s: &&str| !s.is_empty();
        let agent = |rest: &[&str]| -> Option<(Driver, Option<String>, Option<String>)> {
            let [driver, model, effort] = rest else {
                return None;
            };
            Some((
                parse_driver(driver)?,
                Some(*model).filter(non_empty).map(str::to_owned),
                Some(*effort).filter(non_empty).map(str::to_owned),
            ))
        };
        let simple = |action: Self| rest.is_empty().then_some(action);
        match kind {
            "continue" => agent(&rest).map(|(driver, model, effort)| Self::ContinueIn {
                thread,
                driver,
                model,
                effort,
            }),
            "switch" => agent(&rest).map(|(driver, model, effort)| Self::SwitchTo {
                thread,
                driver,
                model,
                effort,
            }),
            "fork" => simple(Self::Fork(thread)),
            "compare-forks" => simple(Self::CompareForks(thread)),
            "compact" => simple(Self::Compact(thread)),
            "rename" => simple(Self::Rename(thread)),
            "archive" => simple(Self::Archive(thread)),
            "unarchive" => simple(Self::Unarchive(thread)),
            "delete" => simple(Self::Delete(thread)),
            "folder" => simple(Self::OpenFolder(thread)),
            "export" => simple(Self::Export(thread)),
            "review-environment" => simple(Self::ReviewEnvironment(thread)),
            "copy-id" => simple(Self::CopyId(thread)),
            "pin" => simple(Self::Pin(thread)),
            "unpin" => simple(Self::Unpin(thread)),
            "pin-move" => match rest.as_slice() {
                [delta] => delta.parse().ok().map(|d| Self::PinMove(thread, d)),
                _ => None,
            },
            "tags" => simple(Self::EditTags(thread)),
            "clear-tags" => simple(Self::ClearTags(thread)),
            _ => None,
        }
    }
}

/// One entry of a thread's context menu, before it becomes a `gio::Menu`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuEntry {
    Item {
        label: String,
        action: ThreadAction,
        enabled: bool,
    },
    Submenu {
        label: String,
        enabled: bool,
        entries: Vec<MenuEntry>,
    },
    /// A group set off by a separator.
    Section(Vec<MenuEntry>),
}

/// An enabled agent and the models the catalogue lists for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuAgent {
    pub driver: Driver,
    pub models: Vec<agent_core::catalog::CatalogModel>,
}

/// Everything the thread menu depends on.
#[derive(Debug, Clone)]
pub struct ThreadMenuInput {
    pub thread: String,
    pub archived: bool,
    /// The thread is pinned: it offers Unpin, Move Up and Move Down instead of Pin to Top.
    pub pinned: bool,
    /// The thread has tags: it offers Clear Tags.
    pub tagged: bool,
    /// The thread has at least one message to hand over ("Continue in a new thread with…" needs one).
    pub has_messages: bool,
    /// The thread is a fork, or was forked, and the other thread still exists: it offers
    /// Compare with Fork….
    pub forks: bool,
    /// Only enabled (and installed) agents appear.
    pub agents: Vec<MenuAgent>,
}

/// The agents a menu offers: every driver `usable` accepts (see `availability`), in registry
/// order, each with the catalogue models listed for it. A cached model list of an agent that is
/// not usable is kept in the catalogue but never shown.
pub fn menu_agents(
    catalog: &[agent_core::catalog::CatalogModel],
    usable: impl Fn(Driver) -> bool,
) -> Vec<MenuAgent> {
    Driver::ALL
        .into_iter()
        .filter(|d| usable(*d))
        .map(|driver| MenuAgent {
            driver,
            models: catalog
                .iter()
                .filter(|m| m.driver == driver)
                .cloned()
                .collect(),
        })
        .collect()
}

/// The model and effort to hand to `switch` for a catalogue row at `effort`: agy folds the effort
/// into the model id, the others take it separately.
fn model_choice(
    model: &agent_core::catalog::CatalogModel,
    effort: Option<&str>,
) -> (Option<String>, Option<String>) {
    let effort = effort.filter(|e| model.efforts.iter().any(|x| x == e));
    (Some(model.model_id_for(effort)), effort.map(str::to_owned))
}

/// An agent's submenu: its default model first, then every model (a model with efforts is a
/// submenu of them, the model's own default marked).
fn agent_entries(
    agent: &MenuAgent,
    enabled: bool,
    action: impl Fn(Driver, Option<String>, Option<String>) -> ThreadAction,
) -> Vec<MenuEntry> {
    let mut entries = vec![MenuEntry::Item {
        label: "Default model".to_owned(),
        action: action(agent.driver, None, None),
        enabled,
    }];
    for m in &agent.models {
        if m.efforts.is_empty() {
            let (model, effort) = model_choice(m, None);
            entries.push(MenuEntry::Item {
                label: m.display.clone(),
                action: action(agent.driver, model, effort),
                enabled,
            });
            continue;
        }
        let default = m.default_effort();
        let mut efforts = vec![MenuEntry::Item {
            label: "Default effort".to_owned(),
            action: action(agent.driver, Some(m.model_id_for(None)), None),
            enabled,
        }];
        for e in &m.efforts {
            let (model, effort) = model_choice(m, Some(e));
            efforts.push(MenuEntry::Item {
                label: if default == Some(e.as_str()) {
                    format!("{e} (default)")
                } else {
                    e.clone()
                },
                action: action(agent.driver, model, effort),
                enabled,
            });
        }
        entries.push(MenuEntry::Submenu {
            label: m.display.clone(),
            enabled,
            entries: efforts,
        });
    }
    entries
}

/// The right-click menu of a thread: continue it in another agent (a new thread), switch it in
/// place, then rename / archive / delete, then open its folder and copy its id.
pub fn thread_menu(input: &ThreadMenuInput) -> Vec<MenuEntry> {
    let by_agent =
        |label: &str,
         enabled: bool,
         action: &dyn Fn(Driver, Option<String>, Option<String>) -> ThreadAction| {
            MenuEntry::Submenu {
                label: label.to_owned(),
                enabled: enabled && !input.agents.is_empty(),
                entries: input
                    .agents
                    .iter()
                    .map(|agent| MenuEntry::Submenu {
                        label: agent.driver.info().label.to_owned(),
                        enabled,
                        entries: agent_entries(agent, enabled, action),
                    })
                    .collect(),
            }
        };
    let thread = input.thread.clone();
    let t = thread.clone();
    let continue_in = by_agent(
        "Continue in a new thread with…",
        input.has_messages,
        &move |driver, model, effort| ThreadAction::ContinueIn {
            thread: t.clone(),
            driver,
            model,
            effort,
        },
    );
    let t = thread.clone();
    let switch_to = by_agent(
        "Switch agent (stops the current turn)",
        true,
        &move |driver, model, effort| ThreadAction::SwitchTo {
            thread: t.clone(),
            driver,
            model,
            effort,
        },
    );
    let item = |label: &str, action: ThreadAction| MenuEntry::Item {
        label: label.to_owned(),
        action,
        enabled: true,
    };
    vec![
        MenuEntry::Section(
            vec![
                continue_in,
                switch_to,
                MenuEntry::Item {
                    label: "Fork thread".to_owned(),
                    action: ThreadAction::Fork(thread.clone()),
                    enabled: input.has_messages,
                },
            ]
            .into_iter()
            .chain(input.forks.then(|| {
                item(
                    "Compare with Fork…",
                    ThreadAction::CompareForks(thread.clone()),
                )
            }))
            .chain([MenuEntry::Item {
                label: "Compact into a new thread".to_owned(),
                action: ThreadAction::Compact(thread.clone()),
                enabled: input.has_messages,
            }])
            .collect(),
        ),
        MenuEntry::Section({
            let mut entries = vec![
                item("Rename…", ThreadAction::Rename(thread.clone())),
                if input.pinned {
                    item("Unpin", ThreadAction::Unpin(thread.clone()))
                } else {
                    item("Pin to Top", ThreadAction::Pin(thread.clone()))
                },
            ];
            if input.pinned {
                entries.push(item("Move Up", ThreadAction::PinMove(thread.clone(), -1)));
                entries.push(item("Move Down", ThreadAction::PinMove(thread.clone(), 1)));
            }
            entries.push(item("Tags…", ThreadAction::EditTags(thread.clone())));
            if input.tagged {
                entries.push(item("Clear Tags", ThreadAction::ClearTags(thread.clone())));
            }
            entries.push(if input.archived {
                item("Unarchive", ThreadAction::Unarchive(thread.clone()))
            } else {
                item("Archive", ThreadAction::Archive(thread.clone()))
            });
            entries.push(item("Delete…", ThreadAction::Delete(thread.clone())));
            entries
        }),
        MenuEntry::Section(vec![
            item("Open folder", ThreadAction::OpenFolder(thread.clone())),
            MenuEntry::Item {
                label: "Export…".to_owned(),
                action: ThreadAction::Export(thread.clone()),
                enabled: input.has_messages,
            },
            MenuEntry::Item {
                label: "Review Multi-Agent Environment…".to_owned(),
                action: ThreadAction::ReviewEnvironment(thread.clone()),
                enabled: !input.agents.is_empty(),
            },
            item("Copy thread id", ThreadAction::CopyId(thread)),
        ]),
    ]
}

pub fn parse_driver(name: &str) -> Option<Driver> {
    Driver::from_key(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(text: &str) -> SearchQuery {
        SearchQuery::parse(text)
    }

    #[test]
    fn an_empty_sidebar_says_why_by_whether_a_search_is_on() {
        assert_eq!(empty_sidebar_text(&q("")).0, "No Threads Yet");
        assert_eq!(empty_sidebar_text(&q("   ")).0, "No Threads Yet");
        assert_eq!(empty_sidebar_text(&q(" # ")).0, "No Threads Yet");
        let words = empty_sidebar_text(&q("deploy"));
        assert_eq!(words.0, "No Matching Threads");
        assert!(words.1.contains("other words"));
        let tags = empty_sidebar_text(&q("deploy #review"));
        assert_eq!(tags.0, "No Matching Threads");
        assert!(tags.1.contains("tag"), "{}", tags.1);
    }

    #[test]
    fn switches_ask_for_every_busy_reason() {
        for running in [false, true] {
            for background in [0, 2] {
                for approval in [false, true] {
                    let reason = busy_reason(running, background, false, approval);
                    assert_eq!(reason.is_some(), is_busy(running, background > 0, approval));
                    if background > 0 {
                        assert!(reason.unwrap().contains("2 background tasks will stop"));
                    }
                }
            }
        }
        assert!(busy_reason(false, 1, false, false)
            .unwrap()
            .contains("1 background task will stop"));
        // Delegated sub-agents at work, nothing else: the switch still asks, and says why.
        assert!(busy_reason(false, 0, true, false)
            .is_some_and(|why| why.contains("sub-agents' work will stop")));
    }

    #[test]
    fn a_thread_open_elsewhere_has_its_own_state() {
        assert_eq!(OpenState::Elsewhere.description(), "Open in another window");
        assert_ne!(OpenState::Elsewhere, OpenState::Here);
        assert_ne!(OpenState::Elsewhere, OpenState::Closed);
        let mut elsewhere = row("t", "/r", 0);
        elsewhere.open = OpenState::Elsewhere;
        elsewhere.archived = true;
        assert_eq!(group_rows(vec![elsewhere], &q(""), false).len(), 1);
    }

    fn model_row(id: &str, badge: Option<Badge>) -> SidebarRow {
        SidebarRow {
            key: RowKey::Thread(id.into()),
            title: id.into(),
            folder: "/work".into(),
            updated_ms: 1_000,
            driver: None,
            badge,
            activity: None,
            background: Vec::new(),
            open: OpenState::Here,
            archived: false,
            pin_rank: None,
            remote: false,
            tags: Vec::new(),
        }
    }

    #[test]
    fn a_threads_activity_follows_its_agent_process() {
        use ThreadActivity::{Live, Paused, Starting};
        // (preparing, alive, starting, suspended) -> activity
        let table = [
            ((false, false, false, false), None),
            ((true, false, false, false), Some(Starting)),
            ((false, true, true, false), Some(Starting)),
            ((true, true, true, false), Some(Starting)),
            ((false, true, false, false), Some(Live)),
            // The window is still applying what waited for the session: not up yet.
            ((true, true, false, false), Some(Starting)),
            ((false, false, false, true), Some(Paused)),
            // A resume clears the pause before it spawns: a live process is never paused.
            ((false, true, true, true), Some(Starting)),
            ((false, true, false, true), Some(Live)),
            // An exited process with no pause behind it: nothing to show.
            ((false, false, true, false), None),
        ];
        for ((preparing, alive, starting, suspended), want) in table {
            assert_eq!(
                thread_activity(preparing, alive, starting, suspended),
                want,
                "preparing {preparing}, alive {alive}, starting {starting}, suspended {suspended}"
            );
        }
        assert!(Paused.tooltip().contains("resumes when you send"));
    }

    #[test]
    fn the_empty_state_offers_a_reopen_only_when_there_is_something_to_reopen() {
        assert_eq!(reopen_offer(0), None);
        assert_eq!(
            reopen_offer(1).as_deref(),
            Some("Reopen 1 thread from your last session")
        );
        assert_eq!(
            reopen_offer(4).as_deref(),
            Some("Reopen 4 threads from your last session")
        );
    }

    #[test]
    fn the_import_toast_counts_sessions_and_names_each_agent_once() {
        let (c, a, x) = (Driver::Claude, Driver::Agy, Driver::Codex);
        let name = |d: Driver| d.info().label;
        assert_eq!(sessions_added_toast(&[]), None);
        assert_eq!(
            sessions_added_toast(&[c]).as_deref(),
            Some(format!("Added 1 recent session from {}", name(c)).as_str())
        );
        assert_eq!(
            sessions_added_toast(&[c, c, a]).as_deref(),
            Some(format!("Added 3 recent sessions from {} and {}", name(c), name(a)).as_str())
        );
        assert_eq!(
            sessions_added_toast(&[x, c, x, a]).as_deref(),
            Some(
                format!(
                    "Added 4 recent sessions from {}, {} and {}",
                    name(x),
                    name(c),
                    name(a)
                )
                .as_str()
            )
        );
    }

    #[test]
    fn a_change_of_activity_redraws_the_row() {
        let rows = vec![model_row("a", None)];
        let drawn = model_of(rows.clone(), Some("a"), true);
        let mut paused = rows;
        paused[0].activity = Some(ThreadActivity::Paused);
        assert_eq!(
            redraw_for(Some(&drawn), &model_of(paused, Some("a"), true)),
            Redraw::Rebuild
        );
    }

    fn model_of(rows: Vec<SidebarRow>, selected: Option<&str>, focused: bool) -> SidebarModel {
        let SidebarSections {
            pinned,
            active,
            groups,
        } = sidebar_sections(rows, &q(""), false);
        SidebarModel {
            pinned_active: pinned.iter().filter(|r| is_active(r)).count(),
            pinned,
            active,
            groups: fold_groups(groups, &Default::default(), true, None),
            selected: selected.map(|id| RowKey::Thread(id.into())),
            focused,
            query: String::new(),
        }
    }

    #[test]
    fn nothing_drawn_yet_means_a_rebuild() {
        let next = model_of(vec![model_row("a", None)], None, true);
        assert_eq!(redraw_for(None, &next), Redraw::Rebuild);
    }

    #[test]
    fn the_same_model_keeps_every_widget() {
        let rows = vec![model_row("a", None), model_row("b", Some(Badge::Running))];
        let drawn = model_of(rows.clone(), Some("a"), true);
        assert_eq!(
            redraw_for(Some(&drawn), &model_of(rows, Some("a"), true)),
            Redraw::InPlace
        );
    }

    #[test]
    fn focus_or_the_shown_row_changing_only_restyles() {
        let rows = vec![
            model_row("a", Some(Badge::NeedsApproval)),
            model_row("b", None),
        ];
        let drawn = model_of(rows.clone(), Some("a"), true);
        for next in [
            model_of(rows.clone(), Some("a"), false),
            model_of(rows.clone(), Some("b"), true),
            model_of(rows.clone(), None, true),
        ] {
            assert_eq!(redraw_for(Some(&drawn), &next), Redraw::Restyle);
        }
    }

    #[test]
    fn a_changed_row_or_search_rebuilds() {
        let rows = vec![model_row("a", None), model_row("b", None)];
        let drawn = model_of(rows.clone(), Some("a"), true);
        // A new badge, a new title, a row less, a newer activity time (which also re-orders).
        let mut badged = rows.clone();
        badged[0].badge = Some(Badge::Unread);
        let mut renamed = rows.clone();
        renamed[1].title = "other".into();
        let mut newer = rows.clone();
        newer[1].updated_ms += 60_000;
        for next in [
            model_of(badged, Some("a"), true),
            model_of(renamed, Some("a"), true),
            model_of(rows[..1].to_vec(), Some("a"), true),
            model_of(newer, Some("a"), true),
        ] {
            assert_eq!(redraw_for(Some(&drawn), &next), Redraw::Rebuild);
        }
        let mut searched = model_of(rows, Some("a"), true);
        searched.query = "a".into();
        assert_eq!(redraw_for(Some(&drawn), &searched), Redraw::Rebuild);
    }

    #[test]
    fn model_rows_follow_the_drawing_order() {
        let model = model_of(
            vec![
                model_row("idle", None),
                model_row("busy", Some(Badge::Running)),
            ],
            None,
            true,
        );
        let ids: Vec<_> = model.rows().map(|r| r.title.as_str()).collect();
        assert_eq!(ids, ["busy", "idle"]);
    }

    #[test]
    fn busy_means_running_or_waiting_and_the_heading_counts() {
        assert!(!is_busy(false, false, false));
        assert!(is_busy(true, false, false));
        // Sub-agents or background commands still going: closing would cut them off.
        assert!(is_busy(false, true, false));
        assert!(is_busy(false, false, true));
        assert_eq!(stop_heading(1), "Stop 1 running thread and close?");
        assert_eq!(stop_heading(3), "Stop 3 running threads and close?");
    }

    #[test]
    fn a_request_is_announced_unless_you_are_looking_at_it() {
        let r = |shown, focused| react(Attention::Asked, shown, focused, false);
        // Not gated on notify_on_bell (false here).
        assert_eq!((r(false, true).notify, r(false, true).unread), (true, true));
        assert_eq!(
            (r(false, false).notify, r(false, false).unread),
            (true, true)
        );
        // Shown but the window is away: announced, no unread (the glow carries it).
        assert_eq!(
            (r(true, false).notify, r(true, false).unread),
            (true, false)
        );
        assert_eq!((r(true, true).notify, r(true, true).unread), (false, false));
    }

    #[test]
    fn an_expired_approval_leaves_a_trace_unless_you_are_looking() {
        for (shown, focused, unread) in [
            (false, true, true),
            (false, false, true),
            (true, false, true),
            (true, true, false),
        ] {
            let r = react(Attention::Expired, shown, focused, true);
            assert_eq!((r.unread, r.notify), (unread, false), "{shown} {focused}");
        }
    }

    #[test]
    fn a_finished_turn_is_unread_when_not_looked_at_and_notifies_only_by_setting() {
        // A shown thread in an unfocused window keeps an unread mark.
        assert!(react(Attention::TurnDone, true, false, false).unread);
        assert!(!react(Attention::TurnDone, true, true, true).unread);
        assert!(!react(Attention::TurnDone, true, true, true).notify);
        assert!(!react(Attention::TurnDone, false, true, false).notify);
        assert!(react(Attention::TurnDone, false, true, true).notify);
        assert!(react(Attention::TurnDone, true, false, true).notify);
    }

    fn row(id: &str, folder: &str, at: i64) -> SidebarRow {
        SidebarRow {
            key: RowKey::Thread(id.into()),
            title: format!("Thread {id}"),
            folder: folder.into(),
            updated_ms: at,
            driver: Some(Driver::Claude),
            badge: None,
            activity: None,
            background: Vec::new(),
            open: OpenState::Closed,
            archived: false,
            pin_rank: None,
            remote: false,
            tags: Vec::new(),
        }
    }

    #[test]
    fn a_thread_waiting_for_approval_glows_only_while_you_are_away_from_it() {
        let mut waiting = row("w", "/w", 1);
        waiting.badge = Some(Badge::NeedsApproval);
        let shown = |id: &str| RowKey::Thread(id.into());
        // Another thread shown, or this one shown in an unfocused window: it glows.
        assert!(needs_attention(&waiting, Some(&shown("other")), true));
        assert!(needs_attention(&waiting, None, true));
        assert!(needs_attention(&waiting, Some(&shown("w")), false));
        // In front of you: no glow (the approval card is right there).
        assert!(!needs_attention(&waiting, Some(&shown("w")), true));
        // Other states never glow.
        let mut running = row("r", "/w", 1);
        running.badge = Some(Badge::Running);
        assert!(!needs_attention(&running, Some(&shown("other")), false));
        assert!(!needs_attention(&row("n", "/w", 1), None, false));
    }

    /// Folder groups start collapsed; an expanded one, or any during a search, lists every row,
    /// and a collapsed one keeps only the thread on screen, counting the rest as hidden.
    #[test]
    fn folder_groups_start_collapsed_but_keep_the_shown_thread() {
        let groups = group_rows(
            vec![
                row("a", "/w/one", 3),
                row("b", "/w/one", 2),
                row("c", "/w/two", 1),
            ],
            &q(""),
            false,
        );
        let shape = |folded: Vec<FolderGroup>| -> Vec<(String, bool, usize, Vec<String>)> {
            folded
                .into_iter()
                .map(|g| {
                    let ids = g.rows.iter().map(|r| r.title.clone()).collect();
                    (g.folder, g.expanded, g.hidden, ids)
                })
                .collect()
        };
        let none = std::collections::HashSet::new();
        assert_eq!(
            shape(fold_groups(groups.clone(), &none, false, None)),
            [
                ("/w/one".into(), false, 2, vec![]),
                ("/w/two".into(), false, 1, vec![]),
            ],
            "collapsed by default"
        );
        let shown = RowKey::Thread("b".into());
        assert_eq!(
            shape(fold_groups(groups.clone(), &none, false, Some(&shown)))[0],
            ("/w/one".into(), false, 1, vec!["Thread b".into()]),
            "the shown thread stays in its collapsed folder"
        );
        let one: std::collections::HashSet<String> = ["/w/one".to_owned()].into();
        assert_eq!(
            shape(fold_groups(groups.clone(), &one, false, None)),
            [
                (
                    "/w/one".into(),
                    true,
                    0,
                    vec!["Thread a".into(), "Thread b".into()]
                ),
                ("/w/two".into(), false, 1, vec![]),
            ]
        );
        assert!(
            fold_groups(groups, &none, true, None)
                .iter()
                .all(|g| g.expanded && g.hidden == 0),
            "a search shows every match"
        );
    }

    /// Active threads get their own section, most urgent first, and leave their folder group;
    /// a folder left empty disappears, and the rest re-order by what is left in them. Search and
    /// the archive toggle apply to both sections.
    #[test]
    fn active_threads_get_their_own_section() {
        let id = |r: &SidebarRow| match &r.key {
            RowKey::Thread(id) => id.clone(),
            RowKey::Terminal(_) => "t".into(),
        };
        let with = |mut r: SidebarRow, badge: Badge| {
            r.badge = Some(badge);
            r
        };
        let rows = vec![
            with(row("run-old", "/w/one", 10), Badge::Running),
            with(row("ask", "/w/two", 5), Badge::NeedsApproval),
            row("idle", "/w/one", 50),
            with(row("bg", "/w/three", 60), Badge::Background),
            with(row("unread", "/w/two", 20), Badge::Unread),
            with(row("run-new", "/w/two", 40), Badge::Running),
            with(row("limited", "/w/four", 1), Badge::RateLimited),
        ];
        let SidebarSections { active, groups, .. } = sidebar_sections(rows.clone(), &q(""), false);
        assert_eq!(
            active.iter().map(id).collect::<Vec<_>>(),
            ["ask", "limited", "run-new", "run-old", "bg"],
            "approval, rate limit, working (newest first), background"
        );
        let shape: Vec<(String, Vec<String>)> = groups
            .iter()
            .map(|(f, r)| (f.clone(), r.iter().map(id).collect()))
            .collect();
        assert_eq!(
            shape,
            [
                ("/w/one".to_owned(), vec!["idle".to_owned()]),
                ("/w/two".to_owned(), vec!["unread".to_owned()]),
            ],
            "unread is not active; emptied folders go; ordered by what is left"
        );
        assert!(!is_active(&with(row("u", "/w", 1), Badge::Unread)));
        assert!(!is_active(&row("n", "/w", 1)));

        // The search filters the active section too; nothing active: no section.
        let SidebarSections { active, groups, .. } =
            sidebar_sections(rows, &q("thread ask"), false);
        assert_eq!(active.iter().map(id).collect::<Vec<_>>(), ["ask"]);
        assert!(groups.is_empty());
        let sections = sidebar_sections(vec![row("x", "/w", 1)], &q(""), false);
        assert!(sections.active.is_empty() && sections.pinned.is_empty());
    }

    #[test]
    fn archived_rows_hide_behind_the_toggle_unless_open() {
        let mut archived = row("old", "/w/one", 5);
        archived.archived = true;
        let mut open_archived = row("open", "/w/one", 6);
        open_archived.archived = true;
        open_archived.open = OpenState::Here;
        let rows = vec![row("a", "/w/one", 10), archived, open_archived];
        let ids = |groups: Vec<(String, Vec<SidebarRow>)>| -> Vec<String> {
            groups
                .into_iter()
                .flat_map(|(_, r)| r)
                .map(|r| match r.key {
                    RowKey::Thread(id) => id,
                    RowKey::Terminal(_) => "t".into(),
                })
                .collect()
        };
        assert_eq!(ids(group_rows(rows.clone(), &q(""), false)), ["a", "open"]);
        assert_eq!(
            ids(group_rows(rows.clone(), &q(""), true)),
            ["a", "open", "old"]
        );
        // The search still applies to what is shown.
        assert_eq!(ids(group_rows(rows, &q("thread old"), true)), ["old"]);
    }

    #[test]
    fn rows_group_by_folder_newest_group_and_row_first() {
        let groups = group_rows(
            vec![
                row("a", "/w/one", 10),
                row("b", "/w/two", 30),
                row("c", "/w/one", 40),
                row("d", "/w/two", 20),
            ],
            &q(""),
            false,
        );
        let shape: Vec<(&str, Vec<&str>)> = groups
            .iter()
            .map(|(f, rows)| {
                (
                    f.as_str(),
                    rows.iter()
                        .map(|r| match &r.key {
                            RowKey::Thread(id) => id.as_str(),
                            RowKey::Terminal(_) => "t",
                        })
                        .collect(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            [("/w/one", vec!["c", "a"]), ("/w/two", vec!["b", "d"])]
        );
    }

    #[test]
    fn search_matches_title_or_folder_case_insensitively() {
        let rows = vec![row("a", "/w/One", 1), row("b", "/w/two", 2)];
        assert_eq!(group_rows(rows.clone(), &q("one"), false).len(), 1);
        assert_eq!(
            group_rows(rows.clone(), &q("THREAD B"), false)[0].0,
            "/w/two"
        );
        assert!(group_rows(rows, &q("nothing"), false).is_empty());
    }

    // ---- the thread menu ----

    fn model(driver: Driver, id: &str, efforts: &[&str]) -> agent_core::catalog::CatalogModel {
        agent_core::catalog::CatalogModel {
            driver,
            id: id.into(),
            display: id.to_uppercase(),
            description: None,
            efforts: efforts.iter().map(|e| (*e).to_owned()).collect(),
            default_effort: None,
            via: None,
            tools: None,
        }
    }

    fn input(has_messages: bool, archived: bool) -> ThreadMenuInput {
        ThreadMenuInput {
            thread: "t1".into(),
            archived,
            pinned: false,
            tagged: false,
            has_messages,
            forks: false,
            agents: vec![
                MenuAgent {
                    driver: Driver::Claude,
                    models: vec![model(Driver::Claude, "opus", &["low", "high"])],
                },
                MenuAgent {
                    driver: Driver::Agy,
                    models: vec![model(Driver::Agy, "gemini-pro", &["low", "high"])],
                },
                MenuAgent {
                    driver: Driver::Codex,
                    models: vec![model(Driver::Codex, "gpt", &[])],
                },
            ],
        }
    }

    /// Every item reachable in a submenu, depth first, as (label, action).
    fn items(entries: &[MenuEntry]) -> Vec<(String, ThreadAction, bool)> {
        entries
            .iter()
            .flat_map(|e| match e {
                MenuEntry::Item {
                    label,
                    action,
                    enabled,
                } => vec![(label.clone(), action.clone(), *enabled)],
                MenuEntry::Submenu { entries, .. } | MenuEntry::Section(entries) => items(entries),
            })
            .collect()
    }

    fn submenu<'a>(entries: &'a [MenuEntry], label: &str) -> &'a MenuEntry {
        entries
            .iter()
            .flat_map(|e| match e {
                MenuEntry::Section(s) => s.iter().collect::<Vec<_>>(),
                other => vec![other],
            })
            .find(|e| matches!(e, MenuEntry::Submenu { label: l, .. } if l == label))
            .unwrap_or_else(|| panic!("no submenu {label}"))
    }

    #[test]
    fn the_menu_offers_every_enabled_agent_with_its_models_and_efforts() {
        let menu = thread_menu(&input(true, false));
        let MenuEntry::Submenu {
            entries: agents,
            enabled,
            ..
        } = submenu(&menu, "Continue in a new thread with…")
        else {
            panic!("a submenu");
        };
        assert!(*enabled);
        let labels: Vec<String> = agents
            .iter()
            .map(|a| match a {
                MenuEntry::Submenu { label, .. } => label.clone(),
                _ => panic!("agent submenus only"),
            })
            .collect();
        assert_eq!(
            labels,
            [
                "Claude".to_owned(),
                "Antigravity".to_owned(),
                "Codex".to_owned(),
            ]
        );
        // Plain labels: no emoji (a PopoverMenu submenu entry draws no icon).
        assert!(labels.iter().all(|l| l.is_ascii()));
        let all = items(&[submenu(&menu, "Continue in a new thread with…").clone()]);
        let claude: Vec<_> = all
            .iter()
            .filter(|(_, a, _)| {
                matches!(
                    a,
                    ThreadAction::ContinueIn {
                        driver: Driver::Claude,
                        ..
                    }
                )
            })
            .collect();
        // Default model, the model's default effort and each effort.
        assert_eq!(claude.len(), 4);
        assert_eq!(claude[0].0, "Default model");
        assert!(matches!(
            &claude[0].1,
            ThreadAction::ContinueIn {
                model: None,
                effort: None,
                ..
            }
        ));
        assert!(claude.iter().any(|(l, a, _)| l == "high"
            && matches!(a, ThreadAction::ContinueIn { model: Some(m), effort: Some(e), .. }
                if m == "opus" && e == "high")));
        // agy folds the effort into the model id.
        // (its first effort is the default when `medium` is not offered, and is marked as such)
        assert!(all.iter().any(|(l, a, _)| l == "low (default)"
            && matches!(a, ThreadAction::ContinueIn { driver: Driver::Agy, model: Some(m), effort: Some(e), .. }
                if m == "gemini-pro-low" && e == "low")));
        // A model with no efforts is a plain item.
        assert!(all.iter().any(|(l, a, _)| l == "GPT"
            && matches!(a, ThreadAction::ContinueIn { driver: Driver::Codex, model: Some(m), effort: None, .. }
                if m == "gpt")));
        // Switch uses the same agents with its own action.
        let switch = items(&[submenu(&menu, "Switch agent (stops the current turn)").clone()]);
        assert_eq!(switch.len(), all.len());
        assert!(switch
            .iter()
            .all(|(_, a, on)| *on && matches!(a, ThreadAction::SwitchTo { .. })));
    }

    #[test]
    fn only_usable_agents_reach_the_menu_even_when_the_catalogue_caches_their_models() {
        let catalog = vec![
            model(Driver::Claude, "opus", &[]),
            model(Driver::Agy, "gemini", &[]),
            model(Driver::Codex, "gpt", &[]),
        ];
        let agents = menu_agents(&catalog, |d| matches!(d, Driver::Claude | Driver::Codex));
        assert_eq!(
            agents.iter().map(|a| a.driver).collect::<Vec<_>>(),
            [Driver::Claude, Driver::Codex]
        );
        assert!(agents.iter().all(|a| a.models.len() == 1));
        assert!(menu_agents(&catalog, |_| false).is_empty());
    }

    #[test]
    fn continue_is_disabled_without_messages_and_empty_without_agents() {
        let menu = thread_menu(&input(false, false));
        let cont = items(&[submenu(&menu, "Continue in a new thread with…").clone()]);
        assert!(!cont.is_empty() && cont.iter().all(|(_, _, on)| !on));
        // Switching in place does not need a history.
        let switch = items(&[submenu(&menu, "Switch agent (stops the current turn)").clone()]);
        assert!(switch.iter().all(|(_, _, on)| *on));

        let mut none = input(true, false);
        none.agents.clear();
        let menu = thread_menu(&none);
        for label in [
            "Continue in a new thread with…",
            "Switch agent (stops the current turn)",
        ] {
            assert!(matches!(
                submenu(&menu, label),
                MenuEntry::Submenu { enabled: false, entries, .. } if entries.is_empty()
            ));
        }
    }

    #[test]
    fn archive_toggles_and_the_housekeeping_entries_are_always_there() {
        let labels = |archived| -> Vec<String> {
            let menu = thread_menu(&input(true, archived));
            menu.iter()
                .filter_map(|e| match e {
                    MenuEntry::Section(s) => Some(s),
                    _ => None,
                })
                .flat_map(|s| s.iter())
                .filter_map(|e| match e {
                    MenuEntry::Item { label, .. } => Some(label.clone()),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(
            labels(false),
            [
                "Fork thread",
                "Compact into a new thread",
                "Rename…",
                "Pin to Top",
                "Tags…",
                "Archive",
                "Delete…",
                "Open folder",
                "Export…",
                "Review Multi-Agent Environment…",
                "Copy thread id"
            ]
        );
        assert_eq!(
            labels(true),
            [
                "Fork thread",
                "Compact into a new thread",
                "Rename…",
                "Pin to Top",
                "Tags…",
                "Unarchive",
                "Delete…",
                "Open folder",
                "Export…",
                "Review Multi-Agent Environment…",
                "Copy thread id"
            ]
        );
    }

    #[test]
    fn actions_round_trip_through_their_target_string() {
        let actions = [
            ThreadAction::ContinueIn {
                thread: "t".into(),
                driver: Driver::Codex,
                model: Some("gpt-5-codex".into()),
                effort: Some("high".into()),
            },
            ThreadAction::SwitchTo {
                thread: "t".into(),
                driver: Driver::Agy,
                model: None,
                effort: None,
            },
            ThreadAction::Fork("t".into()),
            ThreadAction::CompareForks("t".into()),
            ThreadAction::Compact("t".into()),
            ThreadAction::Rename("t".into()),
            ThreadAction::Archive("t".into()),
            ThreadAction::Unarchive("t".into()),
            ThreadAction::Delete("t".into()),
            ThreadAction::OpenFolder("t".into()),
            ThreadAction::Export("t".into()),
            ThreadAction::ReviewEnvironment("t".into()),
            ThreadAction::CopyId("t".into()),
        ];
        let actions = actions.into_iter().chain([
            ThreadAction::Pin("t".into()),
            ThreadAction::Unpin("t".into()),
            ThreadAction::PinMove("t".into(), -1),
            ThreadAction::PinMove("t".into(), 1),
            ThreadAction::EditTags("t".into()),
            ThreadAction::ClearTags("t".into()),
        ]);
        for a in actions {
            assert_eq!(ThreadAction::decode(&a.encode()), Some(a.clone()), "{a:?}");
        }
        // Garbage and malformed targets decode to nothing, never to a wrong action.
        for bad in [
            "",
            "tags",
            "tags\u{1f}",
            "tags\u{1f}t\u{1f}extra",
            "clear-tags\u{1f}t\u{1f}x",
            "delete",
            "delete\u{1f}",
            "bogus\u{1f}t",
            "delete\u{1f}t\u{1f}extra",
            "continue\u{1f}t\u{1f}nope\u{1f}\u{1f}",
            "continue\u{1f}t\u{1f}claude",
        ] {
            assert_eq!(ThreadAction::decode(bad), None, "{bad:?}");
        }
    }

    fn pinned_row(id: &str, folder: &str, at: i64, rank: i64) -> SidebarRow {
        let mut r = row(id, folder, at);
        r.pin_rank = Some(rank);
        r
    }

    fn ids_of(rows: &[SidebarRow]) -> Vec<String> {
        rows.iter()
            .map(|r| match &r.key {
                RowKey::Thread(id) => id.clone(),
                RowKey::Terminal(_) => "t".into(),
            })
            .collect()
    }

    #[test]
    fn pinned_rows_lead_in_user_order_and_leave_their_folder() {
        let rows = vec![
            row("a", "/w/one", 50),
            pinned_row("p2", "/w/one", 10, 1),
            pinned_row("p1", "/w/two", 90, 0),
            row("b", "/w/two", 20),
        ];
        let sections = sidebar_sections(rows, &q(""), false);
        assert_eq!(
            ids_of(&sections.pinned),
            ["p1", "p2"],
            "the user's order, not the newest first"
        );
        let grouped: Vec<(String, Vec<String>)> = sections
            .groups
            .iter()
            .map(|(f, r)| (f.clone(), ids_of(r)))
            .collect();
        assert_eq!(
            grouped,
            [
                ("/w/one".to_owned(), vec!["a".to_owned()]),
                ("/w/two".to_owned(), vec!["b".to_owned()]),
            ],
            "a pinned row leaves its folder, and the folders keep their own recency order"
        );
        // Only pinned threads: their folders are gone.
        let only = sidebar_sections(vec![pinned_row("p", "/w", 1, 0)], &q(""), false);
        assert!(only.groups.is_empty() && only.active.is_empty());
    }

    #[test]
    fn pinned_active_row_is_listed_once_with_its_badge() {
        let mut busy = pinned_row("busy", "/w", 5, 0);
        busy.badge = Some(Badge::NeedsApproval);
        let mut other = row("other", "/w", 4);
        other.badge = Some(Badge::Running);
        let sections = sidebar_sections(vec![busy, other], &q(""), false);
        assert_eq!(ids_of(&sections.pinned), ["busy"]);
        assert_eq!(sections.pinned[0].badge, Some(Badge::NeedsApproval));
        assert_eq!(
            ids_of(&sections.active),
            ["other"],
            "listed once, in Pinned only"
        );
        assert_eq!(sections.pinned_active(), 1, "the Active header counts it");
    }

    #[test]
    fn archived_pinned_hides_behind_toggle_unless_open() {
        let mut archived = pinned_row("old", "/w", 5, 0);
        archived.archived = true;
        let mut open = pinned_row("open", "/w", 6, 1);
        open.archived = true;
        open.open = OpenState::Here;
        let rows = vec![archived, open];
        assert_eq!(
            ids_of(&sidebar_sections(rows.clone(), &q(""), false).pinned),
            ["open"]
        );
        assert_eq!(
            ids_of(&sidebar_sections(rows.clone(), &q(""), true).pinned),
            ["old", "open"]
        );
        // The search applies to the pinned rows too.
        assert_eq!(
            ids_of(&sidebar_sections(rows, &q("thread old"), true).pinned),
            ["old"]
        );
    }

    #[test]
    fn pin_actions_round_trip_encode_decode() {
        for action in [
            ThreadAction::Pin("t".into()),
            ThreadAction::Unpin("t".into()),
            ThreadAction::PinMove("t".into(), -1),
            ThreadAction::PinMove("t".into(), 1),
            ThreadAction::PinMove("t".into(), i8::MIN),
        ] {
            assert_eq!(
                ThreadAction::decode(&action.encode()),
                Some(action.clone()),
                "{action:?}"
            );
        }
        for bad in [
            "pin",
            "pin\u{1f}",
            "pin\u{1f}t\u{1f}1",
            "unpin\u{1f}t\u{1f}extra",
            "pin-move\u{1f}t",
            "pin-move\u{1f}t\u{1f}",
            "pin-move\u{1f}t\u{1f}up",
            "pin-move\u{1f}t\u{1f}999",
            "pin-move\u{1f}t\u{1f}1\u{1f}2",
        ] {
            assert_eq!(ThreadAction::decode(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn menu_shows_pin_and_move_only_when_they_apply() {
        let labels = |pinned: bool, archived: bool| -> Vec<String> {
            let mut i = input(true, archived);
            i.pinned = pinned;
            items(&thread_menu(&i))
                .into_iter()
                .map(|(label, _, _)| label)
                .filter(|l| {
                    l.to_lowercase().contains("pin") || l.contains("Move") || l.contains("rchive")
                })
                .collect()
        };
        assert_eq!(labels(false, false), ["Pin to Top", "Archive"]);
        assert_eq!(labels(false, true), ["Pin to Top", "Unarchive"]);
        assert_eq!(
            labels(true, false),
            ["Unpin", "Move Up", "Move Down", "Archive"]
        );
        let mut i = input(true, false);
        i.pinned = true;
        let all = items(&thread_menu(&i));
        let action = |label: &str| {
            all.iter()
                .find(|(l, _, _)| l == label)
                .map(|(_, a, on)| (a.clone(), *on))
        };
        assert_eq!(
            action("Move Up"),
            Some((ThreadAction::PinMove("t1".into(), -1), true))
        );
        assert_eq!(
            action("Move Down"),
            Some((ThreadAction::PinMove("t1".into(), 1), true))
        );
        assert_eq!(
            action("Unpin"),
            Some((ThreadAction::Unpin("t1".into()), true))
        );
    }

    #[test]
    fn rows_order_puts_pinned_first_for_keyboard_navigation() {
        let mut busy = row("busy", "/w/one", 5);
        busy.badge = Some(Badge::Running);
        let rows = vec![
            row("a", "/w/one", 50),
            busy,
            pinned_row("p2", "/w/two", 1, 1),
            pinned_row("p1", "/w/one", 2, 0),
        ];
        let model = model_of(rows, None, true);
        let order: Vec<String> = model
            .rows()
            .map(|r| match &r.key {
                RowKey::Thread(id) => id.clone(),
                RowKey::Terminal(_) => "t".into(),
            })
            .collect();
        assert_eq!(order, ["p1", "p2", "busy", "a"]);
    }

    fn tagged(id: &str, folder: &str, at: i64, tags: &[&str]) -> SidebarRow {
        let mut r = row(id, folder, at);
        r.tags = tags
            .iter()
            .map(|t| RowTag {
                name: (*t).to_owned(),
                colour: None,
            })
            .collect();
        r
    }

    fn all_ids(sections: &SidebarSections) -> Vec<String> {
        let mut ids = ids_of(&sections.pinned);
        ids.extend(ids_of(&sections.active));
        for (_, rows) in &sections.groups {
            ids.extend(ids_of(rows));
        }
        ids
    }

    #[test]
    fn hash_terms_filter_by_tag_and_and_together() {
        let rows = vec![
            tagged("both", "/w/one", 5, &["ops", "review"]),
            tagged("review", "/w/two", 4, &["review"]),
            tagged("none", "/w/one", 3, &[]),
            tagged("prefix", "/w/two", 2, &["reviewer"]),
        ];
        let found = |query: &str| all_ids(&sidebar_sections(rows.clone(), &q(query), false));
        assert_eq!(
            found("#review"),
            ["both", "review"],
            "a whole tag, not a prefix"
        );
        assert_eq!(found("#REVIEW"), ["both", "review"]);
        assert_eq!(found("#review #ops"), ["both"], "ANDed");
        assert!(found("#review #missing").is_empty());
        // Tag terms and words together: both must hold.
        assert_eq!(found("#review thread r"), ["review"]);
        assert_eq!(
            found("#review one"),
            ["both"],
            "the folder still matches the words"
        );
        // A lone # is a tag still being typed: it filters nothing.
        assert_eq!(found("#").len(), 4);
        // The filter applies to the pinned and active sections too.
        let mut pinned = tagged("pinned", "/w", 9, &["ops"]);
        pinned.pin_rank = Some(0);
        let mut busy = tagged("busy", "/w", 8, &[]);
        busy.badge = Some(Badge::Running);
        let sections = sidebar_sections(vec![pinned, busy], &q("#ops"), false);
        assert_eq!(ids_of(&sections.pinned), ["pinned"]);
        assert!(
            sections.active.is_empty(),
            "an untagged active row is filtered out"
        );
    }

    #[test]
    fn plain_search_also_matches_tags() {
        let rows = vec![
            tagged("a", "/w", 2, &["blocked"]),
            tagged("b", "/w", 1, &["spike"]),
        ];
        let found = |query: &str| all_ids(&sidebar_sections(rows.clone(), &q(query), false));
        assert_eq!(found("BLOCK"), ["a"], "a word matches inside a tag");
        assert_eq!(found("thread b"), ["b"], "titles still match");
        assert!(found("nothing").is_empty());
        // A terminal page has no tags: a tag filter leaves it out.
        let mut term = row("t", "/w", 3);
        term.key = RowKey::Terminal(7);
        assert_eq!(group_rows(vec![term.clone()], &q(""), false).len(), 1);
        assert!(group_rows(vec![term], &q("#blocked"), false).is_empty());
    }

    #[test]
    fn menu_shows_tags_and_clear_tags_only_when_they_apply() {
        let labels = |tags: bool| -> Vec<(String, ThreadAction)> {
            let mut i = input(true, false);
            i.tagged = tags;
            items(&thread_menu(&i))
                .into_iter()
                .filter(|(label, _, _)| label.contains("Tags"))
                .map(|(label, action, _)| (label, action))
                .collect()
        };
        assert_eq!(
            labels(false),
            [("Tags…".to_owned(), ThreadAction::EditTags("t1".into()))]
        );
        assert_eq!(
            labels(true),
            [
                ("Tags…".to_owned(), ThreadAction::EditTags("t1".into())),
                (
                    "Clear Tags".to_owned(),
                    ThreadAction::ClearTags("t1".into())
                ),
            ]
        );
    }

    #[test]
    fn a_row_shows_two_chips_and_counts_the_rest() {
        let row = tagged("t", "/w", 1, &["a", "b", "c", "d"]);
        let (shown, more) = shown_chips(&row.tags);
        assert_eq!(
            shown.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(more, 2);
        assert_eq!(
            tags_text(&row.tags),
            "#a #b #c #d",
            "the tooltip lists them all"
        );
        let one = tagged("t", "/w", 1, &["a"]);
        assert_eq!(shown_chips(&one.tags).1, 0);
        assert_eq!(shown_chips(&[]).0.len(), 0);
    }

    #[test]
    fn tags_take_their_colours_and_colours_are_tokens() {
        let colours: std::collections::BTreeMap<String, TagColour> =
            [("review".to_owned(), TagColour::Warning)].into();
        let tags = row_tags(&["ops".to_owned(), "review".to_owned()], &colours);
        assert_eq!(
            tags,
            [
                RowTag {
                    name: "ops".into(),
                    colour: None
                },
                RowTag {
                    name: "review".into(),
                    colour: Some(TagColour::Warning)
                },
            ]
        );
        // A colour change is a different row: the sidebar redraws it.
        let mut a = tagged("t", "/w", 1, &["review"]);
        let b = a.clone();
        a.tags[0].colour = Some(TagColour::Danger);
        assert_ne!(a, b);
    }

    #[test]
    fn the_colour_dropdown_maps_both_ways() {
        let choices = colour_choices();
        assert_eq!(choices.len(), TagColour::ALL.len() + 1);
        assert_eq!(choices[0], "No colour");
        assert_eq!(colour_at(0), None);
        assert_eq!(colour_index(None), 0);
        for colour in TagColour::ALL {
            let i = colour_index(Some(colour));
            assert_eq!(colour_at(i), Some(colour));
            assert_eq!(choices[i as usize], colour.label());
        }
        assert_eq!(colour_at(99), None);
    }

    #[test]
    fn a_chip_adds_its_tag_term_once() {
        assert_eq!(with_tag_term("", "review"), "#review");
        assert_eq!(with_tag_term("  ", "review"), "#review");
        assert_eq!(with_tag_term("deploy ", "review"), "deploy #review");
        assert_eq!(with_tag_term("#ops", "review"), "#ops #review");
        assert_eq!(
            with_tag_term("#Review x", "review"),
            "#Review x",
            "already there"
        );
    }

    #[test]
    fn the_tags_dialog_offers_every_tag_and_ticks_the_threads_own() {
        let known = [("ops".to_owned(), 2), ("spike".to_owned(), 1)];
        assert_eq!(
            tag_choices(&known, &["review".to_owned(), "ops".to_owned()]),
            [
                ("ops".to_owned(), true),
                ("review".to_owned(), true),
                ("spike".to_owned(), false),
            ]
        );
        assert_eq!(
            chosen_tags(["ops"], "Blocked, #spike ops").unwrap(),
            ["blocked", "ops", "spike"]
        );
        assert_eq!(chosen_tags([], "").unwrap(), Vec::<String>::new());
        assert!(chosen_tags(["ops"], "not,ok\u{7}").is_err());
        let eight = ["a", "b", "c", "d", "e", "f", "g", "h"];
        assert!(chosen_tags(eight, "").is_ok());
        assert_eq!(
            chosen_tags(eight, "i"),
            Err(agent_core::thread_tag::TagError::TooMany(9))
        );
    }

    #[test]
    fn thread_action_export_round_trips() {
        let action = ThreadAction::Export("t-1".into());
        assert_eq!(action.encode(), "export\u{1f}t-1");
        assert_eq!(ThreadAction::decode(&action.encode()), Some(action));
        assert_eq!(ThreadAction::decode("export\u{1f}"), None);
        assert_eq!(ThreadAction::decode("export\u{1f}t\u{1f}x"), None);
    }

    /// Compare with Fork… is offered only for a thread with a fork relation, right after Fork
    /// thread, for the clicked thread, whether or not it has messages of its own yet.
    #[test]
    fn compare_appears_only_for_threads_with_a_fork_relation() {
        let compare = |menu: &[MenuEntry]| {
            items(menu)
                .into_iter()
                .find(|(_, a, _)| matches!(a, ThreadAction::CompareForks(_)))
        };
        assert_eq!(compare(&thread_menu(&input(true, false))), None);
        for has_messages in [false, true] {
            let mut forked = input(has_messages, false);
            forked.forks = true;
            let menu = thread_menu(&forked);
            assert_eq!(
                compare(&menu),
                Some((
                    "Compare with Fork…".to_owned(),
                    ThreadAction::CompareForks("t1".into()),
                    true
                ))
            );
            let labels: Vec<String> = items(&menu).into_iter().map(|(l, _, _)| l).collect();
            let at = |label: &str| labels.iter().position(|l| l == label);
            assert_eq!(
                at("Compare with Fork…"),
                at("Fork thread").map(|i| i + 1),
                "beside Fork thread"
            );
        }
        let action = ThreadAction::CompareForks("t-1".into());
        assert_eq!(action.encode(), "compare-forks\u{1f}t-1");
        assert_eq!(ThreadAction::decode("compare-forks\u{1f}t\u{1f}x"), None);
    }

    #[test]
    fn export_is_disabled_without_messages() {
        for (has_messages, expected) in [(false, false), (true, true)] {
            let menu = thread_menu(&input(has_messages, false));
            let entry = items(&menu)
                .into_iter()
                .find(|(_, a, _)| matches!(a, ThreadAction::Export(_)))
                .expect("an Export entry");
            assert_eq!(entry.0, "Export…");
            assert_eq!(entry.2, expected);
        }
    }

    #[test]
    fn review_targets_the_clicked_thread_without_needing_history() {
        let mut target = input(false, false);
        target.thread = "clicked-not-selected".into();
        let menu = thread_menu(&target);
        assert!(items(&menu).iter().any(|(_, action, enabled)| *enabled
            && *action == ThreadAction::ReviewEnvironment("clicked-not-selected".into())));
        target.agents.clear();
        assert!(items(&thread_menu(&target))
            .iter()
            .any(|(_, action, enabled)| !*enabled
                && matches!(action, ThreadAction::ReviewEnvironment(_))));
    }

    #[test]
    fn the_most_urgent_badge_wins() {
        assert_eq!(
            badge_for(true, true, true, true, true),
            Some(Badge::NeedsApproval)
        );
        assert_eq!(
            badge_for(true, true, false, true, true),
            Some(Badge::RateLimited)
        );
        assert_eq!(
            badge_for(true, true, false, false, true),
            Some(Badge::Running)
        );
        // The main agent is done but sub-agents still run: not finished, and not "new" yet.
        assert_eq!(
            badge_for(false, true, false, false, true),
            Some(Badge::Background)
        );
        assert_eq!(
            badge_for(false, false, false, false, true),
            Some(Badge::Unread)
        );
        assert_eq!(badge_for(false, false, false, false, false), None);
    }

    #[test]
    fn background_work_is_named_and_holds_the_finish() {
        assert_eq!(
            background_tooltip(&["Review the diff".into(), String::new()]),
            "2 background tasks running: Review the diff"
        );
        assert_eq!(background_tooltip(&[]), "");
        assert_eq!(
            background_tooltip(&["a".into()]),
            "1 background task running: a"
        );
        // Long names are cut (on a character, not a byte) and only the first few are listed.
        let long = "é".repeat(80);
        let tip = background_tooltip(&[long]);
        assert!(tip.ends_with('…') && tip.chars().count() < 100, "{tip}");
        let many: Vec<String> = (0..8).map(|i| format!("t{i}")).collect();
        assert_eq!(
            background_tooltip(&many),
            "8 background tasks running: t0, t1, t2, t3, t4"
        );

        assert!(!turn_finishes_thread(true));
        assert!(turn_finishes_thread(false));
        // A held finish is released once the background work is gone and nothing runs.
        assert!(held_finish_due(true, false, false));
        assert!(!held_finish_due(true, true, false));
        assert!(!held_finish_due(true, false, true));
        assert!(!held_finish_due(false, false, false));
    }

    #[test]
    fn a_spinning_row_says_what_is_running() {
        let bg = vec!["Review the diff".to_owned()];
        assert_eq!(row_tooltip(Badge::Running, &[]), "Working");
        assert_eq!(
            row_tooltip(Badge::Running, &bg),
            "Working; 1 background task running: Review the diff"
        );
        assert_eq!(
            row_tooltip(Badge::Background, &bg),
            "1 background task running: Review the diff"
        );
    }

    #[test]
    fn a_row_with_only_unnamed_background_work_still_explains_its_badge() {
        assert_eq!(
            row_tooltip(Badge::Background, &[]),
            "Background tasks running"
        );
    }

    #[test]
    fn child_asks_follow_each_sub_agent_request() {
        let mut asks = ChildAsks::default();
        assert!(!asks.any());
        asks.requested("w1", Some("r1"));
        asks.requested("w1", Some("r2"));
        asks.requested("w2", None);
        asks.resolved("w1", Some("r1"));
        assert!(asks.any());
        asks.resolved("w1", Some("r2"));
        // An id-less request is withdrawn by any resolution from that sub-agent.
        asks.resolved("w2", Some("x"));
        assert!(!asks.any());
        asks.requested("w1", Some("r1"));
        asks.requested("w1", Some("r2"));
        asks.resolved("w1", None);
        assert!(!asks.any());
        asks.requested("w1", Some("r1"));
        asks.requested("w2", Some("r1"));
        asks.worker_ended("w1");
        assert!(asks.any());
        asks.retain_workers(&["w1"]);
        assert!(!asks.any());
    }

    #[test]
    fn relative_times_are_compact() {
        let now = 1_000_000_000;
        assert_eq!(relative_time(now, now + 5), "now");
        assert_eq!(relative_time(now, now - 5 * 60_000), "5m");
        assert_eq!(relative_time(now, now - 3 * 3_600_000), "3h");
        assert_eq!(relative_time(now, now - 2 * 86_400_000), "2d");
        assert_eq!(relative_time(now, now - 21 * 86_400_000), "3w");
    }

    #[test]
    fn titles_come_from_the_first_line_and_are_bounded() {
        assert_eq!(thread_title("\n  fix the build \nmore"), "fix the build");
        let long = "x".repeat(80);
        let t = thread_title(&long);
        assert_eq!(t.chars().count(), 60);
        assert!(t.ends_with('…'));
        assert_eq!(thread_title(""), "");
    }

    #[test]
    fn an_image_alone_names_the_thread_by_its_file_or_image() {
        let mut image = agent_core::attachment::AttachmentRef {
            id: "a".repeat(64),
            kind: agent_core::attachment::AttachmentKind::Image,
            mime: "image/png".into(),
            width: 1,
            height: 1,
            bytes: 1,
            name: Some("login-bug.png".into()),
            origin: agent_core::attachment::Origin::User,
        };
        assert_eq!(image_title(&image), "login-bug.png");
        image.name = None;
        assert_eq!(image_title(&image), "Image");
        image.name = Some("  ".into());
        assert_eq!(image_title(&image), "Image");
    }

    #[test]
    fn resume_maps_chat_agents_to_threads_and_others_to_terminals() {
        assert_eq!(
            resume_as(Some(Driver::Agy), "abc"),
            ResumeAs::Chat {
                driver: Driver::Agy,
                native_id: "abc".into()
            }
        );
        assert_eq!(resume_as(None, "abc"), ResumeAs::Terminal);
        assert_eq!(stored_model(Some("default")), None);
        assert_eq!(stored_model(Some("opus")).as_deref(), Some("opus"));
        for d in Driver::ALL {
            assert_eq!(parse_driver(d.info().key), Some(d));
            // Exhaustive: a new driver fails to compile until it is in Driver::ALL.
            match d {
                Driver::Claude | Driver::Agy | Driver::Codex | Driver::Local => {}
            }
            assert_ne!(handoff_target(d, |_| true), Some(d));
        }
        assert_eq!(Driver::Codex.info().label, "Codex");
    }

    #[test]
    fn an_untitled_thread_is_shown_as_new_thread() {
        assert_eq!(display_title(""), "New thread");
        assert_eq!(display_title("Fix the build"), "Fix the build");
    }

    /// The 2026-10-11 sequence: the limit, then a turn Claude started itself (no output), then a
    /// retry the limit refused. None of those ends the limit; an answer does.
    #[test]
    fn only_an_answer_ends_a_rate_limit() {
        use agent_core::event::{Event, ItemKind, TurnState, Usage};
        let completed = |output_tokens| Event::TurnCompleted {
            state: TurnState::Completed,
            usage: Some(Usage {
                output_tokens,
                ..Usage::default()
            }),
            cost_usd: None,
            error: None,
            model: None,
            billing: None,
        };
        let started = |kind| Event::ItemStarted {
            kind,
            title: String::new(),
            input: None,
            parent: None,
        };
        for still in [
            Event::TurnStarted { model: None },
            completed(0),
            Event::TurnCompleted {
                state: TurnState::Failed,
                usage: None,
                cost_usd: None,
                error: Some("You've hit your session limit".into()),
                model: None,
                billing: None,
            },
            started(ItemKind::UserMessage),
            Event::Notice {
                text: "You've hit your session limit".into(),
            },
        ] {
            assert!(!limit_over(&still), "{still:?}");
        }
        // A sub-agent still running from before the limit is not the agent answering.
        assert!(!limit_over(&Event::ItemStarted {
            kind: ItemKind::AssistantMessage,
            title: String::new(),
            input: None,
            parent: Some("task-1".into()),
        }));
        assert!(limit_over(&completed(12)));
        assert!(limit_over(&started(ItemKind::AssistantMessage)));
    }

    #[test]
    fn the_default_handoff_target_cycles_to_the_next_usable_agent() {
        use Driver::{Agy, Claude, Codex, Local};
        let all = |_| true;
        assert_eq!(handoff_target(Claude, all), Some(Agy));
        assert_eq!(handoff_target(Agy, all), Some(Codex));
        // A vendor out of quota can carry on on the user's own server.
        assert_eq!(handoff_target(Codex, all), Some(Local));
        assert_eq!(handoff_target(Local, all), Some(Claude));
        assert_eq!(handoff_target(Codex, |d| d != Local), Some(Claude));
        // Skips an agent that is off or not installed, and wraps.
        assert_eq!(handoff_target(Claude, |d| d == Codex), Some(Codex));
        assert_eq!(handoff_target(Codex, |d| d == Agy), Some(Agy));
        // Never itself, and nothing when it is the only one.
        assert_eq!(handoff_target(Claude, |d| d == Claude), None);
        assert_eq!(handoff_target(Agy, |_| false), None);
    }

    /// A thread that forbids unasked edits is not handed to an agy that would edit unasked,
    /// unless nothing else is usable, and then it says so.
    #[test]
    fn an_automatic_handoff_skips_an_agy_that_would_edit_unasked() {
        use Driver::{Agy, Claude, Codex};
        let all = |_| true;
        // agy can ask (or the thread allows unasked edits): the plain cycle.
        assert_eq!(safe_handoff_target(Claude, all, false), Some((Agy, false)));
        // agy cannot ask: skipped for the next usable agent.
        assert_eq!(safe_handoff_target(Claude, all, true), Some((Codex, false)));
        // agy is the only other agent: offered, flagged.
        assert_eq!(
            safe_handoff_target(Claude, |d| matches!(d, Claude | Agy), true),
            Some((Agy, true))
        );
        // Nothing usable at all.
        assert_eq!(safe_handoff_target(Claude, |d| d == Claude, true), None);
        // Leaving agy is never affected.
        assert_eq!(safe_handoff_target(Agy, all, true), Some((Codex, false)));
        assert_eq!(continue_label(Codex, false), "Continue in Codex");
        // Plan (read-only to the user) forbids unasked edits as much as Ask; only Accept edits
        // allows them, and an unknown mode is treated as forbidding them.
        assert!(forbids_unasked_edits(Some(Mode::Ask)));
        assert!(forbids_unasked_edits(Some(Mode::Plan)));
        assert!(forbids_unasked_edits(None));
        assert!(!forbids_unasked_edits(Some(Mode::AcceptEdits)));
        assert_eq!(
            continue_label(Agy, true),
            format!("Continue in {} (edits without asking)", Agy.info().label)
        );
    }

    #[test]
    fn a_limit_wait_says_when_it_continues() {
        use Driver::Claude;
        let label = Claude.info().label;
        assert_eq!(
            limit_wait_text(Claude, Some("16:40")),
            format!("Continues here after {label}'s limit resets at 16:40")
        );
        assert_eq!(
            limit_wait_text(Claude, None),
            format!("Continues here once {label} has usage again")
        );
    }
}

#[cfg(test)]
mod message_search_tests {
    use super::*;
    use agent_kit::store::{SearchHit, ThreadSummary, SNIPPET_CLOSE, SNIPPET_OPEN};

    fn marked(s: &str) -> String {
        s.replace('[', &SNIPPET_OPEN.to_string())
            .replace(']', &SNIPPET_CLOSE.to_string())
    }

    #[test]
    fn a_snippet_is_escaped_text_with_only_its_matches_emphasised() {
        let snippet = marked("run <b>&amp; [cargo] \"x\" 'y' > [test]");
        assert_eq!(
            snippet_markup(&snippet),
            "run &lt;b&gt;&amp;amp; <b>cargo</b> &quot;x&quot; &#39;y&#39; &gt; <b>test</b>"
        );
        assert_eq!(
            snippet_plain(&snippet),
            "run <b>&amp; cargo \"x\" 'y' > test"
        );
    }

    #[test]
    fn markup_that_a_thread_wrote_never_becomes_markup() {
        let snippet = marked("<span foreground=\"red\">[hi]</span>\n\u{1}<i>");
        let markup = snippet_markup(&snippet);
        assert!(!markup.contains("<span") && !markup.contains("<i>"));
        assert!(!markup.contains('\n') && !markup.contains('\u{1}'));
        assert_eq!(markup.matches("<b>").count(), 1);
        assert_eq!(markup.matches("</b>").count(), 1);
    }

    #[test]
    fn unbalanced_markers_still_make_balanced_markup() {
        assert_eq!(snippet_markup(&marked("a] [b")), "a <b>b</b>");
        assert_eq!(snippet_markup(&marked("[[a]]")), "<b>a</b>");
        assert_eq!(snippet_markup(""), "");
    }

    #[test]
    fn the_status_line_never_calls_an_unreadable_index_no_match() {
        let unavailable = message_status(MessageSearch::Unavailable, 0).expect("said");
        assert!(unavailable.starts_with("Search is unavailable"));
        assert_eq!(
            message_status(MessageSearch::Unavailable, 4).as_deref(),
            Some(unavailable.as_str())
        );
        assert_eq!(
            message_status(MessageSearch::Found(0), 0).as_deref(),
            Some("No messages match.")
        );
        assert_eq!(
            message_status(MessageSearch::Found(0), 1).as_deref(),
            Some("No messages match yet. Indexing 1 thread…")
        );
        assert_eq!(
            message_status(MessageSearch::Found(3), 12).as_deref(),
            Some("Indexing 12 threads… These results may be partial.")
        );
        assert_eq!(message_status(MessageSearch::Found(3), 0), None);
        assert_eq!(
            message_status(MessageSearch::Loading, 2).as_deref(),
            Some("Loading threads…")
        );
    }

    fn summary(id: &str, title: &str) -> ThreadSummary {
        ThreadSummary {
            id: id.into(),
            title: title.into(),
            cwd: "/w".into(),
            updated_at: 0,
            driver: None,
            model: None,
            unread: false,
            archived: false,
            pin_rank: None,
            tags: Vec::new(),
        }
    }

    fn hit(thread: &str, item: &str, archived: bool) -> SearchHit {
        SearchHit {
            thread_id: thread.into(),
            item_id: item.into(),
            first_seq: 1,
            role: "assistant".into(),
            at: 0,
            snippet: marked("the [deploy] step"),
            rank: -1.0,
            archived,
        }
    }

    #[test]
    fn hit_rows_keep_the_rank_order_and_drop_threads_that_are_gone() {
        let threads = [summary("t1", "Deploy notes"), summary("t2", "")];
        let rows = message_hit_rows(
            vec![
                hit("t2", "b", true),
                hit("gone", "x", false),
                hit("t1", "a", false),
            ],
            &threads,
        );
        let keys: Vec<_> = rows
            .iter()
            .map(|r| (r.thread.as_str(), r.item.as_str()))
            .collect();
        assert_eq!(keys, [("t2", "b"), ("t1", "a")]);
        assert_eq!(rows[0].title, "New thread");
        assert!(rows[0].archived);
        assert_eq!(rows[1].folder, "/w");
        assert_eq!(
            rows[0].accessible_label(30_000),
            "Agent in New thread, archived, just now: the deploy step"
        );
        assert_eq!(
            rows[1].accessible_label(3_600_000),
            "Agent in Deploy notes, 1h ago: the deploy step"
        );
    }

    #[test]
    fn every_role_has_a_name() {
        assert_eq!(hit_role_label("user"), "You");
        assert_eq!(hit_role_label("assistant"), "Agent");
        assert_eq!(hit_role_label("tool"), "Tool");
        assert_eq!(hit_role_label("other"), "Message");
    }
}
