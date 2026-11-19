//! Ask Several Agents (`F7`): `/ask` and Composer's "Ask Several Agents…" menu item.
//!
//! Forks the current thread into 2 to 4 parallel participant threads running different (or the
//! same) agents/models/profiles on the same prompt, starting each participant in the background
//! without stealing focus, and opens a [`CompareWindow`] to read all answers and diffs side by
//! side, keep the best one (with Undo), or return its redacted brief back to the source thread.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use agent_core::adapter::Driver;
use agent_core::ask_several::{
    validate_choices, CompareGroup, CompareParticipant, TargetSpec, MAX_CHOICES, MIN_CHOICES,
};
use agent_core::event::{Envelope, Event};
use agent_core::fork_compare::ForkLink;
use agent_core::redact::redact;
use agent_kit::store::{CompareColumn, KeepGroupOutcome};
use gtk4::glib;
use tracing::info;

use super::fork_compare::{column_view, fork_baseline, not_compared_note, Reread, SHARED_FOLDER};
use super::threads::{
    app_handle, carried_with_stored, load_thread, off_main, picker_specs, record_new_thread_as,
    save_pending_prompt, store_job, CreatedThread, CreationNotice, LeftBehind, NewThreadIds,
    NewThreadSpec,
};
use super::*;
use crate::chat::session::{build_answer_handoff, build_handoff, seeded_handoff_events};
use crate::git::{ForkBaseline, ForkChanges};
use crate::model_catalog::ModelCatalog;
use crate::window::compare::{
    ChangesColumn, ChangesContent, ChangesState, ColumnView, CompareContent, CompareWindow,
    KeepState,
};
use crate::window::sidebar_model::{display_title, RowKey};

/// Selectable `(driver, profile, model, effort)` target offered in the Ask Several dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TargetOption {
    label: String,
    driver: Driver,
    profile_id: Option<String>,
    model: Option<String>,
    effort: Option<String>,
}

/// Weak widget handles for one participant row in the Ask Several dialog so signal closures never
/// form strong reference cycles.
#[derive(Clone)]
struct ParticipantRow {
    root: glib::WeakRef<gtk4::Box>,
    dropdown: glib::WeakRef<gtk4::DropDown>,
    worktree: glib::WeakRef<gtk4::Switch>,
    note: glib::WeakRef<gtk4::Label>,
    remove: glib::WeakRef<gtk4::Button>,
}

/// A Group Keep that landed, for Undo and withdrawing any clean-worktree removal toasts.
struct GroupKeepDone {
    group_id: String,
    kept: String,
    kept_thread: String,
    source_thread: String,
    handoff_fence: String,
    archived: Vec<String>,
    undone: Cell<bool>,
    removal: RefCell<Vec<adw::Toast>>,
}

fn unix_now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Redacts first so no secret crossing the preview boundary can survive as an unmasked fragment.
fn redacted_prompt_preview(prompt: &str) -> String {
    redact(prompt).chars().take(120).collect()
}

/// Formats an error message together with any armed [`LeftBehind`] worktree notes.
fn left_behind_said(guards: &[LeftBehind], text: &str) -> String {
    let mut parts = vec![redact(text)];
    for g in guards {
        if let Some(note) = g.note() {
            parts.push(note.to_owned());
        }
    }
    parts.join("\n\n")
}

/// Builds the list of selectable `(driver, profile, model)` options for the Ask Several dialog
/// from configured usable agents and cached catalogue models.
fn target_options(
    config: &crate::config::TerminalConfig,
    usable: impl Fn(Driver) -> bool,
) -> Vec<TargetOption> {
    let catalog = ModelCatalog::shared();
    let specs = picker_specs(config, None, Driver::Claude);
    let mut out = Vec::new();
    for spec in specs {
        if !usable(spec.driver) {
            continue;
        }
        let models = catalog.models_for(spec.driver, spec.source.as_ref());
        // Local model servers require a concrete model name when present in the catalogue.
        if !agent_core::provider::required(spec.driver) || models.is_empty() {
            out.push(TargetOption {
                label: spec.name.clone(),
                driver: spec.driver,
                profile_id: spec.profile.clone(),
                model: None,
                effort: None,
            });
        }
        for m in models {
            out.push(TargetOption {
                label: format!("{} · {}", spec.name, m.display),
                driver: spec.driver,
                profile_id: spec.profile.clone(),
                model: Some(m.model_id_for(None)),
                effort: None,
            });
        }
    }
    out
}

fn target_cost(
    config: &crate::config::TerminalConfig,
    driver: Driver,
    profile: Option<&str>,
) -> (bool, agent_core::model_tags::Headroom) {
    use agent_core::model_tags::Headroom;
    let p = config.thread_profile(profile, driver);
    let free = p.is_some_and(|p| crate::config::profile_is_free(p, driver));
    let server = p.and_then(|p| crate::config::profile_provider(p, driver));
    let headroom = if server.is_some() {
        p.and_then(|p| {
            crate::local_headroom::LocalHeadroom::shared()
                .usage_of(p, crate::freshness::now_epoch())
        })
        .map(|u| u.headroom)
        .unwrap_or(Headroom::Unknown)
    } else {
        crate::usage_file::headroom_of(
            &crate::account_status::AccountStatus::shared().snapshot(driver),
            crate::freshness::now_epoch(),
        )
    };
    (free, headroom)
}

/// Ensures two participant columns picking the same target option get distinct readable headings.
fn disambiguate_labels(choices: &mut [TargetSpec]) {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for c in choices.iter() {
        *counts.entry(c.label.clone()).or_insert(0) += 1;
    }
    let mut seen: HashMap<String, usize> = HashMap::new();
    for c in choices.iter_mut() {
        if counts.get(&c.label).copied().unwrap_or(0) > 1 {
            let n = seen.entry(c.label.clone()).or_insert(0);
            *n += 1;
            c.label = format!("{} ({n})", c.label);
        }
    }
}

/// Default initial indices into `options` preferring distinct drivers for the initial two rows.
fn default_option_indices(options: &[TargetOption]) -> Vec<u32> {
    if options.is_empty() {
        return Vec::new();
    }
    let first = 0usize;
    let second = options
        .iter()
        .position(|o| o.driver != options[first].driver)
        .unwrap_or_else(|| usize::from(options.len() > 1));
    vec![first as u32, second as u32]
}

/// Summarises the last assistant reply from a participant's entries after its fork seq.
fn last_reply_text(column: &CompareColumn) -> String {
    column
        .entries
        .iter()
        .rev()
        .find(|e| e.role == agent_core::compare::Role::Reply && !e.text.trim().is_empty())
        .map(|e| e.text.clone())
        .unwrap_or_default()
}

fn participant_column_view(
    participant: &CompareParticipant,
    column: CompareColumn,
    keep: KeepState,
) -> ColumnView {
    let mut view = column_view(column);
    if !participant.label.trim().is_empty() {
        view.heading = participant.label.clone();
    }
    view.keep = keep;
    view
}

impl AgentTerminalWindow {
    /// A source's live comparison shelf. Weak widgets stop the timer when the page closes;
    /// at most one bounded store read is in flight for this page.
    pub(super) fn attach_compare_strip(&self, thread: &str, view: &crate::chat::view::ChatView) {
        let weak = view.downgrade();
        let obj = self.obj().downgrade();
        let source = thread.to_owned();
        let reading = Rc::new(Cell::new(false));
        let signature = Rc::new(RefCell::new(String::new()));
        glib::timeout_add_local(std::time::Duration::from_millis(1000), move || {
            let (Some(view), Some(obj)) = (weak.upgrade(), obj.upgrade()) else {
                return glib::ControlFlow::Break;
            };
            if obj.imp().is_closing() {
                return glib::ControlFlow::Break;
            }
            if !view.is_mapped() {
                return glib::ControlFlow::Continue;
            }
            if reading.replace(true) {
                return glib::ControlFlow::Continue;
            }
            let (weak, obj, source, reading, signature) = (
                view.downgrade(),
                obj.downgrade(),
                source.clone(),
                reading.clone(),
                signature.clone(),
            );
            glib::MainContext::default().spawn_local(async move {
                let groups = store_job(move |s| -> agent_kit::store::Result<_> {
                    // The latest run fits above the composer; the picker retains older runs.
                    let groups: Vec<_> = s.compare_groups_for_source(&source)?.into_iter().take(1).collect();
                    let mut outcomes = HashMap::new();
                    for p in groups.iter().flat_map(|g| &g.participants) {
                        let max = s.max_seq(&p.thread_id)?;
                        let events = s.events(&p.thread_id, Some(max.saturating_sub(512).max(p.after_seq)), 512)?;
                        let state = events.iter().rev().find_map(|(_, e)| match e.event {
                            Event::TurnCompleted { state, .. } => Some(state),
                            _ => None,
                        });
                        outcomes.insert(p.thread_id.clone(), state);
                    }
                    Ok((groups, outcomes))
                }).await;
                reading.set(false);
                let (Some(view), Some(obj), Some(Ok((groups, outcomes)))) = (weak.upgrade(), obj.upgrade(), groups) else { return };
                let imp = obj.imp();
                let states: Vec<_> = groups.iter().flat_map(|g| g.participants.iter()).map(|p| {
                    let owner = super::threads::thread_owner(&p.thread_id);
                    let rows = owner.as_ref().map(|w| w.imp().sidebar_rows()).unwrap_or_else(|| imp.sidebar_rows());
                    let row = rows.iter().find(|r| r.key == RowKey::Thread(p.thread_id.clone()));
                    use crate::window::sidebar_model::Badge;
                    let badge = row.and_then(|r| r.badge);
                    let status = if row.is_none() { "Unavailable" } else if row.is_some_and(|r| r.archived) { "Discarded" } else {
                        agent_core::ask_several::participant_status(
                            badge == Some(Badge::NeedsApproval), badge == Some(Badge::RateLimited),
                            matches!(badge, Some(Badge::Running | Badge::Background))
                                || row.is_some_and(|r| r.activity == Some(crate::window::sidebar_model::ThreadActivity::Starting)),
                            outcomes.get(&p.thread_id).copied().flatten())
                    };
                    (p.thread_id.clone(), status)
                }).collect();
                let next = format!("{groups:?}{states:?}");
                if *signature.borrow() == next { return; }
                *signature.borrow_mut() = next;
                let strip = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
                for group in groups {
                    let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
                    let all_discarded = group.participants.iter().all(|p| states.iter().any(|(id, status)| id == &p.thread_id && *status == "Discarded"));
                    let title = gtk4::Label::builder().label(if group.kept_thread.is_some() { "Agent comparison · answer kept" } else if all_discarded { "Agent comparison · all participants discarded" } else { "Agent comparison" }).hexpand(true).xalign(0.0).build();
                    header.append(&title);
                    let compare = gtk4::Button::with_label("Compare");
                    let (weak, gid) = (obj.downgrade(), group.id.clone());
                    compare.connect_clicked(move |_| { if let Some(obj) = weak.upgrade() { obj.imp().open_group_compare(&gid); } });
                    header.append(&compare);
                    strip.append(&header);
                    for (index, p) in group.participants.iter().enumerate() {
                        let row = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
                        let state = states.iter().find(|(id, _)| id == &p.thread_id).map_or("Unavailable", |(_, s)| *s);
                        let state = if group.kept_thread.as_deref() == Some(&p.thread_id) { "Kept" } else { state };
                        let label = gtk4::Label::builder().label(format!("{} · {state}", p.label)).hexpand(true).xalign(0.0).wrap(true).build();
                        row.append(&label);
                        let actions = gtk4::FlowBox::builder().selection_mode(gtk4::SelectionMode::None).column_spacing(6).row_spacing(6).max_children_per_line(4).build();
                        for action in ["Open", "Keep", "Discard", "Keep this thread instead"] {
                            let button = gtk4::Button::with_label(action);
                            button.update_property(&[gtk4::accessible::Property::Label(&format!("{action}: {}", p.label))]);
                            button.set_sensitive(match action {
                                "Keep" => group.kept_thread.is_none() && !matches!(state, "Discarded" | "Unavailable" | "Working" | "Needs approval" | "Waiting for limit"),
                                "Discard" => state != "Discarded" && group.kept_thread.as_deref() != Some(&p.thread_id),
                                _ => true,
                            });
                            let (weak, gid, id) = (obj.downgrade(), group.id.clone(), p.thread_id.clone());
                            button.connect_clicked(move |_| {
                                let Some(obj) = weak.upgrade() else { return };
                                match action {
                                    "Open" => { obj.imp().open_thread(&id, true); }
                                    "Keep" => {
                                        let over = CompareWindow::new(obj.upcast_ref(), "Compare Agents");
                                        load_group_compare(&over, gid.clone());
                                        let (weak, gid_r) = (over.downgrade(), gid.clone());
                                        obj.imp().confirm_keep_group(&over, gid.clone(), index, Reread::new(move || {
                                            if let Some(over) = weak.upgrade() { load_group_compare(&over, gid_r); }
                                        }));
                                        over.window().present();
                                    }
                                    "Discard" => obj.imp().set_thread_archived(&id, true),
                                    _ => {
                                        let (gid, id) = (gid.clone(), id.clone());
                                        obj.imp().write_store("detach comparison participant", move |s| s.detach_compare_participant(&gid, &id));
                                    }
                                }
                            });
                            actions.insert(&button, -1);
                        }
                        row.append(&actions);
                        strip.append(&row);
                    }
                }
                view.set_compare_strip(strip.first_child().is_some().then_some(&strip));
            });
            glib::ControlFlow::Continue
        });
    }

    /// Entry point for `/ask [prompt]` and the composer's "Ask Several Agents…" action.
    pub(super) fn ask_several_from(&self, thread: &str, initial_prompt: Option<String>) {
        if self.is_closing() {
            return;
        }
        let dir = self
            .with_thread_tab(thread, |t| t.dir.clone())
            .or_else(|| self.summary_of(thread).map(|s| s.cwd));
        let Some(dir) = dir else {
            self.show_toast("Cannot open Ask Several: thread folder is unknown");
            return;
        };
        if self.thread_is_remote(thread) || agent_core::remote::is_remote_cwd(&dir) {
            self.show_toast("Ask several does not run on a remote thread");
            return;
        }
        let options = target_options(&self.config.borrow(), |d| self.agent_usable(d));
        if options.is_empty() {
            present_message(
                &self.obj(),
                "No Chat Agent Available",
                "None of Claude, Antigravity or Codex is installed and enabled. Install one, \
                 or enable it in Settings → Agents.",
            );
            return;
        }
        let obj = self.obj().downgrade();
        let thread = thread.to_owned();
        glib::MainContext::default().spawn_local(async move {
            let dir_check = dir.clone();
            let (git_ok, in_repo) = off_main("checking git repository", move || {
                let git_ok = agent_kit::git::git_installed();
                let in_repo = git_ok
                    && agent_kit::git::discover(Path::new(&dir_check))
                        .ok()
                        .flatten()
                        .is_some();
                Ok((git_ok, in_repo))
            })
            .await
            .unwrap_or((false, false));
            let history_tokens = store_job({
                let source = thread.clone();
                move |s| -> agent_kit::store::Result<usize> {
                    let handoff = build_handoff(&s.transcript_messages(&source)?, "source thread");
                    Ok(agent_core::local_context::estimated_tokens(
                        handoff.summary.len(),
                        handoff.carried,
                    ))
                }
            })
            .await
            .and_then(Result::ok);
            let Some(obj) = obj.upgrade() else { return };
            if obj.imp().is_closing() {
                return;
            }
            obj.imp().present_ask_several_dialog(
                thread,
                options,
                initial_prompt,
                git_ok,
                in_repo,
                history_tokens,
            );
        });
    }

    fn present_ask_several_dialog(
        &self,
        source_thread: String,
        options: Vec<TargetOption>,
        initial_prompt: Option<String>,
        git_ok: bool,
        in_repo: bool,
        history_tokens: Option<usize>,
    ) {
        let dialog = adw::AlertDialog::builder()
            .heading("Ask Several Agents")
            .body("Fork this thread to 2–4 agents in parallel, compare their replies side by side, and keep the best one.")
            .build();
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("ask", "Ask All");
        dialog.set_default_response(Some("ask"));
        dialog.set_close_response("cancel");
        dialog.set_response_appearance("ask", adw::ResponseAppearance::Suggested);

        let outer = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .spacing(12)
            .width_request(460)
            .build();

        let prompt_label = gtk4::Label::builder()
            .label("Prompt")
            .xalign(0.0)
            .css_classes(["heading"])
            .build();
        let buffer = gtk4::TextBuffer::new(None);
        if let Some(initial) = initial_prompt {
            buffer.set_text(initial.trim());
        }
        let prompt_view = gtk4::TextView::builder()
            .buffer(&buffer)
            .wrap_mode(gtk4::WrapMode::WordChar)
            .accepts_tab(false)
            .top_margin(8)
            .bottom_margin(8)
            .left_margin(8)
            .right_margin(8)
            .build();
        prompt_view.update_property(&[gtk4::accessible::Property::Label("Prompt for all agents")]);
        let prompt_scroll = gtk4::ScrolledWindow::builder()
            .min_content_height(96)
            .max_content_height(180)
            .css_classes(["card"])
            .child(&prompt_view)
            .build();

        let rows_header = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .spacing(6)
            .build();
        let rows_title = gtk4::Label::builder()
            .label("Agents (2–4)")
            .xalign(0.0)
            .hexpand(true)
            .css_classes(["heading"])
            .build();
        let add_btn = gtk4::Button::builder()
            .label("Add Agent")
            .icon_name("at-list-add-symbolic")
            .css_classes(["flat"])
            .build();
        rows_header.append(&rows_title);
        rows_header.append(&add_btn);

        let rows_box = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .spacing(8)
            .build();

        let status = gtk4::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["warning", "caption"])
            .visible(false)
            .build();

        outer.append(&prompt_label);
        outer.append(&prompt_scroll);
        outer.append(&rows_header);
        outer.append(&rows_box);
        let agy_ack = gtk4::CheckButton::with_label(
            "I accept Antigravity's forced permissive mode: it can edit without asking, including outside its worktree, and access the network.",
        );
        if let Some(label) = agy_ack.child().and_downcast::<gtk4::Label>() {
            label.set_wrap(true);
        }
        agy_ack.set_visible(false);
        let exhausted_ack = gtk4::CheckButton::with_label("Ask exhausted accounts anyway");
        exhausted_ack.set_visible(false);
        let summary = gtk4::Label::builder().xalign(0.0).wrap(true).build();
        outer.append(&summary);
        outer.append(&agy_ack);
        outer.append(&exhausted_ack);
        outer.append(&status);
        dialog.set_extra_child(Some(&outer));

        let rows: Rc<RefCell<Vec<ParticipantRow>>> = Rc::new(RefCell::new(Vec::new()));
        let cost_labels: Vec<String> = options
            .iter()
            .map(|o| {
                let (free, headroom) =
                    target_cost(&self.config.borrow(), o.driver, o.profile_id.as_deref());
                format!(
                    "{} · {} · headroom {}",
                    o.label,
                    if free { "Free" } else { "Metered" },
                    headroom.as_str()
                )
            })
            .collect();
        let labels_storage: Vec<&str> = cost_labels.iter().map(String::as_str).collect();
        let model_list = gtk4::StringList::new(&labels_storage);
        let options = Rc::new(options);

        let collect_specs = {
            let rows_weak = Rc::downgrade(&rows);
            let options = Rc::clone(&options);
            move || -> Vec<TargetSpec> {
                let Some(rows) = rows_weak.upgrade() else {
                    return Vec::new();
                };
                let out = rows
                    .borrow()
                    .iter()
                    .filter_map(|row| {
                        let dropdown = row.dropdown.upgrade()?;
                        let worktree = row.worktree.upgrade()?;
                        let idx = usize::try_from(dropdown.selected()).ok()?;
                        let opt = options.get(idx)?;
                        let wt = opt.driver == Driver::Agy || worktree.is_active();
                        Some(TargetSpec {
                            label: opt.label.clone(),
                            driver: opt.driver,
                            profile_id: opt.profile_id.clone(),
                            model: opt.model.clone(),
                            effort: opt.effort.clone(),
                            worktree: wt,
                        })
                    })
                    .collect();
                out
            }
        };

        let revalidate = {
            let dialog_weak = dialog.downgrade();
            let buffer_weak = buffer.downgrade();
            let rows_weak = Rc::downgrade(&rows);
            let options = Rc::clone(&options);
            let add_weak = add_btn.downgrade();
            let status_weak = status.downgrade();
            let collect_specs = collect_specs.clone();
            let agy_ack = agy_ack.downgrade();
            let exhausted_ack = exhausted_ack.downgrade();
            let summary = summary.downgrade();
            let config = self.config.clone();
            Rc::new(move || {
                let (Some(dialog), Some(buffer), Some(rows), Some(add_btn), Some(status)) = (
                    dialog_weak.upgrade(),
                    buffer_weak.upgrade(),
                    rows_weak.upgrade(),
                    add_weak.upgrade(),
                    status_weak.upgrade(),
                ) else {
                    return;
                };
                let count = rows.borrow().len();
                add_btn.set_sensitive(count < MAX_CHOICES);
                for row in rows.borrow().iter() {
                    let (Some(remove), Some(dropdown), Some(worktree), Some(note)) = (
                        row.remove.upgrade(),
                        row.dropdown.upgrade(),
                        row.worktree.upgrade(),
                        row.note.upgrade(),
                    ) else {
                        continue;
                    };
                    remove.set_sensitive(count > MIN_CHOICES);
                    let opt = usize::try_from(dropdown.selected())
                        .ok()
                        .and_then(|i| options.get(i));
                    let is_agy = opt.is_some_and(|o| o.driver == Driver::Agy);
                    let cost_note = opt
                        .map(|o| {
                            let (free, h) =
                                target_cost(&config.borrow(), o.driver, o.profile_id.as_deref());
                            format!(
                                "{} · headroom {}",
                                if free { "Free" } else { "Metered" },
                                h.as_str()
                            )
                        })
                        .unwrap_or_default();
                    if is_agy {
                        if !worktree.is_active() {
                            worktree.set_active(true);
                        }
                        worktree.set_sensitive(false);
                        note.set_label(&format!("{cost_note}. Antigravity runs in forced dangerous permissive mode in an isolated worktree. Plan cannot prevent its edits."));
                        note.set_visible(true);
                    } else {
                        worktree.set_sensitive(in_repo);
                        note.set_label(&cost_note);
                        note.set_visible(true);
                    }
                }
                let specs = collect_specs();
                let (Some(agy_ack), Some(exhausted_ack), Some(summary)) = (
                    agy_ack.upgrade(),
                    exhausted_ack.upgrade(),
                    summary.upgrade(),
                ) else {
                    return;
                };
                let agy = specs.iter().any(|c| c.driver == Driver::Agy);
                let costs: Vec<_> = specs
                    .iter()
                    .map(|c| target_cost(&config.borrow(), c.driver, c.profile_id.as_deref()))
                    .collect();
                let exhausted = costs
                    .iter()
                    .any(|(_, h)| *h == agent_core::model_tags::Headroom::Exhausted);
                agy_ack.set_visible(agy);
                exhausted_ack.set_visible(exhausted);
                let history = history_tokens.map_or_else(
                    || "budgeted history (size unavailable)".to_owned(),
                    |n| format!("budgeted history (~{n} estimated tokens)"),
                );
                summary.set_label(&format!("Sends this thread's {history} and your prompt to {} agents, {} metered. Shared folders run in Plan; worktrees carry the source's safe mode.", specs.len(), costs.iter().filter(|(free, _)| !free).count()));
                let needs_wt = specs.iter().any(|c| c.worktree);
                let prompt_empty = buffer
                    .text(&buffer.start_iter(), &buffer.end_iter(), false)
                    .trim()
                    .is_empty();
                let err = if needs_wt && !in_repo {
                    Some(if !git_ok {
                        "Git is not installed, so isolated worktrees cannot be used."
                    } else {
                        "This folder is not a git repository; isolated worktrees (needed for Antigravity) are unavailable."
                    })
                } else if let Err(why) = validate_choices(&specs, false) {
                    Some(why)
                } else if agy && !agy_ack.is_active() {
                    Some("Accept Antigravity's forced mode before asking.")
                } else if exhausted && !exhausted_ack.is_active() {
                    Some("Confirm asking exhausted accounts before continuing.")
                } else if prompt_empty {
                    Some("Enter a prompt to send to all participants.")
                } else {
                    None
                };
                if let Some(msg) = err {
                    status.set_label(msg);
                    status.set_visible(true);
                    dialog.set_response_enabled("ask", false);
                } else {
                    status.set_visible(false);
                    dialog.set_response_enabled("ask", true);
                }
            })
        };
        for ack in [&agy_ack, &exhausted_ack] {
            let revalidate = revalidate.clone();
            ack.connect_toggled(move |_| revalidate());
        }
        let accounts = crate::account_status::AccountStatus::shared();
        let refresh = revalidate.clone();
        let listener = accounts.connect_changed(move || refresh());
        dialog.connect_closed(move |_| accounts.disconnect(listener));

        let add_row = {
            let rows_weak = Rc::downgrade(&rows);
            let box_weak = rows_box.downgrade();
            let model_weak = model_list.downgrade();
            let rev_weak = Rc::downgrade(&revalidate);
            Rc::new(move |initial_idx: u32| {
                let (Some(rows), Some(rows_box), Some(model_list)) = (
                    rows_weak.upgrade(),
                    box_weak.upgrade(),
                    model_weak.upgrade(),
                ) else {
                    return;
                };
                if rows.borrow().len() >= MAX_CHOICES {
                    return;
                }
                let container = gtk4::Box::builder()
                    .orientation(gtk4::Orientation::Vertical)
                    .spacing(4)
                    .css_classes(["card"])
                    .margin_top(2)
                    .margin_bottom(2)
                    .build();
                let line = gtk4::Box::builder()
                    .orientation(gtk4::Orientation::Horizontal)
                    .spacing(8)
                    .margin_start(8)
                    .margin_end(8)
                    .margin_top(6)
                    .margin_bottom(6)
                    .build();
                let dropdown =
                    gtk4::DropDown::new(Some(model_list.clone()), gtk4::Expression::NONE);
                dropdown.set_hexpand(true);
                if initial_idx < model_list.n_items() {
                    dropdown.set_selected(initial_idx);
                }
                let wt_label = gtk4::Label::builder()
                    .label("Worktree")
                    .css_classes(["caption"])
                    .build();
                let worktree = gtk4::Switch::builder()
                    .valign(gtk4::Align::Center)
                    .tooltip_text("Run this participant in its own isolated git worktree")
                    .build();
                worktree.update_property(&[gtk4::accessible::Property::Label("Isolated worktree")]);
                let remove = gtk4::Button::builder()
                    .icon_name("at-window-close-symbolic")
                    .tooltip_text("Remove participant")
                    .css_classes(["flat", "circular"])
                    .valign(gtk4::Align::Center)
                    .build();
                line.append(&dropdown);
                line.append(&wt_label);
                line.append(&worktree);
                line.append(&remove);

                let note = gtk4::Label::builder()
                    .xalign(0.0)
                    .margin_start(8)
                    .margin_end(8)
                    .margin_bottom(6)
                    .css_classes(["caption", "warning"])
                    .wrap(true)
                    .visible(false)
                    .build();
                container.append(&line);
                container.append(&note);
                rows_box.append(&container);

                let row = ParticipantRow {
                    root: container.downgrade(),
                    dropdown: dropdown.downgrade(),
                    worktree: worktree.downgrade(),
                    note: note.downgrade(),
                    remove: remove.downgrade(),
                };
                rows.borrow_mut().push(row);

                let rev = rev_weak.clone();
                dropdown.connect_selected_notify(move |_| {
                    if let Some(rev) = rev.upgrade() {
                        rev();
                    }
                });
                let rev = rev_weak.clone();
                worktree.connect_active_notify(move |_| {
                    if let Some(rev) = rev.upgrade() {
                        rev();
                    }
                });
                let rows_weak_btn = rows_weak.clone();
                let box_weak_btn = box_weak.clone();
                let root_weak = container.downgrade();
                let rev = rev_weak.clone();
                remove.connect_clicked(move |_| {
                    let (Some(rows), Some(rows_box), Some(container)) = (
                        rows_weak_btn.upgrade(),
                        box_weak_btn.upgrade(),
                        root_weak.upgrade(),
                    ) else {
                        return;
                    };
                    if rows.borrow().len() <= MIN_CHOICES {
                        return;
                    }
                    rows.borrow_mut()
                        .retain(|r| r.root.upgrade().is_some_and(|w| w != container));
                    rows_box.remove(&container);
                    if let Some(rev) = rev.upgrade() {
                        rev();
                    }
                });
                if let Some(rev) = rev_weak.upgrade() {
                    rev();
                }
            })
        };

        for idx in default_option_indices(&options) {
            add_row(idx);
        }
        {
            let rows_weak = Rc::downgrade(&rows);
            let options_len = options.len() as u32;
            let add_weak = Rc::downgrade(&add_row);
            add_btn.connect_clicked(move |_| {
                let (Some(rows), Some(add_row)) = (rows_weak.upgrade(), add_weak.upgrade()) else {
                    return;
                };
                let next = if options_len == 0 {
                    0
                } else {
                    (rows.borrow().len() as u32) % options_len
                };
                add_row(next);
            });
        }
        {
            let rev_weak = Rc::downgrade(&revalidate);
            buffer.connect_changed(move |_| {
                if let Some(rev) = rev_weak.upgrade() {
                    rev();
                }
            });
        }
        revalidate();

        let rows_cleanup = rows.clone();
        let buffer_weak = buffer.downgrade();
        let collect_for_ask = collect_specs;
        // Keep `add_row` and `revalidate` alive while the dialog is open, then clear row
        // bookkeeping after reading selections (libadwaita emits `closed` before `response`).
        let keep_closures = (add_row, revalidate);
        self.ask(dialog, "ask", move |imp, go| {
            let _ = &keep_closures;
            if !go {
                rows_cleanup.borrow_mut().clear();
                return;
            }
            let Some(buffer) = buffer_weak.upgrade() else {
                rows_cleanup.borrow_mut().clear();
                return;
            };
            let prompt = buffer
                .text(&buffer.start_iter(), &buffer.end_iter(), false)
                .trim()
                .to_owned();
            let mut choices = collect_for_ask();
            rows_cleanup.borrow_mut().clear();
            disambiguate_labels(&mut choices);
            if validate_choices(&choices, false).is_err() || prompt.is_empty() {
                return;
            }
            imp.launch_ask_several(source_thread.clone(), prompt, choices);
        });
    }

    /// Creates the participant forks (plus isolated worktrees & git baseline when applicable),
    /// stores the [`CompareGroup`], opens & starts all participants in the background without
    /// stealing focus, and opens [`CompareWindow`].
    fn launch_ask_several(&self, source_thread: String, prompt: String, choices: Vec<TargetSpec>) {
        if self.is_closing() {
            return;
        }
        let dir = self
            .with_thread_tab(&source_thread, |t| t.dir.clone())
            .or_else(|| self.summary_of(&source_thread).map(|s| s.cwd));
        let Some(dir) = dir else {
            self.show_toast("Cannot launch Ask Several: thread folder is gone");
            return;
        };
        let title = self.thread_title_of(&source_thread);
        let root = {
            let root = self.config.borrow().worktree_root.trim().to_string();
            (!root.is_empty()).then(|| PathBuf::from(crate::utils::expand_tilde(&root)))
        };
        let checkpoints = self.config.borrow().checkpoints;
        let source_draft = self
            .with_chat(&source_thread, |c| {
                c.view.as_ref().map(|v| v.composer_schedule_draft())
            })
            .flatten();
        let carry = self.thread_carry_settings(&source_thread);
        let needs_shared_baseline = checkpoints && choices.iter().any(|c| !c.worktree);
        let obj = self.obj().downgrade();

        glib::MainContext::default().spawn_local(async move {
            let loaded = store_job({
                let source = source_thread.clone();
                move |store| {
                    let loaded = load_thread(store, &source, None)?;
                    let (messages, fork_seq) = store.transcript_at_fork(&source)?;
                    let handoff = build_handoff(&messages, &format!("thread {source}"));
                    Ok::<_, agent_kit::store::StoreError>((handoff, loaded, fork_seq))
                }
            })
            .await;

            let Some(obj_up) = obj.upgrade() else { return };
            if obj_up.imp().is_closing() {
                return;
            }
            let (handoff, loaded, fork_seq) = match loaded {
                Some(Ok(v)) => v,
                Some(Err(e)) => {
                    present_message(
                        &obj_up,
                        "Cannot Launch Ask Several",
                        &redact(&e.to_string()),
                    );
                    return;
                }
                None => {
                    obj_up.imp().show_toast("Cannot read the source thread");
                    return;
                }
            };
            drop(obj_up);

            // Plan and create any requested isolated worktrees sequentially so branch names do not
            // collide. Return already-created worktrees on partial error so guards warn accurately.
            let wt_flags: Vec<(String, bool)> = choices
                .iter()
                .map(|c| (c.label.clone(), c.worktree))
                .collect();
            let dir_git = dir.clone();
            let title_git = title.clone();
            let git_prep = off_main("preparing Ask Several worktrees", move || {
                let mut worktrees: Vec<Option<crate::worktree::ForkWorktree>> =
                    Vec::with_capacity(wt_flags.len());
                let mut created_so_far: Vec<crate::worktree::ForkWorktree> = Vec::new();
                for (label, want_wt) in wt_flags {
                    if !want_wt {
                        worktrees.push(None);
                        continue;
                    }
                    let slug = format!("{title_git} {label}");
                    let plan =
                        match crate::worktree::plan_fork(Path::new(&dir_git), &slug, root.as_deref()) {
                            Ok(plan) => plan,
                            Err(e) => {
                                return Ok(Err((
                                    worktree_repo_problem(Path::new(&dir_git), &e),
                                    created_so_far,
                                )));
                            }
                        };
                    let made = match crate::worktree::add_fork(Path::new(&dir_git), &plan) {
                        Ok(made) => made,
                        Err(e) => return Ok(Err((e, created_so_far))),
                    };
                    created_so_far.push(made.clone());
                    worktrees.push(Some(made));
                }
                Ok::<_, String>(Ok(worktrees))
            })
            .await;

            let worktrees = match git_prep {
                Ok(Ok(w)) => w,
                Ok(Err((msg, partial))) => {
                    let guards: Vec<LeftBehind> = partial
                        .iter()
                        .map(|m| LeftBehind::fork_worktree(&m.info))
                        .collect();
                    if let Some(obj_up) = obj.upgrade() {
                        present_message(
                            &obj_up,
                            "Could Not Prepare Worktrees",
                            &left_behind_said(&guards, &msg),
                        );
                    }
                    return;
                }
                Err(e) => {
                    if let Some(obj_up) = obj.upgrade() {
                        present_message(&obj_up, "Could Not Prepare Worktrees", &redact(&e));
                    }
                    return;
                }
            };

            // Arm LeftBehind guards immediately for every created worktree before any further await.
            let mut guards: Vec<LeftBehind> = worktrees
                .iter()
                .map(|m| {
                    m.as_ref()
                        .map(|m| LeftBehind::fork_worktree(&m.info))
                        .unwrap_or_default()
                })
                .collect();

            let shared_baseline: Option<ForkBaseline> = if needs_shared_baseline {
                fork_baseline(dir.clone()).await
            } else {
                None
            };

            let Some(obj_up) = obj.upgrade() else { return };
            let imp = obj_up.imp();
            if imp.is_closing() {
                return;
            }

            let settings = carried_with_stored(carry.model, carry.effort, carry.mode, &loaded);
            let carries_handoff = handoff.carried > 0;
            let source_label = if title.is_empty() {
                "the previous thread".to_owned()
            } else {
                format!("“{title}”")
            };

            let store = app_handle();
            let group_id = match store.new_id() {
                Ok(id) => id,
                Err(e) => {
                    present_message(
                        &obj_up,
                        "Cannot Create Comparison Group",
                        &left_behind_said(&guards, &e.to_string()),
                    );
                    return;
                }
            };

            let mut specs = Vec::with_capacity(choices.len());
            let mut participants = Vec::with_capacity(choices.len());
            let mut created_list = Vec::with_capacity(choices.len());

            for (choice, made) in choices.into_iter().zip(worktrees) {
                let guard = guards.remove(0);
                let ids = match store.new_id().and_then(|thread| {
                    Ok(NewThreadIds {
                        thread,
                        provider: store.new_id()?,
                    })
                }) {
                    Ok(ids) => ids,
                    Err(e) => {
                        let mut all_guards = vec![guard];
                        all_guards.extend(guards);
                        all_guards.extend(
                            created_list
                                .into_iter()
                                .map(|(_, _, g): (String, CreatedThread, LeftBehind)| g),
                        );
                        present_message(
                            &obj_up,
                            "Cannot Create Thread",
                            &left_behind_said(&all_guards, &e.to_string()),
                        );
                        return;
                    }
                };
                let baseline_ref = made
                    .as_ref()
                    .map(|m| m.baseline.clone())
                    .or_else(|| shared_baseline.clone());
                let link = ForkLink {
                    base_tree: baseline_ref.as_ref().map(|b| b.tree.clone()),
                    base_commit: baseline_ref.as_ref().map(|b| b.commit.clone()),
                    not_compared: made
                        .as_ref()
                        .map(|m| m.from_head.clone())
                        .unwrap_or_default(),
                    ..ForkLink::new(&source_thread, fork_seq)
                };
                let worktree_info = made.as_ref().map(|m| m.info.clone());
                let cwd = made
                    .as_ref()
                    .map(|m| m.dir.to_string_lossy().into_owned())
                    .unwrap_or_else(|| dir.clone());
                let thread_title = Some(format!("{} — {}", choice.label, display_title(&title)));
                let handoff_spec = carries_handoff.then(|| (handoff.clone(), source_label.clone()));

                let mut spec = NewThreadSpec::new(&cwd, choice.driver);
                spec.title = thread_title.clone();
                spec.model = choice.model.clone();
                // A shared checkout is answer-only. Isolated worktrees carry the safe mode.
                let mode = if choice.worktree { settings.mode } else { Some(agent_core::adapter::Mode::Plan) };
                spec.mode = mode;
                spec.handoff = handoff_spec;
                spec.attributes = agent_kit::store::ThreadAttributes {
                    profile_id: choice.profile_id.clone(),
                    worktree: worktree_info.clone(),
                    fork: Some(link.clone()),
                };

                participants.push(CompareParticipant {
                    thread_id: ids.thread.clone(),
                    label: choice.label,
                    driver: choice.driver,
                    profile_id: choice.profile_id.clone(),
                    model: choice.model.clone(),
                    after_seq: 0,
                });

                created_list.push((
                    ids.thread.clone(),
                    CreatedThread {
                        notice: Some(CreationNotice::Fork {
                            link,
                            baseline_in: baseline_ref.map(|b| b.toplevel),
                        }),
                        driver: choice.driver,
                        prompt: Some(prompt.clone()),
                        effort: choice.effort.or_else(|| settings.effort.clone()),
                        handoff: carries_handoff,
                        title: thread_title,
                        profile_id: choice.profile_id,
                        worktree: worktree_info,
                        mode,
                    },
                    guard,
                ));

                specs.push((ids, spec));
            }

            let group = CompareGroup {
                id: group_id.clone(),
                source_thread: source_thread.clone(),
                prompt_preview: redacted_prompt_preview(&prompt),
                created_at: unix_now_secs(),
                participants,
                kept_thread: None,
            };

            let prompt_for_write = prompt.clone();
            let count = specs.len();
            let group_id_for_done = group_id.clone();
            let write_fn = move |s: &agent_kit::store::Store| {
                s.atomically(|s| {
                    let mut group_to_save = group;
                    for ((ids, spec), part) in
                        specs.iter().zip(group_to_save.participants.iter_mut())
                    {
                        record_new_thread_as(s, ids, spec)?;
                        save_pending_prompt(s, &ids.thread, &prompt_for_write)?;
                        part.after_seq = s.max_seq(&ids.thread)?;
                    }
                    s.save_compare_group(&group_to_save)
                })
            };

            let finish =
                move |imp: &AgentTerminalWindow, outcome: Option<agent_kit::store::Result<()>>| {
                    let guards: Vec<LeftBehind> = Vec::new();
                    let _ = &guards;
                    match outcome {
                        Some(Ok(())) => {
                            if let Some(owner) = super::threads::thread_owner(&source_thread) {
                                if let Some(view) = owner.imp().with_chat(&source_thread, |c| c.view.clone()).flatten() {
                                    if source_draft.as_ref().is_some_and(|draft| !draft.1 && *draft == view.composer_schedule_draft()) {
                                        view.clear_composer_draft();
                                    }
                                }
                            }
                            info!(
                                "Started Ask Several group {group_id_for_done} with {count} participant(s)"
                            );
                            for (thread_id, created, mut left_behind) in created_list {
                                if imp.open_created_quiet(&thread_id, created).is_some() {
                                    left_behind.disarm();
                                }
                            }
                            imp.show_toast(&format!("Asking {count} agents in parallel"));
                            imp.open_group_compare(&group_id_for_done);
                        }
                        Some(Err(e)) => {
                            let held: Vec<LeftBehind> =
                                created_list.into_iter().map(|(_, _, g)| g).collect();
                            present_message(
                                &imp.obj(),
                                "Cannot Launch Ask Several",
                                &left_behind_said(&held, &e.to_string()),
                            );
                        }
                        None => {
                            let held: Vec<LeftBehind> =
                                created_list.into_iter().map(|(_, _, g)| g).collect();
                            present_message(
                                &imp.obj(),
                                "Cannot Launch Ask Several",
                                &left_behind_said(&held, "The thread store did not answer in time."),
                            );
                        }
                    }
                };

            if store.writer().is_none() {
                let res = store.reader().atomically(write_fn);
                finish(imp, Some(res));
            } else {
                imp.write_store_then("ask several agents", write_fn, finish);
            }
        });
    }

    /// Context menu action "Compare Group…" on `thread` (whether source or participant).
    pub(super) fn open_compare_group_for(&self, thread: &str) {
        let obj = self.obj().downgrade();
        let thread = thread.to_owned();
        glib::MainContext::default().spawn_local(async move {
            let found = store_job({
                let thread = thread.clone();
                move |store| -> agent_kit::store::Result<Vec<CompareGroup>> {
                    if let Some(group) = store.compare_group_for_thread(&thread)? {
                        return Ok(vec![group]);
                    }
                    store.compare_groups_for_source(&thread)
                }
            })
            .await;
            let Some(obj) = obj.upgrade() else { return };
            let imp = obj.imp();
            if imp.is_closing() {
                return;
            }
            match found {
                Some(Ok(mut groups)) if groups.len() == 1 => {
                    imp.open_group_compare(&groups.remove(0).id);
                }
                Some(Ok(groups)) if !groups.is_empty() => imp.pick_compare_group(groups),
                Some(Ok(_)) => imp.show_toast("This thread has no multi-agent comparison yet"),
                Some(Err(e)) => {
                    present_message(&obj, "Cannot Compare Agents", &redact(&e.to_string()));
                }
                None => imp.show_toast("Cannot read the thread"),
            }
        });
    }

    fn pick_compare_group(&self, groups: Vec<CompareGroup>) {
        let dialog = confirm_dialog(
            "Compare Agents",
            "Choose the multi-agent run to open.",
            ("cancel", "Cancel"),
            ("compare", "Compare"),
            adw::ResponseAppearance::Suggested,
            true,
        );
        let labels: Vec<String> = groups
            .iter()
            .map(|g| {
                let names: Vec<&str> = g.participants.iter().map(|p| p.label.as_str()).collect();
                format!("{} — {}", names.join(", "), g.prompt_preview)
            })
            .collect();
        let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let choice = adw::ComboRow::builder()
            .title("Run")
            .model(&gtk4::StringList::new(&label_refs))
            .build();
        let list = gtk4::ListBox::builder()
            .selection_mode(gtk4::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        list.append(&choice);
        dialog.set_extra_child(Some(&list));
        self.ask(dialog, "compare", move |imp, go| {
            if !go {
                return;
            }
            if let Some(group) = usize::try_from(choice.selected())
                .ok()
                .and_then(|i| groups.get(i))
            {
                imp.open_group_compare(&group.id);
            }
        });
    }

    /// Opens [`CompareWindow`] on `group_id` with Refresh and Keep This One wired up.
    pub(super) fn open_group_compare(&self, group_id: &str) {
        let window = CompareWindow::new(self.obj().upcast_ref(), "Compare Agents");
        let gid = group_id.to_owned();
        load_group_compare(&window, gid.clone());
        let gid_r = gid.clone();
        window.connect_refresh(move |w| load_group_compare(w, gid_r.clone()));
        let obj = self.obj().downgrade();
        window.connect_keep(move |w, column| {
            let Some(obj) = obj.upgrade() else { return };
            let weak = w.downgrade();
            let gid_k = gid.clone();
            let reread = Reread::new(move || {
                if let Some(w) = weak.upgrade() {
                    load_group_compare(&w, gid_k);
                }
            });
            obj.imp().confirm_keep_group(w, gid.clone(), column, reread);
        });
        window.window().present();
    }

    fn confirm_keep_group(
        &self,
        over: &CompareWindow,
        group_id: String,
        column: usize,
        reread: Reread,
    ) {
        let obj = self.obj().downgrade();
        let parent_weak = over.window().downgrade();
        glib::MainContext::default().spawn_local(async move {
            let read = store_job({
                let gid = group_id.clone();
                move |store| -> agent_kit::store::Result<Option<(CompareGroup, Vec<CompareColumn>)>> {
                    let Some(group) = store.compare_group(&gid)? else {
                        return Ok(None);
                    };
                    let cols = store.compare_columns(&group.sides())?;
                    Ok(Some((group, cols)))
                }
            })
            .await;
            let (Some(obj), Some(parent)) = (obj.upgrade(), parent_weak.upgrade()) else {
                reread.now();
                return;
            };
            if obj.imp().is_closing() {
                reread.now();
                return;
            }
            let Some(Ok(Some((group, cols)))) = read else {
                reread.now();
                obj.imp().show_toast("Comparison group no longer exists");
                return;
            };
            if group.kept_thread.is_some() {
                reread.now();
                obj.imp().show_toast("Another answer in this group was already kept");
                return;
            }
            let Some((kept_part, kept_col)) = group
                .participants
                .get(column)
                .cloned()
                .zip(cols.get(column))
            else {
                reread.now();
                return;
            };
            let others_count = group
                .participants
                .iter()
                .enumerate()
                .filter(|(i, p)| {
                    *i != column
                        && cols
                            .get(*i)
                            .and_then(|c| c.summary.as_ref())
                            .is_some_and(|s| !s.archived)
                        && !p.thread_id.is_empty()
                })
                .count();
            let close_losses: Vec<(String, super::threads::CloseReasons)> = group
                .participants
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != column)
                .filter_map(|(_, p)| {
                    let key = RowKey::Thread(p.thread_id.clone());
                    let reasons = super::threads::thread_owner(&p.thread_id)
                        .and_then(|w| w.imp().row_close_reasons(&key))
                        .or_else(|| obj.imp().row_close_reasons(&key))?;
                    Some((p.label.clone(), reasons))
                })
                .collect();
            let extra_losses = super::threads::window_close_question(&close_losses)
                .map(|(_, body, _)| format!("\n\n{body}"))
                .unwrap_or_default();
            let dialog = confirm_dialog(
                &format!("Keep “{}”?", kept_part.label),
                &format!(
                    "The other {others_count} participant thread(s) will be archived (never deleted), \
                     cleanly finished worktrees can be removed, and a summary of this answer will be \
                     returned to the source thread.{extra_losses}"
                ),
                ("cancel", "Cancel"),
                ("keep", "Keep This One"),
                adw::ResponseAppearance::Suggested,
                false,
            );
            let reply_text = last_reply_text(kept_col);
            let obj_weak = obj.downgrade();
            drop(obj);
            let confirmed = crate::dialogs::confirmed(&dialog, &parent, "keep").await;
            drop(parent);
            if !confirmed {
                reread.now();
                return;
            }
            let Some(obj) = obj_weak.upgrade() else {
                reread.now();
                return;
            };
            if obj.imp().is_closing() {
                reread.now();
                return;
            }
            obj.imp()
                .apply_keep_group(group, kept_part, reply_text, reread);
        });
    }

    fn apply_keep_group(
        &self,
        group: CompareGroup,
        kept: CompareParticipant,
        _reply_text: String,
        reread: Reread,
    ) {
        let source_owner = super::threads::thread_owner(&group.source_thread);
        if super::threads::thread_owner(&kept.thread_id)
            .is_some_and(|w| w.imp().row_is_busy(&RowKey::Thread(kept.thread_id.clone())))
        {
            self.show_toast("Wait for the chosen participant to finish before keeping its answer");
            reread.now();
            return;
        }
        if source_owner.as_ref().is_some_and(|w| {
            w.imp()
                .with_chat(
                    &group.source_thread,
                    super::threads::ChatTab::answer_source_blocked,
                )
                .unwrap_or(false)
        }) {
            self.show_toast("Wait for the source thread to finish before keeping an answer");
            reread.now();
            return;
        }
        let source_session = source_owner
            .as_ref()
            .and_then(|w| w.imp().slot_of(&group.source_thread))
            .and_then(|slot| slot.get());
        let Some(source_lock) = super::threads::reserve_answer_source(&group.source_thread) else {
            self.show_toast("Another comparison action is already saving for this source");
            reread.now();
            return;
        };
        let reservation = match source_session
            .as_ref()
            .map(|s| s.reserve_answer())
            .transpose()
        {
            Ok(r) => r,
            Err(why) => {
                self.show_toast(why);
                reread.now();
                return;
            }
        };
        let weak = self.obj().downgrade();
        glib::MainContext::default().spawn_local(async move {
            let _source_lock = source_lock;
            let read = store_job({
                let source = group.source_thread.clone();
                let part = kept.clone();
                move |s| -> agent_kit::store::Result<_> {
                    let own = s
                        .transcript_items(&part.thread_id, Default::default())?
                        .into_iter()
                        .filter(|i| i.first_seq > part.after_seq)
                        .map(|i| i.message)
                        .collect::<Vec<_>>();
                    let handoff = build_answer_handoff(&own, &part.label);
                    let branch = s
                        .thread_attributes(&part.thread_id)?
                        .worktree
                        .map(|w| w.branch);
                    Ok((handoff, s.max_seq(&source)?, branch))
                }
            })
            .await;
            let Some(obj) = weak.upgrade() else { return };
            let imp = obj.imp();
            let Some(Ok((handoff, source_seq, branch))) = read else {
                imp.show_toast("Could not read the chosen answer");
                reread.now();
                return;
            };
            if handoff.carried == 0 {
                imp.show_toast("This participant has no answer to keep yet");
                reread.now();
                return;
            }
            // Recheck the current identity immediately before queueing the write. The
            // identity lock prevents a closed source from opening during either await.
            let reservation = if reservation.is_none() {
                let current = super::threads::thread_owner(&group.source_thread)
                    .and_then(|w| w.imp().slot_of(&group.source_thread))
                    .and_then(|s| s.get());
                match current.as_ref().map(|s| s.reserve_answer()).transpose() {
                    Ok(reservation) => reservation,
                    Err(why) => {
                        imp.show_toast(why);
                        reread.now();
                        return;
                    }
                }
            } else {
                reservation
            };
            let mut brief = format!(
                "Kept {}'s answer ({} of {} messages); it goes with your next message.",
                kept.label, handoff.carried, handoff.total
            );
            if let Some(branch) = branch {
                brief.push_str(&format!(" Edits remain on branch {branch}."));
            }
            let brief = redact(&brief);
            let mut events = seeded_handoff_events(&handoff, &kept.label);
            if let Some(last) = events.last_mut() {
                *last = Envelope::new(Event::Notice {
                    text: brief.clone(),
                });
            }
            let gid = group.id.clone();
            let kept_id = kept.thread_id.clone();
            let job = move |s: &agent_kit::store::Store| {
                s.keep_compare_group_answer(&gid, &kept_id, source_seq, &events)
            };
            // A Keep must use the actual receipt: releasing the source on a timeout could
            // send its next prompt without the handoff that commits a moment later.
            drop(obj);
            let store = app_handle();
            let outcome = if let Some(writer) = store.writer() {
                let receipt = writer.call_result("keep answer from group", job);
                gtk4::gio::spawn_blocking(move || receipt.recv().ok())
                    .await
                    .ok()
                    .flatten()
            } else {
                store_job(job).await
            };
            // The committed source/session state belongs to the source window, even when
            // the window that initiated Keep disappeared while the writer was busy.
            if let Some(Ok(KeepGroupOutcome::Kept(done))) = &outcome {
                if let Some(reservation) = &reservation {
                    reservation.adopt(handoff.clone(), &brief);
                } else if let Some(owner) = super::threads::thread_owner(&group.source_thread) {
                    owner
                        .imp()
                        .restore_saved_answer(&group.source_thread, handoff.clone(), &brief);
                }
                for other in &done.archived {
                    if let Some(owner) = super::threads::thread_owner(other) {
                        owner.imp().close_row_for_keep(other);
                    }
                }
            }
            let Some(obj) = weak.upgrade() else {
                reread.now();
                return;
            };
            let imp = obj.imp();
            match outcome {
                Some(Ok(KeepGroupOutcome::Kept(done))) => {
                    imp.reload_summaries();
                    reread.now();
                    let keep_done = Rc::new(GroupKeepDone {
                        group_id: group.id,
                        kept: kept.label.clone(),
                        kept_thread: kept.thread_id,
                        source_thread: group.source_thread,
                        handoff_fence: handoff.fence.clone(),
                        archived: done.archived,
                        undone: Cell::new(false),
                        removal: RefCell::new(Vec::new()),
                    });
                    let held = keep_done.clone();
                    let undo = imp.held_action_toast(
                        &format!(
                            "Kept “{}” and returned summary to the source thread",
                            keep_done.kept
                        ),
                        "Undo",
                        10,
                        adw::ToastPriority::Normal,
                        move |imp| imp.undo_keep_group(&held),
                    );
                    drop(undo);
                    for (other_id, info) in done.worktrees {
                        imp.offer_archived_group_worktree_removal(&keep_done, info, other_id);
                    }
                }
                Some(Ok(KeepGroupOutcome::Refused(why))) => {
                    reread.now();
                    imp.show_toast(&format!("Nothing was kept: {why}"));
                }
                Some(Err(e)) => {
                    reread.now();
                    present_message(
                        &imp.obj(),
                        "Could Not Keep This Answer",
                        &redact(&e.to_string()),
                    );
                }
                None => {
                    reread.now();
                    present_message(
                        &imp.obj(),
                        "Could Not Keep This Answer",
                        "The thread store did not answer in time.",
                    );
                }
            }
        });
    }

    fn undo_keep_group(&self, keep: &Rc<GroupKeepDone>) {
        if keep.undone.replace(true) {
            return;
        }
        for offer in keep.removal.borrow_mut().drain(..) {
            offer.dismiss();
        }
        let gid = keep.group_id.clone();
        let archived = keep.archived.clone();
        let kept = keep.kept_thread.clone();
        let source = keep.source_thread.clone();
        let fence = keep.handoff_fence.clone();
        let Some(source_lock) = super::threads::reserve_answer_source(&source) else {
            keep.undone.set(false);
            self.show_toast("Another comparison action is saving; try again once it finishes");
            return;
        };
        let preparing = super::threads::thread_owner(&source).is_some_and(|w| {
            w.imp()
                .with_chat(&source, super::threads::ChatTab::answer_preparing)
                .unwrap_or(false)
        });
        let session = super::threads::thread_owner(&source)
            .and_then(|w| w.imp().slot_of(&source))
            .and_then(|s| s.get());
        // A preparation may already hold an older history snapshot. Retain its source
        // context rather than withdraw it only to have that snapshot restore it later.
        let reservation = session
            .as_ref()
            .filter(|_| !preparing)
            .and_then(|s| s.reserve_answer_withdrawal(&fence));
        let withdraw = !preparing && (session.is_none() || reservation.is_some());
        let keep = keep.clone();
        let weak = self.obj().downgrade();
        glib::MainContext::default().spawn_local(async move {
            let _source_lock = source_lock;
            let (source_write, fence_write) = (source.clone(), fence.clone());
            let job = move |s: &agent_kit::store::Store| s.undo_compare_group_answer(
                &gid, &archived, &kept, &source_write, &fence_write, withdraw);
            let store = app_handle();
            let outcome = if let Some(writer) = store.writer() {
                let receipt = writer.call_result("undo comparison choice", job);
                gtk4::gio::spawn_blocking(move || receipt.recv().ok()).await.ok().flatten()
            } else { store_job(job).await };
            if !matches!(outcome, Some(Ok(_))) { keep.undone.set(false); }
            // Withdrawal is receipt-dependent, not dependent on the initiating window.
            if matches!(outcome, Some(Ok(true))) {
                if let Some(reserved) = &reservation { reserved.withdraw(&fence); }
                if let Some(owner) = super::threads::thread_owner(&source) {
                    owner.imp().withdraw_saved_answer(&source, &fence);
                }
            }
            let Some(obj) = weak.upgrade() else { return };
            let imp = obj.imp();
            match outcome {
                Some(Ok(cleared)) => {
                    imp.reload_summaries();
                    imp.show_toast(if cleared { "Comparison restored; the unsent chosen context was withdrawn" }
                        else { "Participants restored; context already sent, consumed, preparing or busy remains in the source" });
                }
                Some(Err(e)) => present_message(&imp.obj(), "Could Not Restore Group", &redact(&e.to_string())),
                None => present_message(&imp.obj(), "Could Not Restore Group", "The thread store stopped before answering."),
            }
        });
    }

    fn offer_archived_group_worktree_removal(
        &self,
        keep: &Rc<GroupKeepDone>,
        info: crate::worktree::WorktreeInfo,
        other: String,
    ) {
        let dirs = self.open_tab_dirs();
        let obj = self.obj().downgrade();
        let keep_weak = Rc::downgrade(keep);
        glib::MainContext::default().spawn_local(async move {
            let checked = info.clone();
            let other_for_store = other.clone();
            let blocker = store_job(move |store| -> Result<Option<String>, String> {
                if crate::worktree::any_inside(&dirs, &checked.path) {
                    return Ok(Some("a tab is open in it".to_owned()));
                }
                if let Some(who) = store
                    .worktree_occupant(&checked.path, &other_for_store)
                    .map_err(|e| format!("the thread store could not say who is in it: {e}"))?
                {
                    return Ok(Some(format!("{who} works in it")));
                }
                Ok(crate::worktree::removal_blocker(&checked))
            })
            .await
            .unwrap_or_else(|| Err("the thread store could not be read".to_owned()));
            let Some(obj) = obj.upgrade() else { return };
            let imp = obj.imp();
            if imp.is_closing() {
                return;
            }
            let Some(keep) = keep_weak.upgrade().filter(|k| !k.undone.get()) else {
                return;
            };
            if let Ok(None) = blocker {
                let offer = imp.held_action_toast(
                    &format!("Worktree ⎇ {} is clean", info.branch),
                    "Remove",
                    10,
                    adw::ToastPriority::Normal,
                    move |imp| imp.remove_kept_worktree(info.clone(), other.clone()),
                );
                keep.removal.borrow_mut().push(offer);
            }
        });
    }
}

struct GroupCompareRead {
    group: CompareGroup,
    cols: Vec<CompareColumn>,
    change_inputs: Vec<(Option<String>, Option<String>, Vec<String>)>,
}

fn load_group_compare(window: &CompareWindow, group_id: String) {
    let generation = window.begin();
    let changes_gen = window.begin_changes();
    let weak = window.downgrade();
    glib::MainContext::default().spawn_local(async move {
        let read = store_job({
            let gid = group_id.clone();
            move |store| -> Result<GroupCompareRead, String> {
                let group = store
                    .compare_group(&gid)
                    .map_err(|e| redact(&e.to_string()))?
                    .ok_or_else(|| "Comparison group no longer exists.".to_owned())?;
                let cols = store
                    .compare_columns(&group.sides())
                    .map_err(|e| redact(&e.to_string()))?;
                let mut change_inputs = Vec::with_capacity(group.participants.len());
                for (p, col) in group.participants.iter().zip(cols.iter()) {
                    let link = store
                        .fork_link(&p.thread_id)
                        .map_err(|e| redact(&e.to_string()))?;
                    let dir = col.summary.as_ref().map(|s| s.cwd.clone());
                    let base = link.as_ref().and_then(|l| l.base_tree.clone());
                    let skip = link.map(|l| l.not_compared).unwrap_or_default();
                    change_inputs.push((dir, base, skip));
                }
                Ok(GroupCompareRead {
                    group,
                    cols,
                    change_inputs,
                })
            }
        })
        .await;

        let Some(window) = weak.upgrade() else { return };
        let GroupCompareRead {
            group,
            cols,
            change_inputs,
        } = match read {
            Some(Ok(v)) => v,
            Some(Err(e)) => {
                window.show_error(generation, &e);
                window.show_changes_error(changes_gen, &e);
                return;
            }
            None => {
                window.show_error(generation, "The thread store did not answer in time.");
                window.show_changes_error(changes_gen, "The thread store did not answer in time.");
                return;
            }
        };

        let headings: Vec<String> = group.participants.iter().map(|p| p.label.clone()).collect();
        let all_archived_check = |c: &CompareColumn| c.summary.as_ref().is_some_and(|s| s.archived);
        let settled = group.kept_thread.is_some();
        let column_views: Vec<ColumnView> = group
            .participants
            .iter()
            .zip(cols)
            .map(|(p, col)| {
                let keep = if settled {
                    if group.kept_thread.as_deref() == Some(p.thread_id.as_str()) {
                        KeepState::Kept
                    } else if all_archived_check(&col) {
                        KeepState::Archived
                    } else {
                        KeepState::None
                    }
                } else if col.summary.as_ref().is_some_and(|s| !s.archived) {
                    KeepState::Offered
                } else if all_archived_check(&col) {
                    KeepState::Archived
                } else {
                    KeepState::None
                };
                participant_column_view(p, col, keep)
            })
            .collect();

        window.show(
            generation,
            &CompareContent {
                subtitle: format!(
                    "Asking {} agents: “{}”",
                    column_views.len(),
                    group.prompt_preview
                ),
                columns: column_views,
                empty: "None of the agents has replied yet.".to_owned(),
            },
        );
        // Drop strong window handle before awaiting git work so closing the window releases it.
        drop(window);

        let changes = off_main("reading group file changes", move || {
            let max_not_compared = change_inputs
                .iter()
                .map(|(_, _, skip)| skip.len())
                .max()
                .unwrap_or(0);
            let mut sides = Vec::with_capacity(change_inputs.len());
            let mut toplevels: Vec<Option<PathBuf>> = Vec::with_capacity(change_inputs.len());
            for (dir, base, skip) in change_inputs {
                let c = match dir.as_deref() {
                    None => ForkChanges::Unavailable("This thread no longer exists".to_owned()),
                    Some(d) => crate::git::fork_changes(Path::new(d), base.as_deref(), &skip)
                        .unwrap_or_else(|e| ForkChanges::Unavailable(redact(&e))),
                };
                let top = match &c {
                    ForkChanges::Ready { toplevel, .. } => Some(toplevel.clone()),
                    _ => None,
                };
                toplevels.push(top);
                sides.push(c);
            }
            // Shared if any two ready participants point at the same working tree.
            let mut seen = Vec::new();
            let mut shared = false;
            for top in toplevels.into_iter().flatten() {
                if seen.contains(&top) {
                    shared = true;
                    break;
                }
                seen.push(top);
            }
            Ok((sides, shared, max_not_compared))
        })
        .await;

        let Some(window) = weak.upgrade() else { return };
        let (sides, shared, not_compared) = match changes {
            Ok(v) => v,
            Err(e) => {
                window.show_changes_error(changes_gen, &e);
                return;
            }
        };
        let state_of = |c: ForkChanges| match c {
            ForkChanges::NotRepo => ChangesState::NotRepo,
            ForkChanges::Unavailable(why) => ChangesState::Unavailable(why),
            ForkChanges::Ready { diff, .. } => ChangesState::Ready(diff),
        };
        let notes: Vec<String> = shared
            .then(|| SHARED_FOLDER.to_owned())
            .into_iter()
            .chain(not_compared_note(not_compared))
            .collect();
        let columns = sides
            .into_iter()
            .enumerate()
            .map(|(i, s)| ChangesColumn {
                heading: headings.get(i).cloned().unwrap_or_default(),
                state: state_of(s),
            })
            .collect();
        window.show_changes(
            changes_gen,
            &ChangesContent {
                note: (!notes.is_empty()).then(|| notes.join("\n\n")),
                shared,
                columns,
            },
        );
    });
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    struct NoAgent;
    impl crate::chat::ChatBackend for NoAgent {
        fn send_prompt(&self, _: &str) {
            panic!("comparison UI must not send");
        }
        fn interrupt(&self) {}
        fn respond_approval(&self, _: &str, _: agent_core::event::Decision) {}
        fn answer_questions(&self, _: &str, _: serde_json::Value) {}
        fn switch(&self, _: Driver, _: Option<String>, _: Option<String>) {}
        fn reload_session(&self) {}
        fn set_mode(&self, _: agent_core::adapter::Mode) {}
        fn control(&self, _: agent_core::adapter::Control) -> String {
            String::new()
        }
        fn status(&self) -> crate::chat::SessionStatus {
            use agent_core::adapter::Adapter;
            crate::chat::SessionStatus {
                driver: Driver::Claude,
                model: None,
                pending_model: None,
                effort: None,
                mode: agent_core::adapter::Mode::Plan,
                running_turn: false,
                alive: false,
                capabilities: agent_core::claude::ClaudeAdapter::new()
                    .capabilities()
                    .clone(),
                commands: Vec::new(),
                images_refused: None,
                remote: false,
            }
        }
    }

    #[test]
    fn ask_several_gtk_source_strip_and_persisted_keep() {
        crate::testutil::gtk_init().expect("GTK");
        let app = adw::Application::builder()
            .application_id("org.test.AskSeveral")
            .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gtk4::gio::Cancellable>).unwrap();
        let window = crate::window::AgentTerminalWindow::new(&app);
        super::super::threads::tests::ask_several_preparation_reservations(&window);
        gtk_checks(&window);
        window.present();
        let imp = window.imp();
        let options: Vec<_> = [Driver::Claude, Driver::Agy]
            .into_iter()
            .map(|driver| TargetOption {
                label: driver.info().label.into(),
                driver,
                profile_id: None,
                model: None,
                effort: None,
            })
            .collect();
        imp.present_ask_several_dialog(
            "source".into(),
            options,
            Some("Compare approaches".into()),
            true,
            true,
            Some(100),
        );
        let ctx = glib::MainContext::default();
        assert!(crate::testutil::pump_until(&ctx, 5, || window
            .visible_dialog()
            .is_some()));
        let dialog = window
            .visible_dialog()
            .unwrap()
            .downcast::<adw::AlertDialog>()
            .unwrap();
        assert!(crate::testutil::pump_until_dialog_open(
            &ctx,
            dialog.upcast_ref(),
            5
        ));
        assert!(
            !dialog.is_response_enabled("ask"),
            "agy needs explicit acknowledgement"
        );
        let checks: Vec<gtk4::CheckButton> = crate::testutil::find_all(dialog.upcast_ref());
        let ack = checks
            .iter()
            .find(|c| c.label().is_some_and(|s| s.contains("forced permissive")))
            .unwrap();
        ack.set_active(true);
        assert!(
            dialog.is_response_enabled("ask"),
            "acknowledged agy in its worktree is allowed"
        );
        crate::account_status::AccountStatus::shared().observe(
            Driver::Claude,
            &Envelope::new(Event::QuotaUpdated {
                account: None,
                windows: vec![agent_core::event::QuotaWindow {
                    group: None,
                    label: "5-hour".into(),
                    used: 1.0,
                    resets_at: None,
                }],
            }),
        );
        assert!(
            !dialog.is_response_enabled("ask"),
            "a fresh exhausted reading needs confirmation"
        );
        checks
            .iter()
            .find(|c| c.label().as_deref() == Some("Ask exhausted accounts anyway"))
            .unwrap()
            .set_active(true);
        assert!(dialog.is_response_enabled("ask"));
        dialog.close();
        crate::testutil::settle(&ctx);
        window.destroy();
    }

    #[test]
    fn prompt_preview_redacts_before_truncation() {
        // Put a token starting before char 120 and spanning across char 120.
        let prefix = "x".repeat(110);
        let raw = format!("{prefix} ghp_1234567890abcdefghijklmnopqrstuvwx tail");
        let preview = redacted_prompt_preview(&raw);
        assert!(
            !preview.contains("ghp_"),
            "partial unredacted secret leaked in preview: {preview}"
        );
    }

    #[test]
    fn duplicate_target_labels_are_disambiguated() {
        let mut specs = vec![
            TargetSpec {
                label: "Claude Code".into(),
                driver: Driver::Claude,
                profile_id: None,
                model: None,
                effort: None,
                worktree: false,
            },
            TargetSpec {
                label: "Claude Code".into(),
                driver: Driver::Claude,
                profile_id: None,
                model: None,
                effort: None,
                worktree: true,
            },
            TargetSpec {
                label: "OpenAI Codex".into(),
                driver: Driver::Codex,
                profile_id: None,
                model: None,
                effort: None,
                worktree: false,
            },
        ];
        disambiguate_labels(&mut specs);
        assert_eq!(specs[0].label, "Claude Code (1)");
        assert_eq!(specs[1].label, "Claude Code (2)");
        assert_eq!(specs[2].label, "OpenAI Codex");
    }

    #[test]
    fn default_indices_pick_distinct_drivers_when_available() {
        let opts = vec![
            TargetOption {
                label: "Claude".into(),
                driver: Driver::Claude,
                profile_id: None,
                model: None,
                effort: None,
            },
            TargetOption {
                label: "Claude Opus".into(),
                driver: Driver::Claude,
                profile_id: None,
                model: Some("opus".into()),
                effort: None,
            },
            TargetOption {
                label: "Codex".into(),
                driver: Driver::Codex,
                profile_id: None,
                model: None,
                effort: None,
            },
        ];
        assert_eq!(default_option_indices(&opts), vec![0, 2]);
    }

    pub(in crate::window) fn gtk_checks(window: &crate::window::AgentTerminalWindow) {
        use crate::testutil::pump_until;
        use crate::window::imp::threads::app_store;
        use agent_kit::store::{ProviderRecord, ThreadRecord};

        let imp = window.imp();
        let store = app_store();
        let dir = tempfile::tempdir().expect("dir");
        let cwd = dir.path().to_string_lossy().into_owned();
        let src = store.new_id().expect("src");
        let p1 = store.new_id().expect("p1");
        let p2 = store.new_id().expect("p2");

        store
            .create_thread_record(&ThreadRecord {
                title: Some("Source ask thread"),
                provider: Some(ProviderRecord {
                    id: None,
                    driver: Driver::Claude.info().key,
                    model: None,
                    native_id: None,
                }),
                ..ThreadRecord::new(&src, &cwd)
            })
            .expect("create source");
        for (id, driver, reply) in [
            (&p1, Driver::Claude, "Claude solution"),
            (&p2, Driver::Codex, "Codex solution"),
        ] {
            store
                .create_thread_record(&ThreadRecord {
                    title: Some(reply),
                    provider: Some(ProviderRecord {
                        id: None,
                        driver: driver.info().key,
                        model: None,
                        native_id: None,
                    }),
                    ..ThreadRecord::new(id, &cwd)
                })
                .expect("create participant");
            store
                .append_user_message(id, None, "Which approach?")
                .expect("msg");
            for env in [
                Envelope::item_started(
                    "a1",
                    agent_core::event::ItemKind::AssistantMessage,
                    "",
                    None,
                    None,
                ),
                Envelope::delta("a1", agent_core::event::StreamKind::Assistant, reply),
                Envelope::item_completed(
                    "a1",
                    agent_core::event::ItemStatus::Completed,
                    None,
                    None,
                ),
            ] {
                store.append_event(id, None, &env).expect("reply");
            }
        }

        let group = CompareGroup {
            id: store.new_id().expect("gid"),
            source_thread: src.clone(),
            prompt_preview: "Which approach?".into(),
            created_at: 1_795_000_000,
            participants: vec![
                CompareParticipant {
                    thread_id: p1.clone(),
                    label: "Claude".into(),
                    driver: Driver::Claude,
                    profile_id: None,
                    model: None,
                    after_seq: 0,
                },
                CompareParticipant {
                    thread_id: p2.clone(),
                    label: "Codex".into(),
                    driver: Driver::Codex,
                    profile_id: None,
                    model: None,
                    after_seq: 0,
                },
            ],
            kept_thread: None,
        };
        store.save_compare_group(&group).expect("save group");
        imp.reload_summaries();

        let view = crate::chat::view::ChatView::new(Rc::new(NoAgent));
        let preview = gtk4::Window::builder()
            .default_width(640)
            .default_height(600)
            .child(&view)
            .build();
        imp.attach_compare_strip(&src, &view);
        preview.present();
        fn find_button(widget: &gtk4::Widget, text: &str) -> Option<gtk4::Button> {
            if let Some(button) = widget
                .downcast_ref::<gtk4::Button>()
                .filter(|b| b.label().as_deref() == Some(text))
            {
                return Some(button.clone());
            }
            let mut child = widget.first_child();
            while let Some(w) = child {
                if let Some(b) = find_button(&w, text) {
                    return Some(b);
                }
                child = w.next_sibling();
            }
            None
        }
        let ctx = glib::MainContext::default();
        assert!(
            pump_until(&ctx, 5, || find_button(
                view.upcast_ref(),
                "Keep this thread instead"
            )
            .is_some()),
            "source shelf offers detach"
        );
        for label in ["Open", "Keep", "Discard", "Compare"] {
            assert!(
                find_button(view.upcast_ref(), label).is_some(),
                "shelf has {label}"
            );
        }
        crate::testutil::settle(&ctx);
        crate::testutil::capture_window(&preview, "ask-several-source-strip");

        let cols = store.compare_columns(&group.sides()).expect("cols");
        assert_eq!(cols.len(), 2);
        assert_eq!(last_reply_text(&cols[0]), "Claude solution");
        assert_eq!(last_reply_text(&cols[1]), "Codex solution");

        // No completed assistant output after the comparison point: the prompt alone is
        // history, never an answer, and must not settle or archive anything.
        let mut empty = group.clone();
        empty.id = store.new_id().unwrap();
        for p in &mut empty.participants {
            p.after_seq = store.max_seq(&p.thread_id).unwrap();
        }
        store
            .append_user_message(&p1, None, "Prompt with no answer")
            .unwrap();
        store.save_compare_group(&empty).unwrap();
        let refused = Rc::new(Cell::new(false));
        let flag = refused.clone();
        imp.apply_keep_group(
            empty.clone(),
            empty.participants[0].clone(),
            String::new(),
            Reread::new(move || flag.set(true)),
        );
        assert!(
            imp.open_thread(&src, true).is_none(),
            "a closed source is reserved through the preparation read"
        );
        assert!(pump_until(&ctx, 5, || refused.get()));
        assert!(store
            .compare_group(&empty.id)
            .unwrap()
            .unwrap()
            .kept_thread
            .is_none());
        assert!(!store.thread_summary(&p2).unwrap().unwrap().archived);
        store
            .delete_meta(&agent_core::ask_several::group_meta_key(&empty.id))
            .unwrap();
        store.save_compare_group(&group).unwrap();

        let reread_ran = Rc::new(Cell::new(false));
        let flag = reread_ran.clone();
        let reread = Reread::new(move || flag.set(true));
        imp.apply_keep_group(
            group.clone(),
            group.participants[0].clone(),
            "Claude solution".into(),
            reread,
        );
        assert!(
            imp.open_thread(&src, true).is_none(),
            "source stays closed while Keep is pending"
        );
        let ctx = glib::MainContext::default();
        assert!(pump_until(&ctx, 5, || {
            store
                .compare_group(&group.id)
                .ok()
                .flatten()
                .and_then(|g| g.kept_thread)
                .is_some()
        }));
        assert!(reread_ran.get(), "Reread fired");
        assert!(
            store
                .thread_summary(&p2)
                .expect("p2 summary")
                .expect("exists")
                .archived,
            "unkept participant must be archived"
        );
        assert!(
            !store
                .thread_summary(&p1)
                .expect("p1 summary")
                .expect("exists")
                .archived,
            "kept participant remains active"
        );
        let src_events = store.events(&src, None, 100).expect("src events");
        assert!(
            src_events.iter().any(|(_, e)| matches!(
                &e.event,
                Event::HandoffSeeded { summary, .. } if summary.contains("Claude solution")
            )),
            "kept brief must be returned to source thread"
        );
        assert!(
            crate::chat::session::undelivered_handoff(src_events.iter().map(|(_, e)| e)).is_some(),
            "closed source re-arms the kept handoff after a restart"
        );
        let chosen =
            crate::chat::session::undelivered_handoff(src_events.iter().map(|(_, e)| e)).unwrap();
        let undo = Rc::new(GroupKeepDone {
            group_id: group.id.clone(),
            kept: "Claude".into(),
            kept_thread: p1.clone(),
            source_thread: src.clone(),
            handoff_fence: chosen.fence,
            archived: vec![p2.clone()],
            undone: Cell::new(false),
            removal: RefCell::new(Vec::new()),
        });
        let occupied = super::super::threads::reserve_answer_source(&src).unwrap();
        imp.undo_keep_group(&undo);
        assert!(!undo.undone.get(), "a refused Undo remains retryable");
        drop(occupied);
        imp.undo_keep_group(&undo);
        assert!(pump_until(&ctx, 5, || store
            .compare_group(&group.id)
            .unwrap()
            .unwrap()
            .kept_thread
            .is_none()));
        let undone_events = store.events(&src, None, 100).unwrap();
        assert!(
            crate::chat::session::undelivered_handoff(undone_events.iter().map(|(_, e)| e))
                .is_none(),
            "Undo withdraws a closed source's exact unsent handoff"
        );
        assert!(!store.thread_summary(&p2).unwrap().unwrap().archived);

        imp.apply_keep_group(
            group.clone(),
            group.participants[0].clone(),
            String::new(),
            Reread::new(|| {}),
        );
        assert!(pump_until(&ctx, 5, || store
            .compare_group(&group.id)
            .unwrap()
            .unwrap()
            .kept_thread
            .is_some()));
        let events = store.events(&src, None, 100).unwrap();
        let chosen =
            crate::chat::session::undelivered_handoff(events.iter().map(|(_, e)| e)).unwrap();
        let finish_old_build = super::super::threads::tests::pause_answer_build(&imp.obj(), &src);
        let preparing_undo = Rc::new(GroupKeepDone {
            group_id: group.id.clone(),
            kept: "Claude".into(),
            kept_thread: p1.clone(),
            source_thread: src.clone(),
            handoff_fence: chosen.fence,
            archived: vec![p2.clone()],
            undone: Cell::new(false),
            removal: RefCell::new(Vec::new()),
        });
        imp.undo_keep_group(&preparing_undo);
        assert!(pump_until(&ctx, 5, || {
            !super::super::threads::answer_source_reserved(&src)
        }));
        assert!(!store.thread_summary(&p2).unwrap().unwrap().archived);
        let retained = store.events(&src, None, 100).unwrap();
        assert!(
            crate::chat::session::undelivered_handoff(retained.iter().map(|(_, e)| e)).is_some(),
            "Undo retains context already read by an unfinished build"
        );
        finish_old_build();
        preview.destroy();

        // Clean up temporary test threads so callers inspecting store counts see no residue.
        store.delete_thread(&p1).expect("cleanup p1");
        store.delete_thread(&p2).expect("cleanup p2");
        store.delete_thread(&src).expect("cleanup src");
        imp.reload_summaries();
    }
}
