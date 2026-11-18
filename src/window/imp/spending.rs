//! Spending…: what the agents' completed turns cost, from the store, by day, week or month and
//! grouped by folder, agent, profile or model.
//!
//! Everything is read off the main thread: the spending index is made through the store writer
//! on first use ([`agent_kit::store::Store::ensure_spend_index`]), then the turns are read and
//! every period's and grouping's totals are summed in one `store_job`. The dialog only shows
//! them. Kinds of spend are never added into one number, and a turn that reported no cost is
//! "not reported", never $0. Deleted threads still count (their turns are kept in the ledger).

use std::cell::Cell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use agent_core::adapter::Driver;
use agent_core::event::QuotaWindow;
use agent_core::spend::{self, Amount, Group, Period, Report, Total};
use agent_kit::store::{local_day, Store};
use gtk4::glib;

use super::threads::{store_job, write_job};
use super::AgentTerminalWindow;
use crate::account_status::AccountStatus;
use crate::config::TerminalConfig;

const DAY_MS: i64 = 86_400_000;
/// How far back the dialog reads: every period starts within this (twelve months at most 366
/// days before today, plus the month's own start).
const READ_BACK_DAYS: i64 = 400;

/// Every period's and grouping's report, read in one job.
pub(super) struct Loaded {
    /// `Period::ALL` × `Group::ALL`, in that order ([`Loaded::report`]).
    reports: Vec<Report>,
    /// Real money spent today and this month (metered turns, micro-dollars), for the banner.
    real_today: i64,
    real_month: i64,
}

/// The banner's text when a warning amount is reached, worst first; `None` below 80 % of every
/// amount set. The amounts are micro-dollars of metered turns.
pub(super) fn budget_banner(
    budget: &spend::Budget,
    real_today: i64,
    real_month: i64,
) -> Option<String> {
    use spend::BudgetPeriod;
    let mut due: Vec<spend::BudgetWarning> = [
        (BudgetPeriod::Day, real_today),
        (BudgetPeriod::Month, real_month),
    ]
    .into_iter()
    .filter_map(|(period, spent)| spend::warning_due(period, spent, budget, None))
    .collect();
    due.sort_by_key(|w| std::cmp::Reverse(w.level));
    due.first().map(spend::BudgetWarning::text)
}

impl Loaded {
    fn report(&self, period: usize, group: usize) -> Option<&Report> {
        self.reports.get(period * Group::ALL.len() + group)
    }
}

/// Reads and sums the store's turns as of `now_ms` (epoch milliseconds). Blocking: run it in a
/// `store_job`.
pub(super) fn load(store: &Store, now_ms: i64) -> Result<Loaded, String> {
    let today = local_day(now_ms);
    let data = store
        .spend_rows(now_ms - READ_BACK_DAYS * DAY_MS, &local_day)
        .map_err(|e| e.to_string())?;
    let mut reports = Vec::with_capacity(Period::ALL.len() * Group::ALL.len());
    for period in Period::ALL {
        for group in Group::ALL {
            reports.push(spend::report(&data.rows, period, group, today));
        }
    }
    Ok(Loaded {
        reports,
        real_today: spend::real_money(&data.rows, spend::BudgetPeriod::Day.first_day(today)),
        real_month: spend::real_money(&data.rows, spend::BudgetPeriod::Month.first_day(today)),
    })
}

/// What the dialog calls a group's raw key: a profile by its name, an agent by its own.
pub(super) struct Names {
    profiles: HashMap<String, String>,
    home: Option<PathBuf>,
}

impl Names {
    pub(super) fn new(config: &TerminalConfig, home: Option<PathBuf>) -> Self {
        Self {
            profiles: config
                .profiles
                .iter()
                .map(|p| (p.id.clone(), p.name.clone()))
                .collect(),
            home,
        }
    }

    pub(super) fn label(&self, group: Group, key: &str) -> String {
        match group {
            Group::Folder if key.is_empty() => "No folder".to_owned(),
            Group::Folder => match &self.home {
                Some(home) => agent_kit::paths::tilde(std::path::Path::new(key), home),
                None => key.to_owned(),
            },
            Group::Agent => Driver::from_key(key)
                .map(|d| d.info().label.to_owned())
                .unwrap_or_else(|| key.to_owned()),
            Group::Profile if key.is_empty() => "The agent's own profile".to_owned(),
            Group::Profile => self
                .profiles
                .get(key)
                .cloned()
                .unwrap_or_else(|| "A removed profile".to_owned()),
            Group::Model if key.is_empty() || key == "default" => {
                "The agent's default model".to_owned()
            }
            Group::Model => key.to_owned(),
        }
    }
}

fn turns(n: u64) -> String {
    if n == 1 {
        "1 turn".to_owned()
    } else {
        format!("{n} turns")
    }
}

/// A total's amount as shown: each kind of spend apart, "Not reported" (never $0) for turns
/// that reported no cost, "Free" for free profiles.
pub(super) fn amount_text(total: &Total) -> String {
    match total.amount() {
        Amount::Nothing => "Nothing".to_owned(),
        Amount::Free => "Free".to_owned(),
        Amount::NotReported => "Not reported".to_owned(),
        Amount::Cost(costs) => {
            let not_reported = total.not_reported_turns();
            if not_reported > 0 {
                format!(
                    "{} · {} not reported",
                    costs.describe(),
                    turns(not_reported)
                )
            } else {
                costs.describe()
            }
        }
    }
}

/// "12.3k tokens · 4 turns".
pub(super) fn tokens_text(total: &Total) -> String {
    format!(
        "{} tokens · {}",
        spend::format_tokens(total.token_count()),
        turns(total.turns)
    )
}

/// "Claude: 5-hour 12% · Weekly 30%"; `None` without windows.
pub(super) fn plan_usage_line(agent: &str, windows: &[QuotaWindow]) -> Option<String> {
    if windows.is_empty() {
        return None;
    }
    let parts: Vec<String> = windows
        .iter()
        .map(|w| {
            let name = match &w.group {
                Some(group) => format!("{group} {}", w.label),
                None => w.label.clone(),
            };
            format!("{name} {:.0}%", (w.used.clamp(0.0, 1.0) * 100.0))
        })
        .collect();
    Some(format!("{agent}: {}", parts.join(" · ")))
}

/// The notes under the totals: sub-agents' share, withheld costs, and what the numbers are.
pub(super) fn notes(report: &Report) -> Vec<String> {
    let mut out = Vec::new();
    let delegated = report.total.delegated_total();
    if delegated.turns > 0 {
        out.push(format!(
            "From sub-agents, counted under the thread that started them: {} ({}).",
            amount_text(&delegated),
            turns(delegated.turns)
        ));
    }
    let withheld = report.total.withheld_turns;
    if withheld > 0 {
        out.push(format!(
            "{} stored before each turn's own cost was recorded {} counted for tokens only \
             (deleted threads' such turns are not counted here).",
            turns(withheld),
            if withheld == 1 { "is" } else { "are" }
        ));
    }
    out.push(
        "Costs are what the agents reported. On a plan, a reported cost is the list price, not a \
         charge. Codex, Antigravity and the Local agent report tokens only. Threads you deleted \
         still count."
            .to_owned(),
    );
    out
}

fn label(text: &str, classes: &[&str]) -> gtk4::Label {
    gtk4::Label::builder()
        .label(text)
        .xalign(0.0)
        .wrap(true)
        .css_classes(classes.iter().map(|c| c.to_string()).collect::<Vec<_>>())
        .build()
}

/// One row: its name and amount, a bar, and its tokens.
fn total_row(name: &str, total: &Total, fraction: f64) -> gtk4::ListBoxRow {
    let body = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
    body.set_margin_top(8);
    body.set_margin_bottom(8);
    body.set_margin_start(12);
    body.set_margin_end(12);
    let top = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    let name_label = gtk4::Label::builder()
        .label(name)
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk4::pango::EllipsizeMode::Middle)
        .tooltip_text(name)
        .css_classes(["spending-name"])
        .build();
    let amount = gtk4::Label::builder()
        .label(amount_text(total))
        .xalign(1.0)
        .wrap(true)
        .css_classes(["numeric", "spending-amount"])
        .build();
    top.append(&name_label);
    top.append(&amount);
    body.append(&top);
    let bar = gtk4::LevelBar::builder()
        .min_value(0.0)
        .max_value(1.0)
        .value(fraction.clamp(0.0, 1.0))
        .build();
    bar.update_property(&[gtk4::accessible::Property::Label(&format!(
        "Share of the largest: {:.0}%",
        fraction.clamp(0.0, 1.0) * 100.0
    ))]);
    body.append(&bar);
    body.append(&label(&tokens_text(total), &["dim-label", "caption"]));
    let row = gtk4::ListBoxRow::builder()
        .child(&body)
        .activatable(false)
        .build();
    row.add_css_class("spending-row");
    row
}

fn list(rows: impl IntoIterator<Item = gtk4::ListBoxRow>) -> gtk4::ListBox {
    let list = gtk4::ListBox::new();
    list.set_selection_mode(gtk4::SelectionMode::None);
    list.add_css_class("boxed-list");
    for row in rows {
        list.append(&row);
    }
    list
}

/// Fills `content` with `report`: the total, the groups, the buckets and the notes.
fn show_report(content: &gtk4::Box, report: &Report, names: &Names, plan_lines: &[String]) {
    while let Some(child) = content.first_child() {
        content.remove(&child);
    }
    if report.is_empty() {
        let empty = adw::StatusPage::builder()
            .title("No spending recorded yet")
            .description(format!(
                "{}: no completed turns. A turn's cost or tokens show here once it ends.",
                report.period.span_label()
            ))
            .css_classes(["compact"])
            .build();
        content.append(&empty);
    } else {
        content.append(&label(report.period.span_label(), &["heading"]));
        let total = label(&amount_text(&report.total), &["title-2", "spending-total"]);
        content.append(&total);
        content.append(&label(&tokens_text(&report.total), &["dim-label"]));

        content.append(&label(
            &format!("By {}", report.group.label().to_lowercase()),
            &["heading"],
        ));
        let fractions = spend::bar_fractions(report.groups.iter().map(|(_, t)| t));
        content.append(&list(
            report
                .groups
                .iter()
                .zip(fractions)
                .map(|((key, t), f)| total_row(&names.label(report.group, key), t, f)),
        ));

        content.append(&label(
            match report.period {
                Period::Day => "By day",
                Period::Week => "By week",
                Period::Month => "By month",
            },
            &["heading"],
        ));
        let fractions = spend::bar_fractions(report.buckets.iter().map(|(_, t)| t));
        content.append(&list(report.buckets.iter().zip(fractions).map(
            |((start, t), f)| total_row(&report.period.bucket_label(*start), t, f),
        )));
    }
    if !plan_lines.is_empty() {
        content.append(&label("Plan usage", &["heading"]));
        for line in plan_lines {
            content.append(&label(line, &["dim-label"]));
        }
    }
    for note in notes(report) {
        content.append(&label(&note, &["dim-label", "caption"]));
    }
}

/// The live plan windows of every agent that has some, one line each, each followed by its
/// extra usage while it is switched on.
fn plan_lines(status: &AccountStatus) -> Vec<String> {
    Driver::ALL
        .into_iter()
        .flat_map(|d| {
            let snap = status.snapshot(d);
            let agent = d.info().label;
            plan_usage_line(agent, &snap.windows).into_iter().chain(
                snap.extra
                    .as_ref()
                    .and_then(spend::extra_usage_text)
                    .map(|text| format!("{agent}: {text}")),
            )
        })
        .collect()
}

fn now_ms() -> i64 {
    glib::real_time() / 1000
}

/// Whether `event` moved the real money spent: a metered turn that reported a cost. Only this
/// starts a budget check (extra usage never counts: its unit is not confirmed).
pub(in crate::window) fn moves_real_money(event: &agent_core::event::Event) -> bool {
    matches!(
        event,
        agent_core::event::Event::TurnCompleted {
            billing: Some(spend::Billing::Metered),
            cost_usd: Some(cost),
            ..
        } if *cost > 0.0
    )
}

/// An open main window other than `except`, to show what a closed one cannot.
fn another_window(
    except: Option<&crate::window::AgentTerminalWindow>,
) -> Option<crate::window::AgentTerminalWindow> {
    let app = gtk4::gio::Application::default()?
        .downcast::<gtk4::Application>()
        .ok()?;
    app.windows()
        .into_iter()
        .filter_map(|w| w.downcast::<crate::window::AgentTerminalWindow>().ok())
        .find(|w| Some(w) != except)
}

/// [`AgentTerminalWindow::check_spending_budget`] from outside a window (a delegated worker's
/// committed turn): on `preferred` while it is open, else on any open window.
pub(in crate::window) fn check_spending_budget_on(
    preferred: Option<crate::window::AgentTerminalWindow>,
) {
    if let Some(window) = preferred.or_else(|| another_window(None)) {
        window.imp().check_spending_budget();
    }
}

/// How a warning amount is shown in its entry: "25", "12.5"; blank for none.
pub(super) fn budget_text(amount: Option<f64>) -> String {
    amount.map(|n| n.to_string()).unwrap_or_default()
}

thread_local! {
    /// A budget check is running; a second one asked for meanwhile runs once it ends.
    static BUDGET_CHECK: Cell<(bool, bool)> = const { Cell::new((false, false)) };
    /// Warnings raised while no window was open, held until a window can toast them.
    static PENDING_WARNINGS: std::cell::RefCell<Vec<spend::BudgetWarning>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn send_spending_notification(warning: &spend::BudgetWarning) {
    let Some(app) = gtk4::gio::Application::default() else {
        return;
    };
    let notification = gtk4::gio::Notification::new("Spending warning");
    notification.set_body(Some(&warning.text()));
    notification.set_priority(gtk4::gio::NotificationPriority::High);
    notification.set_default_action("app.new-window");
    let id = format!("spending-warning-{:?}-{:?}", warning.period, warning.level);
    app.send_notification(Some(&id), &notification);
}

/// Shows `warnings` as toasts on `shown`, or queues them and notifies the desktop when no
/// window is open so a recorded warning is never dropped silently.
fn deliver_spending_warnings(
    shown: Option<&crate::window::AgentTerminalWindow>,
    warnings: Vec<spend::BudgetWarning>,
) {
    if warnings.is_empty() {
        return;
    }
    if let Some(window) = shown {
        for warning in warnings {
            window.imp().show_toast(&warning.text());
        }
        return;
    }
    for warning in &warnings {
        send_spending_notification(warning);
    }
    PENDING_WARNINGS.with(|p| p.borrow_mut().extend(warnings));
}

const BUDGET_HELP_DAILY: &str =
    "Warn when today's real money spent reaches 80% and then all of this amount, in US dollars. \
     Blank: no daily warning";
const BUDGET_HELP_MONTHLY: &str =
    "Warn when this month's real money spent reaches 80% and then all of this amount, in US \
     dollars. Blank: no monthly warning";

/// Shows on a warning amount's row whether its entry holds an amount Settings can keep: the
/// reason in the subtitle and the entry marked invalid. Returns the amount when it can.
fn check_budget_entry(
    entry: &gtk4::Entry,
    row: &adw::ActionRow,
    help: &str,
) -> Option<Option<f64>> {
    match spend::parse_budget(&entry.text()) {
        Ok(amount) => {
            entry.remove_css_class("error");
            row.set_subtitle(help);
            entry.set_tooltip_text(None);
            entry.update_property(&[gtk4::accessible::Property::Description("")]);
            entry.update_state(&[gtk4::accessible::State::Invalid(
                gtk4::AccessibleInvalidState::False,
            )]);
            Some(amount)
        }
        Err(problem) => {
            let problem = format!("{problem} The previous amount is still in use.");
            entry.add_css_class("error");
            row.set_subtitle(&glib::markup_escape_text(&problem));
            entry.set_tooltip_text(Some(&problem));
            entry.update_property(&[gtk4::accessible::Property::Description(&problem)]);
            entry.update_state(&[gtk4::accessible::State::Invalid(
                gtk4::AccessibleInvalidState::True,
            )]);
            None
        }
    }
}

impl AgentTerminalWindow {
    /// Settings → General → Spending: the warning switch and its two amounts. Each applies as
    /// it is made (the switch at once, an amount when Enter is pressed or the entry loses
    /// focus); an amount that is not one is refused inline, never saved.
    pub(super) fn spending_group(&self) -> adw::PreferencesGroup {
        let group = adw::PreferencesGroup::builder()
            .title("Spending")
            .description(
                "A warning counts real money only: turns on an API key or a metered model \
                 server. A plan's list price and Claude's extra usage are never counted. It \
                 warns once a day or month at each level, and never stops a turn.",
            )
            .build();
        let switch = self.config_switch(
            "Warn About Spending",
            "Show a notice when spending nears or passes the amounts below",
            |c| c.spend_warn_enabled,
            super::SaveWhen::Debounced(|c, on| c.spend_warn_enabled = on),
            |imp, on| {
                if on {
                    imp.check_spending_budget();
                }
            },
        );
        group.add(&switch);
        group.add(&self.budget_row(
            "Daily Warning",
            BUDGET_HELP_DAILY,
            |c| c.spend_warn_daily,
            |c, v| c.spend_warn_daily = v,
        ));
        group.add(&self.budget_row(
            "Monthly Warning",
            BUDGET_HELP_MONTHLY,
            |c| c.spend_warn_monthly,
            |c, v| c.spend_warn_monthly = v,
        ));
        group
    }

    fn budget_row(
        &self,
        title: &str,
        help: &'static str,
        get: fn(&TerminalConfig) -> Option<f64>,
        set: fn(&mut TerminalConfig, Option<f64>),
    ) -> adw::ActionRow {
        let obj = self.obj();
        let entry = gtk4::Entry::builder()
            .text(budget_text(get(&self.config.borrow())))
            .valign(gtk4::Align::Center)
            .placeholder_text("No warning")
            .input_purpose(gtk4::InputPurpose::Number)
            .width_chars(10)
            .build();
        entry.update_property(&[gtk4::accessible::Property::Label(title)]);
        let row = adw::ActionRow::builder()
            .title(title)
            .subtitle(help)
            .build();
        row.add_css_class("spending-budget");
        row.add_suffix(&entry);
        entry.connect_changed(glib::clone!(
            #[weak]
            row,
            move |entry| {
                check_budget_entry(entry, &row, help);
            }
        ));
        let commit = Rc::new(glib::clone!(
            #[weak]
            obj,
            #[weak]
            row,
            #[weak]
            entry,
            move || {
                let Some(amount) = check_budget_entry(&entry, &row, help) else {
                    return;
                };
                let imp = obj.imp();
                if get(&imp.config.borrow()) == amount {
                    return;
                }
                set(&mut imp.config.borrow_mut(), amount);
                imp.schedule_config_save();
                imp.check_spending_budget();
            }
        ));
        entry.connect_activate(glib::clone!(
            #[strong]
            commit,
            move |_| commit()
        ));
        let focus = gtk4::EventControllerFocus::new();
        focus.connect_leave(move |_| commit());
        entry.add_controller(focus);
        row
    }

    /// Checks the spending warnings now, off the main thread, through the store writer (which
    /// records each warning given, so it is given once a period, across windows and restarts),
    /// and shows a toast for each one due. Asked again while one runs, it runs once more after.
    /// A window closed before the writer answered hands its toasts, and the check asked for
    /// meanwhile, to another open window: a warning recorded as given is never lost.
    pub(super) fn check_spending_budget(&self) {
        let budget = self.config.borrow().spend_budget();
        if budget == spend::Budget::default() {
            return;
        }
        let (running, _) = BUDGET_CHECK.with(Cell::get);
        if running {
            BUDGET_CHECK.with(|c| c.set((true, true)));
            return;
        }
        BUDGET_CHECK.with(|c| c.set((true, false)));
        let now = now_ms();
        let weak = self.obj().downgrade();
        glib::spawn_future_local(async move {
            let result = write_job("check the spending warnings", move |s| {
                s.ensure_spend_index()?;
                s.spend_budget_check(now, &local_day, &budget)
            })
            .await;
            // The window that asked while it is still open (still the application's), else
            // another open one.
            let asked = weak.upgrade();
            let shown = match asked {
                Some(w) if w.application().is_some() => Some(w),
                other => another_window(other.as_ref()).or(other),
            };
            match result {
                Some(Ok(warnings)) => deliver_spending_warnings(shown.as_ref(), warnings),
                Some(Err(e)) => tracing::warn!(error = %e, "could not check the spending warnings"),
                None => tracing::warn!("the spending warnings were not checked"),
            }
            let (_, again) = BUDGET_CHECK.with(Cell::get);
            BUDGET_CHECK.with(|c| c.set((false, false)));
            if again {
                check_spending_budget_on(shown);
            }
        });
    }

    /// Shows any spending warnings that arrived while no window was open.
    pub(super) fn flush_pending_spending_warnings(&self) {
        let pending = PENDING_WARNINGS.with(|p| std::mem::take(&mut *p.borrow_mut()));
        for warning in pending {
            self.show_toast(&warning.text());
        }
    }
}

impl AgentTerminalWindow {
    /// Spending…: opens the dialog at once (loading), then reads the store off the main thread.
    pub(super) fn show_spending(&self) -> adw::Dialog {
        let obj = self.obj();
        let dialog = adw::Dialog::builder()
            .title("Spending")
            .content_width(560)
            .content_height(680)
            .build();
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        let stack = gtk4::Stack::new();
        let spinner = gtk4::Spinner::builder()
            .spinning(true)
            .width_request(32)
            .height_request(32)
            .halign(gtk4::Align::Center)
            .valign(gtk4::Align::Center)
            .build();
        stack.add_named(&spinner, Some("loading"));
        toolbar.set_content(Some(&stack));
        dialog.set_child(Some(&toolbar));
        dialog.present(Some(&*obj));

        let names = Rc::new(Names::new(&self.config.borrow(), glib::home_dir().into()));
        let plan = Rc::new(plan_lines(&AccountStatus::shared()));
        let budget = self.config.borrow().spend_budget();
        let now = now_ms();
        glib::spawn_future_local(glib::clone!(
            #[weak]
            stack,
            async move {
                // Made once, through the writer; a failure leaves the read slower, never wrong.
                // The cost marker again too, in case the writer could not record it at start.
                let prepared = write_job("make the spending index", |s| {
                    s.ensure_spend_index()?;
                    if let Err(e) = s.ensure_spend_cost_marker() {
                        tracing::warn!(error = %e, "could not record where per-turn costs begin");
                    }
                    Ok(())
                });
                match prepared.await {
                    Some(Ok(())) => {}
                    Some(Err(e)) => tracing::warn!(error = %e, "no spending index"),
                    None => tracing::warn!("the spending index was not made"),
                }
                let loaded = match store_job(move |s| load(s, now)).await {
                    Some(Ok(loaded)) => loaded,
                    Some(Err(why)) => return show_error(&stack, &why),
                    None => return show_error(&stack, "The thread store could not be read."),
                };
                let banner = budget_banner(&budget, loaded.real_today, loaded.real_month);
                show_loaded(&stack, Rc::new(loaded), names, plan, banner);
            }
        ));
        dialog
    }
}

fn show_error(stack: &gtk4::Stack, why: &str) {
    let page = adw::StatusPage::builder()
        .title("Could not read spending")
        .description(glib::markup_escape_text(why).as_str())
        .build();
    crate::icons::set_status_icon(&page, "at-dialog-error-symbolic");
    stack.add_named(&page, Some("error"));
    stack.set_visible_child_name("error");
}

fn show_loaded(
    stack: &gtk4::Stack,
    loaded: Rc<Loaded>,
    names: Rc<Names>,
    plan: Rc<Vec<String>>,
    banner: Option<String>,
) {
    let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    if let Some(text) = banner {
        let banner = adw::Banner::builder()
            .title(glib::markup_escape_text(&text).as_str())
            .revealed(true)
            .build();
        banner.add_css_class("spending-banner");
        outer.append(&banner);
    }
    let page = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    page.set_margin_top(12);
    page.set_margin_bottom(18);
    page.set_margin_start(18);
    page.set_margin_end(18);

    let controls = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    let periods = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    periods.add_css_class("linked");
    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 10);
    let period = Rc::new(Cell::new(0usize));
    let group = Rc::new(Cell::new(0usize));
    let redraw: Rc<dyn Fn()> = {
        let (content, loaded, names, plan) = (content.clone(), loaded.clone(), names, plan);
        let (period, group) = (period.clone(), group.clone());
        Rc::new(move || {
            if let Some(report) = loaded.report(period.get(), group.get()) {
                show_report(&content, report, &names, &plan);
            }
        })
    };
    let mut first: Option<gtk4::ToggleButton> = None;
    for (i, p) in Period::ALL.into_iter().enumerate() {
        let button = gtk4::ToggleButton::builder()
            .label(p.label())
            .tooltip_text(p.span_label())
            .active(i == 0)
            .build();
        if let Some(first) = &first {
            button.set_group(Some(first));
        } else {
            first = Some(button.clone());
        }
        let (period, redraw) = (period.clone(), redraw.clone());
        button.connect_toggled(move |b| {
            if b.is_active() && period.get() != i {
                period.set(i);
                redraw();
            }
        });
        periods.append(&button);
    }
    controls.append(&periods);
    let labels: Vec<&str> = Group::ALL.iter().map(|g| g.label()).collect();
    let by = gtk4::DropDown::from_strings(&labels);
    by.set_tooltip_text(Some("Group by"));
    by.update_property(&[gtk4::accessible::Property::Label("Group by")]);
    by.set_halign(gtk4::Align::End);
    by.set_hexpand(true);
    {
        let (group, redraw) = (group.clone(), redraw.clone());
        by.connect_selected_notify(move |d| {
            let i = d.selected() as usize;
            if i < Group::ALL.len() && group.get() != i {
                group.set(i);
                redraw();
            }
        });
    }
    controls.append(&by);
    page.append(&controls);
    page.append(&content);
    redraw();

    let scrolled = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vexpand(true)
        .child(&page)
        .build();
    outer.append(&scrolled);
    stack.add_named(&outer, Some("report"));
    stack.set_visible_child_name("report");
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use agent_core::spend::{Billing, DayKey, SpendRow};

    fn row(driver: &str, cost: Option<f64>, delegated: bool) -> SpendRow {
        SpendRow {
            day: DayKey::parse("2026-11-15").expect("day"),
            driver: driver.into(),
            profile_id: String::new(),
            model: "default".into(),
            folder: "/a".into(),
            billing: Billing::Unknown,
            delegated,
            cost_usd: cost.unwrap_or(0.0),
            cost_turns: u64::from(cost.is_some()),
            turns: 1,
            withheld_turns: 0,
            tokens: agent_core::event::Usage {
                input_tokens: 600,
                output_tokens: 600,
                ..Default::default()
            },
        }
    }

    fn report(rows: &[SpendRow]) -> Report {
        spend::report(
            rows,
            Period::Day,
            Group::Agent,
            DayKey::parse("2026-11-15").expect("day"),
        )
    }

    #[test]
    fn a_total_never_reads_as_zero_dollars_when_nothing_was_reported() {
        let codex = report(&[row("codex", None, false)]);
        assert_eq!(amount_text(&codex.total), "Not reported");
        assert_eq!(tokens_text(&codex.total), "1.2k tokens · 1 turn");
        let mixed = report(&[row("claude", Some(1.5), false), row("codex", None, false)]);
        assert_eq!(
            amount_text(&mixed.total),
            "$1.50 reported · 1 turn not reported"
        );
        assert_eq!(amount_text(&Total::default()), "Nothing");
    }

    #[test]
    fn notes_name_the_sub_agents_share_and_withheld_turns() {
        let mut withheld = row("claude", None, false);
        withheld.withheld_turns = 2;
        withheld.turns = 2;
        let r = report(&[
            row("claude", Some(1.0), false),
            row("claude", Some(0.25), true),
            withheld,
        ]);
        let n = notes(&r);
        assert!(n[0].contains("$0.25 reported (1 turn)"), "{n:?}");
        assert!(n[1].starts_with("2 turns stored before"), "{n:?}");
        assert!(n[1].contains("deleted threads"), "{n:?}");
        assert!(n.last().is_some_and(|l| l.contains("list price")));
        assert_eq!(notes(&report(&[])).len(), 1);

        // Sub-agents that report tokens only are not reported, never $0.00.
        let r = report(&[row("codex", None, true), row("agy", None, true)]);
        let n = notes(&r);
        assert!(n[0].contains(": Not reported (2 turns)"), "{n:?}");
        let r = report(&[row("codex", None, true), row("claude", Some(0.5), true)]);
        let n = notes(&r);
        assert!(
            n[0].contains("$0.50 reported · 1 turn not reported (2 turns)"),
            "{n:?}"
        );
    }

    #[test]
    fn plan_usage_names_each_window() {
        let windows = [
            QuotaWindow {
                group: None,
                label: "5-hour".into(),
                used: 0.123,
                resets_at: None,
            },
            QuotaWindow {
                group: Some("Gemini Models".into()),
                label: "Weekly".into(),
                used: 1.4,
                resets_at: None,
            },
        ];
        assert_eq!(
            plan_usage_line("Claude", &windows).as_deref(),
            Some("Claude: 5-hour 12% · Gemini Models Weekly 100%")
        );
        assert_eq!(plan_usage_line("Codex", &[]), None);
    }

    #[test]
    fn group_keys_are_named_for_reading() {
        let names = Names {
            profiles: HashMap::from([("p1".to_owned(), "Work".to_owned())]),
            home: Some(PathBuf::from("/home/u")),
        };
        assert_eq!(names.label(Group::Folder, "/home/u/src"), "~/src");
        assert_eq!(names.label(Group::Folder, ""), "No folder");
        assert_eq!(names.label(Group::Agent, "agy"), "Antigravity");
        assert_eq!(names.label(Group::Agent, "mystery"), "mystery");
        assert_eq!(names.label(Group::Profile, "p1"), "Work");
        assert_eq!(names.label(Group::Profile, ""), "The agent's own profile");
        assert_eq!(names.label(Group::Profile, "gone"), "A removed profile");
        assert_eq!(
            names.label(Group::Model, "default"),
            "The agent's default model"
        );
        assert_eq!(names.label(Group::Model, "opus"), "opus");
    }

    #[test]
    fn only_real_money_starts_a_budget_check() {
        use agent_core::event::{Event, ExtraUsage, TurnState};
        let turn = |billing, cost| Event::TurnCompleted {
            state: TurnState::Completed,
            usage: None,
            cost_usd: cost,
            error: None,
            model: None,
            billing,
        };
        assert!(moves_real_money(&turn(Some(Billing::Metered), Some(0.2))));
        assert!(!moves_real_money(&turn(Some(Billing::Metered), Some(0.0))));
        assert!(!moves_real_money(&turn(Some(Billing::Metered), None)));
        assert!(!moves_real_money(&turn(Some(Billing::Plan), Some(5.0))));
        assert!(!moves_real_money(&turn(None, Some(5.0))));
        let extra = |used| Event::ExtraUsageUpdated {
            extra: ExtraUsage {
                enabled: true,
                used,
                limit: None,
                currency: Some("USD".into()),
            },
        };
        assert!(
            !moves_real_money(&extra(1.0)),
            "extra usage never counts: its unit is not confirmed"
        );
    }

    #[test]
    fn the_banner_names_the_worst_warning_reached() {
        let usd = spend::to_micros;
        let budget = spend::Budget {
            daily: Some(10.0),
            monthly: Some(100.0),
        };
        assert_eq!(budget_banner(&budget, usd(1.0), usd(10.0)), None);
        assert_eq!(
            budget_banner(&budget, usd(8.0), usd(10.0)).as_deref(),
            Some("Spending today is $8.00, near your $10.00 warning")
        );
        // A month past its amount outranks a day only near.
        assert_eq!(
            budget_banner(&budget, usd(8.0), usd(105.0)).as_deref(),
            Some("Spending this month is $105.00, past your $100.00 warning")
        );
        assert_eq!(
            budget_banner(&spend::Budget::default(), usd(1e6), usd(1e6)),
            None
        );
        assert_eq!(budget_text(Some(12.5)), "12.5");
        assert_eq!(budget_text(None), "");
    }

    /// The Spending group saves only amounts it can keep: typing saves nothing, a refused
    /// amount is shown as wrong and keeps the previous one, blank clears it. Run from the
    /// window test.
    pub(in crate::window) fn the_spending_group_saves_only_valid_amounts(
        window: &crate::window::AgentTerminalWindow,
    ) {
        let imp = window.imp();
        let before = {
            let c = imp.config.borrow();
            (
                c.spend_warn_enabled,
                c.spend_warn_daily,
                c.spend_warn_monthly,
            )
        };
        {
            let mut c = imp.config.borrow_mut();
            c.spend_warn_enabled = false;
            c.spend_warn_daily = None;
            c.spend_warn_monthly = Some(50.0);
        }
        let group = imp.spending_group();
        let ctx = glib::MainContext::default();
        let entry_of = |title: &str| -> gtk4::Entry {
            crate::testutil::descendants(group.upcast_ref())
                .into_iter()
                .filter_map(|w| w.downcast::<adw::ActionRow>().ok())
                .find(|r| r.title() == title)
                .and_then(|r| crate::testutil::find_first::<gtk4::Entry>(r.upcast_ref()))
                .unwrap_or_else(|| panic!("no {title} entry"))
        };
        let (daily, monthly) = (&entry_of("Daily Warning"), &entry_of("Monthly Warning"));
        assert_eq!(monthly.text(), "50");
        let saved = || {
            let c = imp.config.borrow();
            (c.spend_warn_daily, c.spend_warn_monthly)
        };
        let commit = |entry: &gtk4::Entry, text: &str| {
            entry.set_text(text);
            entry.emit_by_name::<()>("activate", &[]);
            crate::testutil::settle(&ctx);
        };

        daily.set_text("12");
        crate::testutil::settle(&ctx);
        assert_eq!(saved(), (None, Some(50.0)), "typing saves nothing");
        for bad in ["-5", "0", "twelve", "nan"] {
            commit(daily, bad);
            assert_eq!(saved(), (None, Some(50.0)), "{bad:?} was saved");
            assert!(
                daily.has_css_class("error"),
                "{bad:?} is not shown as wrong"
            );
        }
        commit(daily, "$12.50");
        assert_eq!(saved(), (Some(12.5), Some(50.0)));
        assert!(!daily.has_css_class("error"));
        commit(monthly, "abc");
        assert_eq!(
            saved().1,
            Some(50.0),
            "a refused amount keeps the saved one"
        );
        commit(monthly, "");
        assert_eq!(saved(), (Some(12.5), None), "blank is no warning");
        // Leaving the field commits it as Enter does.
        monthly.set_text("75");
        crate::testutil::settle(&ctx);
        assert_eq!(saved().1, None, "typing saves nothing");
        let focus = monthly
            .observe_controllers()
            .into_iter()
            .filter_map(|c| c.ok()?.downcast::<gtk4::EventControllerFocus>().ok())
            .last()
            .expect("the entry's focus controller");
        focus.emit_by_name::<()>("leave", &[]);
        crate::testutil::settle(&ctx);
        assert_eq!(
            saved(),
            (Some(12.5), Some(75.0)),
            "leaving the field saved it"
        );

        let switch =
            crate::testutil::find_first::<adw::SwitchRow>(group.upcast_ref()).expect("the switch");
        assert!(!switch.is_active());
        switch.set_active(true);
        crate::testutil::settle(&ctx);
        assert!(imp.config.borrow().spend_warn_enabled);

        let mut c = imp.config.borrow_mut();
        (
            c.spend_warn_enabled,
            c.spend_warn_daily,
            c.spend_warn_monthly,
        ) = before;
    }

    /// A metered turn's cost, checked through the store writer, shows its warning as a toast
    /// once: the same check again shows nothing, and a turn on a plan never warns. Run from the
    /// window test.
    pub(in crate::window) fn a_metered_turn_warns_once_through_the_window(
        window: &crate::window::AgentTerminalWindow,
    ) {
        use agent_core::event::{Envelope, Event, TurnState};
        let imp = window.imp();
        let store = super::super::threads::app_store();
        let ctx = glib::MainContext::default();
        let before = {
            let c = imp.config.borrow();
            (
                c.spend_warn_enabled,
                c.spend_warn_daily,
                c.spend_warn_monthly,
            )
        };
        let thread = store.create_thread("/spend/budget", None).expect("thread");
        let pt = store
            .add_provider_thread(&thread, "claude", "opus")
            .expect("pt");
        let turn = |cost: f64, billing| {
            let env = Envelope::new(Event::TurnCompleted {
                state: TurnState::Completed,
                usage: None,
                cost_usd: Some(cost),
                error: None,
                model: None,
                billing: Some(billing),
            });
            store.append_event(&thread, Some(&pt), &env).expect("turn");
        };
        // Whatever today already spent, the amount is set so this turn reaches 90% of it.
        let spent_before = load(&store, now_ms()).map(|l| l.real_today).unwrap_or(0);
        turn(9.0, Billing::Metered);
        turn(500.0, Billing::Plan);
        let amount = spend::from_micros(spent_before + spend::to_micros(9.0)) / 0.9;
        {
            let mut c = imp.config.borrow_mut();
            c.spend_warn_enabled = true;
            c.spend_warn_daily = Some(amount);
            c.spend_warn_monthly = None;
        }
        let warned = || {
            imp.test_toasts
                .borrow()
                .iter()
                .filter_map(|t| t.title())
                .filter(|t| t.contains("today") && t.contains("near your"))
                .count()
        };
        let shown = warned();
        imp.check_spending_budget();
        assert!(
            crate::testutil::pump_until(&ctx, 20, || warned() == shown + 1),
            "the warning was not shown"
        );
        imp.check_spending_budget();
        crate::testutil::pump_until(&ctx, 2, || BUDGET_CHECK.with(Cell::get) == (false, false));
        crate::testutil::settle(&ctx);
        assert_eq!(warned(), shown + 1, "a level is warned of once a period");

        // A warning raised while no window is open is kept and toasted when a window opens.
        let deferred = spend::BudgetWarning {
            period: spend::BudgetPeriod::Day,
            level: spend::BudgetLevel::Over,
            spent: spend::to_micros(10.0),
            limit: spend::to_micros(10.0),
        };
        let deferred_text = deferred.text();
        deliver_spending_warnings(None, vec![deferred]);
        imp.flush_pending_spending_warnings();
        assert!(
            imp.test_toasts
                .borrow()
                .iter()
                .any(|t| t.title().as_deref() == Some(deferred_text.as_str())),
            "a warning raised with no window open is shown when a window opens"
        );

        store.delete_thread(&thread).expect("delete");
        let mut c = imp.config.borrow_mut();
        (
            c.spend_warn_enabled,
            c.spend_warn_daily,
            c.spend_warn_monthly,
        ) = before;
    }

    /// The dialog, driven as a user drives it, on the window's store: the totals of a priced
    /// and an unpriced agent, regrouping, another period, and a deleted thread still counted.
    /// Run from the window test.
    pub(in crate::window) fn the_spending_dialog_shows_and_keeps_what_turns_cost(
        window: &crate::window::AgentTerminalWindow,
    ) {
        use agent_core::event::{Envelope, Event, TurnState, Usage};
        let imp = window.imp();
        let store = super::super::threads::app_store();
        let ctx = glib::MainContext::default();
        let turn = |thread: &str, pt: &str, cost: Option<f64>| {
            let env = Envelope::new(Event::TurnCompleted {
                state: TurnState::Completed,
                usage: Some(Usage {
                    input_tokens: 500,
                    output_tokens: 500,
                    ..Usage::default()
                }),
                cost_usd: cost,
                error: None,
                model: None,
                billing: None,
            });
            store.append_event(thread, Some(pt), &env).expect("turn");
        };
        let priced = store.create_thread("/spend/a", None).expect("thread");
        let ppt = store
            .add_provider_thread(&priced, "claude", "opus")
            .expect("pt");
        turn(&priced, &ppt, Some(1.25));
        let unpriced = store.create_thread("/spend/b", None).expect("thread");
        let upt = store
            .add_provider_thread(&unpriced, "codex", "gpt")
            .expect("pt");
        turn(&unpriced, &upt, None);

        let open = || {
            let dialog = imp.show_spending();
            let shown = crate::testutil::pump_until(&ctx, 20, || {
                crate::testutil::find_first::<gtk4::ScrolledWindow>(dialog.upcast_ref())
                    .is_some_and(|s| s.is_mapped())
            });
            assert!(shown, "the report is shown");
            dialog
        };
        // A row's name and amount, by its name: other tests' threads may share the store.
        let amount = |dialog: &adw::Dialog, name: &str| -> Option<String> {
            let of = |row: &gtk4::ListBoxRow, class: &str| {
                crate::testutil::find_by_class::<gtk4::Label>(row.upcast_ref(), class)
                    .map(|l| l.text().to_string())
            };
            crate::testutil::find_all_by_class::<gtk4::ListBoxRow>(
                dialog.upcast_ref(),
                "spending-row",
            )
            .iter()
            .find(|r| of(r, "spending-name").as_deref() == Some(name))
            .and_then(|r| of(r, "spending-amount"))
        };

        let dialog = open();
        assert_eq!(
            amount(&dialog, "/spend/a").as_deref(),
            Some("$1.25 reported")
        );
        assert_eq!(amount(&dialog, "/spend/b").as_deref(), Some("Not reported"));
        let problems = crate::testutil::button_problems(dialog.upcast_ref(), 24);
        assert!(problems.is_empty(), "{problems:#?}");

        let by = crate::testutil::find_first::<gtk4::DropDown>(dialog.upcast_ref()).expect("by");
        by.set_selected(1);
        assert!(
            crate::testutil::pump_until(&ctx, 10, || amount(&dialog, "Codex").is_some()),
            "regrouped by agent"
        );
        assert!(amount(&dialog, "Claude").is_some_and(|a| a.contains('$')));
        let month = crate::testutil::find_all::<gtk4::ToggleButton>(dialog.upcast_ref())
            .into_iter()
            .find(|b| b.label().as_deref() == Some("Month"))
            .expect("month");
        month.set_active(true);
        assert!(
            crate::testutil::pump_until(&ctx, 10, || {
                crate::testutil::label_texts(dialog.upcast_ref())
                    .iter()
                    .any(|t| t == "Last 12 months")
            }),
            "the month view is shown"
        );
        dialog.force_close();

        store.delete_thread(&priced).expect("delete");
        let dialog = open();
        assert_eq!(
            amount(&dialog, "/spend/a").as_deref(),
            Some("$1.25 reported"),
            "a deleted thread still counts"
        );
        dialog.force_close();
        store.delete_thread(&unpriced).expect("delete");
    }
}
