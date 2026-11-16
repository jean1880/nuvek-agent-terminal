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
    /// Turns stored before costs were per turn, counted for tokens only.
    withheld_turns: u64,
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
        withheld_turns: data.withheld_turns,
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
pub(super) fn notes(report: &Report, withheld_turns: u64) -> Vec<String> {
    let mut out = Vec::new();
    if report.total.delegated_turns > 0 {
        out.push(format!(
            "From sub-agents, counted under the thread that started them: {} ({}).",
            amount_text_of_costs(&report.total.delegated, report.total.delegated_turns),
            turns(report.total.delegated_turns)
        ));
    }
    if withheld_turns > 0 {
        out.push(format!(
            "{} stored before each turn's own cost was recorded {} counted for tokens only.",
            turns(withheld_turns),
            if withheld_turns == 1 { "is" } else { "are" }
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

fn amount_text_of_costs(costs: &spend::Costs, delegated_turns: u64) -> String {
    if delegated_turns == 0 {
        return "nothing".to_owned();
    }
    costs.describe()
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
fn show_report(
    content: &gtk4::Box,
    report: &Report,
    withheld_turns: u64,
    names: &Names,
    plan_lines: &[String],
) {
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
    for note in notes(report, withheld_turns) {
        content.append(&label(&note, &["dim-label", "caption"]));
    }
}

/// The live plan windows of every agent that has some, one line each.
fn plan_lines(status: &AccountStatus) -> Vec<String> {
    Driver::ALL
        .into_iter()
        .filter_map(|d| plan_usage_line(d.info().label, &status.snapshot(d).windows))
        .collect()
}

fn now_ms() -> i64 {
    glib::real_time() / 1000
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
        let now = now_ms();
        glib::spawn_future_local(glib::clone!(
            #[weak]
            stack,
            async move {
                // Made once, through the writer; a failure leaves the read slower, never wrong.
                match write_job("make the spending index", |s| s.ensure_spend_index()).await {
                    Some(Ok(())) => {}
                    Some(Err(e)) => tracing::warn!(error = %e, "no spending index"),
                    None => tracing::warn!("the spending index was not made"),
                }
                let loaded = match store_job(move |s| load(s, now)).await {
                    Some(Ok(loaded)) => loaded,
                    Some(Err(why)) => return show_error(&stack, &why),
                    None => return show_error(&stack, "The thread store could not be read."),
                };
                show_loaded(&stack, Rc::new(loaded), names, plan);
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

fn show_loaded(stack: &gtk4::Stack, loaded: Rc<Loaded>, names: Rc<Names>, plan: Rc<Vec<String>>) {
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
                show_report(&content, report, loaded.withheld_turns, &names, &plan);
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
    stack.add_named(&scrolled, Some("report"));
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
        let r = report(&[
            row("claude", Some(1.0), false),
            row("claude", Some(0.25), true),
        ]);
        let n = notes(&r, 2);
        assert!(n[0].contains("$0.25 reported (1 turn)"), "{n:?}");
        assert!(n[1].starts_with("2 turns stored before"), "{n:?}");
        assert!(n.last().is_some_and(|l| l.contains("list price")));
        assert_eq!(notes(&report(&[]), 0).len(), 1);
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
