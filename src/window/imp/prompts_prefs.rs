//! Settings → General → Saved Prompts: the list of saved prompts, each one's name, text and
//! folder, and Add, Duplicate and Delete.
//!
//! Every field applies as it is made: a name or folder when its entry is committed (Enter or
//! focus leaving it), the text when Apply is pressed or focus leaves it. A value that is not valid
//! is shown on its field and never saved. A prompt already in the file that is not valid (a
//! hand edit) is listed with its problem and kept, never dropped; only Delete takes one out.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use agent_core::saved_prompts::{self, SavedPrompt};
use gtk4::glib;

use super::remote::{entry_row, mark_entry, on_commit};
use super::AgentTerminalWindow;
use crate::window::AgentTerminalWindow as Window;

const NAME_HELP: &str = "Typed after / in a thread's prompt: letters, digits, - and _";
const FOLDER_HELP: &str = "Offered only in threads in this folder or inside it; blank: everywhere";
const BODY_HELP: &str = "Put in the prompt for you to read and edit; never sent on its own";

/// What a row's subtitle says of a prompt: its problem when it is not offered, else its scope.
fn row_subtitle(prompt: &SavedPrompt, problem: Option<&str>) -> String {
    match problem {
        Some(problem) => format!("Not offered: {problem}"),
        None => match prompt.folder.as_deref() {
            Some(folder) => format!("Only in {folder}"),
            None => "In every folder".to_owned(),
        },
    }
}

/// Each prompt's problem, as the typeahead judges them.
fn problems_of(prompts: &[SavedPrompt]) -> Vec<Option<String>> {
    saved_prompts::problems(
        prompts,
        agent_core::commands::builtins(),
        crate::utils::expand_tilde,
    )
}

/// The text of `view`.
fn view_text(view: &gtk4::TextView) -> String {
    let buffer = view.buffer();
    let (start, end) = buffer.bounds();
    buffer.text(&start, &end, false).to_string()
}

/// Shows `problem` (or, with none, the help) under a text view, for the eye and a screen reader.
fn mark_view(view: &gtk4::TextView, note: &gtk4::Label, problem: Option<&str>) {
    match problem {
        Some(problem) => {
            view.add_css_class("error");
            note.set_text(problem);
            view.update_property(&[gtk4::accessible::Property::Description(problem)]);
            view.update_state(&[gtk4::accessible::State::Invalid(
                gtk4::AccessibleInvalidState::True,
            )]);
        }
        None => {
            view.remove_css_class("error");
            note.set_text(BODY_HELP);
            view.update_property(&[gtk4::accessible::Property::Description("")]);
            view.update_state(&[gtk4::accessible::State::Invalid(
                gtk4::AccessibleInvalidState::False,
            )]);
        }
    }
}

/// The three fields of a prompt, as an edited row and the new-prompt form both show them.
struct Fields {
    name_row: adw::ActionRow,
    name: gtk4::Entry,
    body_row: gtk4::ListBoxRow,
    body: gtk4::TextView,
    body_note: gtk4::Label,
    apply: gtk4::Button,
    folder_row: adw::ActionRow,
    folder: gtk4::Entry,
    choose: gtk4::Button,
    everywhere: gtk4::Button,
}

impl Fields {
    /// The fields filled from `prompt`. `apply_label` names the text's button.
    fn new(prompt: &SavedPrompt, apply_label: &str) -> Self {
        let (name_row, name) = entry_row("Name", NAME_HELP, &prompt.name, "review");

        let body = gtk4::TextView::builder()
            .wrap_mode(gtk4::WrapMode::WordChar)
            .accepts_tab(false)
            .top_margin(6)
            .bottom_margin(6)
            .left_margin(8)
            .right_margin(8)
            .build();
        body.buffer().set_text(&prompt.body);
        body.update_property(&[
            gtk4::accessible::Property::Label("Text"),
            gtk4::accessible::Property::MultiLine(true),
        ]);
        let scroller = gtk4::ScrolledWindow::builder()
            .child(&body)
            .min_content_height(72)
            .max_content_height(240)
            .propagate_natural_height(true)
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .css_classes(["card"])
            .build();
        let title = gtk4::Label::builder()
            .label("Text")
            .xalign(0.0)
            .css_classes(["heading"])
            .build();
        let body_note = gtk4::Label::builder()
            .label(BODY_HELP)
            .xalign(0.0)
            .wrap(true)
            .hexpand(true)
            .css_classes(["dim-label", "caption"])
            .build();
        let apply = gtk4::Button::builder()
            .label("Apply")
            .valign(gtk4::Align::Center)
            .build();
        apply.update_property(&[gtk4::accessible::Property::Label(apply_label)]);
        let foot = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        foot.append(&body_note);
        foot.append(&apply);
        let column = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .spacing(6)
            .margin_top(8)
            .margin_bottom(8)
            .margin_start(12)
            .margin_end(12)
            .build();
        column.append(&title);
        column.append(&scroller);
        column.append(&foot);
        let body_row = gtk4::ListBoxRow::builder()
            .activatable(false)
            .selectable(false)
            .child(&column)
            .build();

        let (folder_row, folder) = entry_row(
            "Only in This Folder",
            FOLDER_HELP,
            prompt.folder.as_deref().unwrap_or(""),
            "every folder",
        );
        let choose = gtk4::Button::builder()
            .label("Choose…")
            .valign(gtk4::Align::Center)
            .tooltip_text("Pick the folder")
            .build();
        choose.update_property(&[gtk4::accessible::Property::Label("Choose the folder")]);
        let everywhere = gtk4::Button::builder()
            .label("Everywhere")
            .valign(gtk4::Align::Center)
            .tooltip_text("Offer it in every folder")
            .build();
        everywhere.update_property(&[gtk4::accessible::Property::Label(
            "Offer it in every folder",
        )]);
        folder_row.add_suffix(&choose);
        folder_row.add_suffix(&everywhere);

        Self {
            name_row,
            name,
            body_row,
            body,
            body_note,
            apply,
            folder_row,
            folder,
            choose,
            everywhere,
        }
    }

    /// The fields a new prompt is read from.
    fn form(&self) -> Form {
        Form {
            name: self.name.clone(),
            name_row: self.name_row.clone(),
            body: self.body.clone(),
            body_note: self.body_note.clone(),
            folder: self.folder.clone(),
            folder_row: self.folder_row.clone(),
        }
    }

    /// Opens a folder chooser over the window the button is in (the Settings dialog's own), a
    /// folder picked goes into the folder entry and `then` runs. The chooser starts where it
    /// likes: looking the typed folder up first would touch the disk on the main thread.
    fn connect_choose(&self, then: impl Fn() + 'static) {
        let folder = self.folder.downgrade();
        let then = Rc::new(then);
        self.choose.connect_clicked(move |button| {
            let Some(entry) = folder.upgrade() else {
                return;
            };
            let parent = button.root().and_downcast::<gtk4::Window>();
            let picker = gtk4::FileDialog::builder()
                .title("Only in This Folder")
                .modal(true)
                .build();
            let then = Rc::clone(&then);
            let entry = entry.downgrade();
            glib::MainContext::default().spawn_local(async move {
                let picked = picker.select_folder_future(parent.as_ref()).await;
                // Dismissing the chooser is a normal outcome, not a failure.
                let Some(path) = picked.ok().and_then(|f| f.path()) else {
                    return;
                };
                if let Some(entry) = entry.upgrade() {
                    entry.set_text(&path.to_string_lossy());
                    then();
                }
            });
        });
    }
}

/// One prompt's row and the controls whose names say which prompt they act on.
struct PromptRow {
    row: adw::ExpanderRow,
    named: Vec<(gtk4::Button, &'static str)>,
    name: gtk4::Entry,
    name_row: adw::ActionRow,
    folder: gtk4::Entry,
    folder_row: adw::ActionRow,
}

impl PromptRow {
    /// Names the row and its controls after `label` (`/name`).
    fn name(&self, label: &str) {
        self.row.set_title(&glib::markup_escape_text(label));
        for (button, verb) in &self.named {
            button.update_property(&[gtk4::accessible::Property::Label(&format!(
                "{verb} {label}"
            ))]);
        }
    }
}

/// The new-prompt form's fields.
struct Form {
    name: gtk4::Entry,
    name_row: adw::ActionRow,
    body: gtk4::TextView,
    body_note: gtk4::Label,
    folder: gtk4::Entry,
    folder_row: adw::ActionRow,
}

/// [`Form`] held weakly, as its own widgets' handlers must hold it.
#[derive(Clone)]
struct WeakForm {
    name: glib::WeakRef<gtk4::Entry>,
    name_row: glib::WeakRef<adw::ActionRow>,
    body: glib::WeakRef<gtk4::TextView>,
    body_note: glib::WeakRef<gtk4::Label>,
    folder: glib::WeakRef<gtk4::Entry>,
    folder_row: glib::WeakRef<adw::ActionRow>,
}

impl Form {
    fn downgrade(&self) -> WeakForm {
        WeakForm {
            name: self.name.downgrade(),
            name_row: self.name_row.downgrade(),
            body: self.body.downgrade(),
            body_note: self.body_note.downgrade(),
            folder: self.folder.downgrade(),
            folder_row: self.folder_row.downgrade(),
        }
    }

    /// What the fields hold now, as a prompt (no id).
    fn candidate(&self) -> SavedPrompt {
        let folder = self.folder.text().trim().to_owned();
        SavedPrompt {
            id: String::new(),
            name: self.name.text().trim().to_owned(),
            body: view_text(&self.body),
            folder: (!folder.is_empty()).then_some(folder),
        }
    }
}

impl WeakForm {
    fn upgrade(&self) -> Option<Form> {
        Some(Form {
            name: self.name.upgrade()?,
            name_row: self.name_row.upgrade()?,
            body: self.body.upgrade()?,
            body_note: self.body_note.upgrade()?,
            folder: self.folder.upgrade()?,
            folder_row: self.folder_row.upgrade()?,
        })
    }
}

/// The Saved Prompts group while Settings is open: one row per prompt, rebuilt when one is
/// added, copied, deleted or put back. Held only by its controls' handlers, and holding the
/// window and the group weakly, so closing Settings frees it.
struct Prompts {
    window: glib::WeakRef<Window>,
    group: glib::WeakRef<adw::PreferencesGroup>,
    rows: RefCell<Vec<PromptRow>>,
    empty: glib::WeakRef<adw::ActionRow>,
    add_row: glib::WeakRef<adw::ExpanderRow>,
}

impl Prompts {
    fn window(&self) -> Option<Window> {
        self.window.upgrade()
    }

    /// Draws one row per prompt (or the empty line), then the new-prompt form.
    fn rebuild(self: &Rc<Self>) {
        let (Some(group), Some(add_row), Some(empty), Some(window)) = (
            self.group.upgrade(),
            self.add_row.upgrade(),
            self.empty.upgrade(),
            self.window(),
        ) else {
            return;
        };
        let old: Vec<_> = self.rows.borrow_mut().drain(..).collect();
        for shown in old {
            group.remove(&shown.row);
        }
        group.remove(&add_row);
        let prompts = window.imp().config.borrow().saved_prompts.clone();
        let problems = problems_of(&prompts);
        for (index, (prompt, problem)) in prompts.iter().zip(problems).enumerate() {
            let shown = Rc::new(RefCell::new(prompt.clone()));
            let row = self.prompt_row(index, &shown, problem.as_deref());
            group.add(&row.row);
            self.rows.borrow_mut().push(row);
        }
        empty.set_visible(prompts.is_empty());
        group.add(&add_row);
        // Marks the fields of an entry already in the file that breaks a rule.
        self.refresh();
    }

    /// Brings every row's title and subtitle up to date: a change to one prompt can make
    /// another a duplicate, or stop it being one.
    fn refresh(&self) {
        let Some(window) = self.window() else {
            return;
        };
        let prompts = window.imp().config.borrow().saved_prompts.clone();
        let problems = problems_of(&prompts);
        for (shown, (prompt, problem)) in
            self.rows.borrow().iter().zip(prompts.iter().zip(problems))
        {
            shown.name(&format!("/{}", prompt.name.trim()));
            let row = &shown.row;
            row.set_subtitle(&glib::markup_escape_text(&row_subtitle(
                prompt,
                problem.as_deref(),
            )));
            if problem.is_some() {
                row.add_css_class("error");
            } else {
                row.remove_css_class("error");
            }
        }
        // A field's own mark: what it holds now, against the list as it now is (another
        // prompt's rename can make a name free, or take it).
        for (index, shown) in self.rows.borrow().iter().enumerate() {
            let Some(prompt) = prompts.get(index) else {
                continue;
            };
            let name = shown.name.text();
            let name_problem = saved_prompts::name_problem(
                &prompts,
                Some(index),
                &name,
                prompt.folder.as_deref(),
                agent_core::commands::builtins(),
                crate::utils::expand_tilde,
            );
            mark_entry(
                &shown.name,
                &shown.name_row,
                name_problem.as_deref(),
                NAME_HELP,
            );
            let folder_problem = saved_prompts::folder_problem(
                &prompts,
                Some(index),
                &prompt.name,
                Some(shown.folder.text().as_str()),
                crate::utils::expand_tilde,
            );
            mark_entry(
                &shown.folder,
                &shown.folder_row,
                folder_problem.as_deref(),
                FOLDER_HELP,
            );
        }
    }

    /// Saves the config after a change and brings the rows up to date.
    fn changed(&self, window: &Window) {
        window.imp().schedule_config_save();
        self.refresh();
    }

    /// Runs `edit` on the prompt at `index` when it is still the one `shown` was built from (a
    /// commit arriving after the list changed under it does nothing), then keeps `shown` in
    /// step. Returns what `edit` said, or `None` when the prompt is not there any more.
    fn edit<T>(
        &self,
        index: usize,
        shown: &RefCell<SavedPrompt>,
        edit: impl FnOnce(&mut crate::config::TerminalConfig) -> Result<T, String>,
    ) -> Option<(Window, Result<T, String>)> {
        let window = self.window()?;
        let still_here =
            window.imp().config.borrow().saved_prompts.get(index) == Some(&*shown.borrow());
        if !still_here {
            return None;
        }
        let result = edit(&mut window.imp().config.borrow_mut());
        if let Some(now) = window.imp().config.borrow().saved_prompts.get(index) {
            shown.replace(now.clone());
        }
        Some((window, result))
    }

    /// Removes the prompt at `index` when it is still `shown`, leaving `shown` unchanged so
    /// another click or focus-leave on the dying row before the idle rebuild cannot hit the
    /// prompt that shifted into `index`.
    fn remove_at(
        &self,
        index: usize,
        shown: &RefCell<SavedPrompt>,
    ) -> Option<(Window, SavedPrompt)> {
        let window = self.window()?;
        let still_here =
            window.imp().config.borrow().saved_prompts.get(index) == Some(&*shown.borrow());
        if !still_here {
            return None;
        }
        let removed = window
            .imp()
            .config
            .borrow_mut()
            .remove_saved_prompt(index)?;
        Some((window, removed))
    }

    fn prompt_row(
        self: &Rc<Self>,
        index: usize,
        shown: &Rc<RefCell<SavedPrompt>>,
        problem: Option<&str>,
    ) -> PromptRow {
        let prompt = shown.borrow().clone();
        let label = format!("/{}", prompt.name.trim());
        let row = adw::ExpanderRow::builder()
            .title(glib::markup_escape_text(&label).as_str())
            .subtitle(glib::markup_escape_text(&row_subtitle(&prompt, problem)).as_str())
            .build();
        if problem.is_some() {
            row.add_css_class("error");
        }
        let duplicate = gtk4::Button::builder()
            .label("Duplicate")
            .valign(gtk4::Align::Center)
            .tooltip_text("Make a copy to change")
            .build();
        duplicate.update_property(&[gtk4::accessible::Property::Label(&format!(
            "Duplicate {label}"
        ))]);
        let delete = gtk4::Button::builder()
            .icon_name("at-user-trash-symbolic")
            .valign(gtk4::Align::Center)
            .tooltip_text("Delete")
            .css_classes(["flat"])
            .build();
        delete.update_property(&[gtk4::accessible::Property::Label(&format!(
            "Delete {label}"
        ))]);
        row.add_suffix(&duplicate);
        row.add_suffix(&delete);
        let (duplicate_named, delete_named) = (duplicate.clone(), delete.clone());

        let fields = Fields::new(&prompt, &format!("Apply the text of {label}"));
        row.add_row(&fields.name_row);
        row.add_row(&fields.body_row);
        row.add_row(&fields.folder_row);

        // The name: checked as typed, saved only when committed and valid.
        let this = Rc::downgrade(self);
        let typed_shown = Rc::clone(shown);
        fields.name.connect_changed(glib::clone!(
            #[weak(rename_to = name_row)]
            fields.name_row,
            move |entry| {
                let Some(window) = this.upgrade().and_then(|t| t.window()) else {
                    return;
                };
                let config = window.imp().config.borrow();
                let folder = typed_shown.borrow().folder.clone();
                let problem = saved_prompts::name_problem(
                    &config.saved_prompts,
                    Some(index),
                    &entry.text(),
                    folder.as_deref(),
                    agent_core::commands::builtins(),
                    crate::utils::expand_tilde,
                );
                mark_entry(entry, &name_row, problem.as_deref(), NAME_HELP);
            }
        ));
        let this = Rc::downgrade(self);
        let name_shown = Rc::clone(shown);
        let name_entry = fields.name.downgrade();
        let name_row = fields.name_row.downgrade();
        on_commit(&fields.name, move || {
            let (Some(this), Some(entry), Some(name_row)) =
                (this.upgrade(), name_entry.upgrade(), name_row.upgrade())
            else {
                return;
            };
            let text = entry.text().to_string();
            let Some((window, result)) = this.edit(index, &name_shown, |c| {
                c.set_saved_prompt_name(index, &text)
            }) else {
                return;
            };
            match result {
                Ok(false) => {}
                Ok(true) => this.changed(&window),
                Err(why) => mark_entry(&entry, &name_row, Some(&why), NAME_HELP),
            }
        });

        // The text: checked as typed, saved on Apply or when focus leaves it.
        let body_note = fields.body_note.downgrade();
        fields.body.buffer().connect_changed(glib::clone!(
            #[weak(rename_to = view)]
            fields.body,
            move |_| {
                if let Some(note) = body_note.upgrade() {
                    let problem = saved_prompts::validate_body(&view_text(&view)).err();
                    mark_view(&view, &note, problem.as_deref());
                }
            }
        ));
        let this = Rc::downgrade(self);
        let body_shown = Rc::clone(shown);
        let body_view = fields.body.downgrade();
        let body_note = fields.body_note.downgrade();
        let commit_body = Rc::new(move || {
            let (Some(this), Some(view), Some(note)) =
                (this.upgrade(), body_view.upgrade(), body_note.upgrade())
            else {
                return;
            };
            let text = view_text(&view);
            let Some((window, result)) = this.edit(index, &body_shown, |c| {
                c.set_saved_prompt_body(index, &text)
            }) else {
                return;
            };
            match result {
                Ok(false) => {}
                Ok(true) => this.changed(&window),
                Err(why) => mark_view(&view, &note, Some(&why)),
            }
        });
        let on_apply = Rc::clone(&commit_body);
        fields.apply.connect_clicked(move |_| on_apply());
        let focus = gtk4::EventControllerFocus::new();
        focus.connect_leave(move |_| commit_body());
        fields.body.add_controller(focus);

        // The folder: checked as typed, saved when committed, chosen, or cleared.
        let this = Rc::downgrade(self);
        let typed_shown = Rc::clone(shown);
        fields.folder.connect_changed(glib::clone!(
            #[weak(rename_to = folder_row)]
            fields.folder_row,
            move |entry| {
                let Some(window) = this.upgrade().and_then(|t| t.window()) else {
                    return;
                };
                let config = window.imp().config.borrow();
                let name = typed_shown.borrow().name.clone();
                let problem = saved_prompts::folder_problem(
                    &config.saved_prompts,
                    Some(index),
                    &name,
                    Some(entry.text().as_str()),
                    crate::utils::expand_tilde,
                );
                mark_entry(entry, &folder_row, problem.as_deref(), FOLDER_HELP);
            }
        ));
        let this = Rc::downgrade(self);
        let folder_shown = Rc::clone(shown);
        let folder_entry = fields.folder.downgrade();
        let folder_row = fields.folder_row.downgrade();
        let commit_folder = Rc::new(move || {
            let (Some(this), Some(entry), Some(folder_row)) =
                (this.upgrade(), folder_entry.upgrade(), folder_row.upgrade())
            else {
                return;
            };
            let text = entry.text().to_string();
            let Some((window, result)) = this.edit(index, &folder_shown, |c| {
                c.set_saved_prompt_folder(index, &text)
            }) else {
                return;
            };
            match result {
                Ok(false) => {}
                Ok(true) => this.changed(&window),
                Err(why) => mark_entry(&entry, &folder_row, Some(&why), FOLDER_HELP),
            }
        });
        let on_folder = Rc::clone(&commit_folder);
        on_commit(&fields.folder, move || on_folder());
        let on_chosen = Rc::clone(&commit_folder);
        fields.connect_choose(move || on_chosen());
        let folder_entry = fields.folder.downgrade();
        fields.everywhere.connect_clicked(move |_| {
            if let Some(entry) = folder_entry.upgrade() {
                entry.set_text("");
                commit_folder();
            }
        });

        // Duplicate: a copy just below, under a free name.
        let this = Rc::downgrade(self);
        let dup_shown = Rc::clone(shown);
        duplicate.connect_clicked(move |_| {
            let Some(this) = this.upgrade() else {
                return;
            };
            let Some((window, result)) =
                this.edit(index, &dup_shown, |c| c.duplicate_saved_prompt(index))
            else {
                return;
            };
            match result {
                Ok(_) => {
                    window.imp().schedule_config_save();
                    let this = Rc::clone(&this);
                    glib::idle_add_local_once(move || this.rebuild());
                }
                Err(why) => this.toast(&why, None),
            }
        });

        // Delete: taken out at once, with Undo on the toast that says so.
        let this = Rc::downgrade(self);
        let delete_shown = Rc::clone(shown);
        let delete_row = row.downgrade();
        delete.connect_clicked(move |_| {
            let Some(this) = this.upgrade() else {
                return;
            };
            let Some((window, removed)) = this.remove_at(index, &delete_shown) else {
                return;
            };
            if let Some(row) = delete_row.upgrade() {
                row.set_sensitive(false);
            }
            window.imp().schedule_config_save();
            let name = removed.name.trim().to_owned();
            this.toast(&format!("Deleted /{name}"), Some((index, removed)));
            // Rebuilt after this handler returns, since it removes this very button's row.
            let this = Rc::clone(&this);
            glib::idle_add_local_once(move || this.rebuild());
        });
        let shown = PromptRow {
            row,
            named: vec![
                (duplicate_named, "Duplicate"),
                (delete_named, "Delete"),
                (fields.apply.clone(), "Apply the text of"),
            ],
            name: fields.name.clone(),
            name_row: fields.name_row.clone(),
            folder: fields.folder.clone(),
            folder_row: fields.folder_row.clone(),
        };
        shown.name(&label);
        shown
    }

    /// Says `text` on the Settings dialog's toast overlay; with `undo`, offers to put that
    /// removed prompt back where it was.
    fn toast(self: &Rc<Self>, text: &str, undo: Option<(usize, SavedPrompt)>) {
        let Some(group) = self.group.upgrade() else {
            return;
        };
        let toast = adw::Toast::new(&glib::markup_escape_text(text));
        if let Some((index, prompt)) = undo {
            toast.set_button_label(Some("Undo"));
            let this = Rc::downgrade(self);
            let prompt = RefCell::new(Some(prompt));
            toast.connect_button_clicked(move |_| {
                let (Some(this), Some(prompt)) = (this.upgrade(), prompt.take()) else {
                    return;
                };
                let Some(window) = this.window() else {
                    return;
                };
                let name = prompt.name.trim().to_owned();
                let restored = window
                    .imp()
                    .config
                    .borrow_mut()
                    .restore_saved_prompt(index, prompt);
                match restored {
                    Ok(()) => {
                        window.imp().schedule_config_save();
                        this.rebuild();
                    }
                    Err(why) => this.toast(&format!("/{name} was not put back: {why}"), None),
                }
            });
        }
        // The Settings dialog's own overlay; a group shown elsewhere uses the nearest one.
        if let Some(dialog) = group
            .ancestor(adw::PreferencesDialog::static_type())
            .and_downcast::<adw::PreferencesDialog>()
        {
            dialog.add_toast(toast);
        } else if let Some(overlay) = group
            .ancestor(adw::ToastOverlay::static_type())
            .and_downcast::<adw::ToastOverlay>()
        {
            overlay.add_toast(toast);
        }
    }
}

impl AgentTerminalWindow {
    /// Settings → General → Saved Prompts.
    pub(super) fn saved_prompts_group(&self) -> adw::PreferencesGroup {
        let group = adw::PreferencesGroup::builder()
            .title("Saved Prompts")
            .description(
                "Type / and a saved prompt's name in a thread's prompt to put its text there, \
                 for you to read and edit before you send it. The text may hold {file} (the \
                 first @path in the prompt, else the file open in the Changes panel), {branch} \
                 (the thread folder's git branch), {selection} (text selected in the thread, \
                 else in the prompt, up to 8 KB) and {folder} (the thread's folder); {{ and }} \
                 are literal braces. A placeholder with no value is left in place, selected, \
                 for you to fill. Saved prompts are stored unencrypted in the settings file.",
            )
            .build();
        let empty = adw::ActionRow::builder()
            .title("No Saved Prompts Yet")
            .subtitle("Add one below, then type / in a thread's prompt to use it")
            .build();

        let add_row = adw::ExpanderRow::builder()
            .title("New Saved Prompt")
            .subtitle("Its name, text and, if you like, a folder it is only offered in")
            .build();
        let fields = Fields::new(&SavedPrompt::default(), "Apply the new prompt's text");
        // The new prompt's text has no Apply of its own: Add takes all three fields at once.
        fields.apply.set_visible(false);
        add_row.add_row(&fields.name_row);
        add_row.add_row(&fields.body_row);
        add_row.add_row(&fields.folder_row);
        let add = gtk4::Button::builder()
            .label("Add")
            .halign(gtk4::Align::End)
            .margin_top(8)
            .margin_bottom(8)
            .margin_end(12)
            .sensitive(false)
            .css_classes(["suggested-action"])
            .build();
        add.update_property(&[gtk4::accessible::Property::Label("Add the saved prompt")]);
        let add_holder = gtk4::ListBoxRow::builder()
            .activatable(false)
            .selectable(false)
            .child(&add)
            .build();
        add_row.add_row(&add_holder);

        let prompts = Rc::new(Prompts {
            window: self.obj().downgrade(),
            group: group.downgrade(),
            rows: RefCell::new(Vec::new()),
            empty: empty.downgrade(),
            add_row: add_row.downgrade(),
        });
        // The empty line stays first, shown only while there is no prompt.
        group.add(&empty);
        group.add(&add_row);
        prompts.rebuild();
        // A reload of the settings file that changes the prompts redraws the list from it.
        let redraw = Rc::downgrade(&prompts);
        *self.saved_prompts_redraw.borrow_mut() = Some(Rc::new(move || {
            if let Some(prompts) = redraw.upgrade() {
                prompts.rebuild();
            }
        }));

        // Each field shows its own problem. As typed (`strict` false) an empty field is not an
        // error yet; on Add or Enter (`strict`) it is. Add is offered only when the whole prompt
        // could be saved.
        let form = fields.form().downgrade();
        let check: Rc<dyn Fn(bool)> = {
            let form = form.clone();
            let window = self.obj().downgrade();
            let add = add.downgrade();
            Rc::new(move |strict: bool| {
                let (Some(fields), Some(window), Some(add)) =
                    (form.upgrade(), window.upgrade(), add.upgrade())
                else {
                    return;
                };
                let config = window.imp().config.borrow();
                let list = &config.saved_prompts;
                let candidate = fields.candidate();
                let folder = candidate.folder.as_deref();
                let name_problem = (strict || !candidate.name.is_empty())
                    .then(|| {
                        saved_prompts::name_problem(
                            list,
                            None,
                            &candidate.name,
                            folder,
                            agent_core::commands::builtins(),
                            crate::utils::expand_tilde,
                        )
                    })
                    .flatten();
                let body_problem = (strict || !candidate.body.is_empty())
                    .then(|| saved_prompts::validate_body(&candidate.body).err())
                    .flatten();
                mark_view(&fields.body, &fields.body_note, body_problem.as_deref());
                let folder_problem = folder.and_then(|f| {
                    saved_prompts::validate_folder(&crate::utils::expand_tilde(f)).err()
                });
                let whole = saved_prompts::candidate_problem(
                    list,
                    None,
                    &candidate,
                    agent_core::commands::builtins(),
                    crate::utils::expand_tilde,
                );
                // A problem no one field has (the count cap) is shown on the name.
                let other = (strict
                    && name_problem.is_none()
                    && body_problem.is_none()
                    && folder_problem.is_none())
                .then(|| whole.clone())
                .flatten();
                mark_entry(
                    &fields.name,
                    &fields.name_row,
                    name_problem.or(other).as_deref(),
                    NAME_HELP,
                );
                mark_entry(
                    &fields.folder,
                    &fields.folder_row,
                    folder_problem.as_deref(),
                    FOLDER_HELP,
                );
                add.set_sensitive(whole.is_none());
                add.set_tooltip_text(whole.as_deref());
            })
        };
        let on_name = Rc::clone(&check);
        fields.name.connect_changed(move |_| on_name(false));
        let on_body = Rc::clone(&check);
        fields
            .body
            .buffer()
            .connect_changed(move |_| on_body(false));
        let on_folder = Rc::clone(&check);
        fields.folder.connect_changed(move |_| on_folder(false));
        let on_chosen = Rc::clone(&check);
        fields.connect_choose(move || on_chosen(false));
        let folder_entry = fields.folder.downgrade();
        fields.everywhere.connect_clicked(move |_| {
            if let Some(entry) = folder_entry.upgrade() {
                entry.set_text("");
            }
        });

        // Added on the Add button, or Enter in the name: leaving a field keeps the draft.
        // The handlers hold the fields weakly: the fields' own widgets hold the handlers.
        let commit = {
            let add_row = add_row.downgrade();
            let check = Rc::clone(&check);
            Rc::new(move || {
                let (Some(window), Some(fields)) = (prompts.window(), form.upgrade()) else {
                    return;
                };
                let candidate = fields.candidate();
                let result = window.imp().config.borrow_mut().add_saved_prompt(candidate);
                match result {
                    Ok(()) => {
                        window.imp().schedule_config_save();
                        fields.name.set_text("");
                        fields.body.buffer().set_text("");
                        fields.folder.set_text("");
                        if let Some(add_row) = add_row.upgrade() {
                            add_row.set_expanded(false);
                        }
                        prompts.rebuild();
                        check(false);
                    }
                    // Each field says what is wrong with it.
                    Err(_) => check(true),
                }
            })
        };
        let on_enter = Rc::clone(&commit);
        fields.name.connect_activate(move |_| on_enter());
        add.connect_clicked(move |_| commit());
        group
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    #[test]
    fn a_rows_subtitle_says_why_it_is_not_offered_or_where_it_is() {
        let mut prompt = SavedPrompt {
            name: "review".into(),
            body: "x".into(),
            ..Default::default()
        };
        assert_eq!(row_subtitle(&prompt, None), "In every folder");
        prompt.folder = Some("~/work".into());
        assert_eq!(row_subtitle(&prompt, None), "Only in ~/work");
        assert_eq!(
            row_subtitle(&prompt, Some("/model is the app's own command (/model)")),
            "Not offered: /model is the app's own command (/model)"
        );
    }

    /// The Saved Prompts group, driven as a user drives it: a field is saved only when it is
    /// valid and committed, a refusal is shown on the field and saves nothing, an invalid entry
    /// already in the file is listed and kept, Duplicate and Delete change the list, and an Undo
    /// that would now clash is refused. Run from the window test.
    pub(in crate::window) fn the_saved_prompts_group_saves_only_what_is_valid(
        window: &crate::window::AgentTerminalWindow,
    ) {
        let imp = window.imp();
        let before = std::mem::take(&mut imp.config.borrow_mut().saved_prompts);
        // A hand-written entry that is not valid: a built-in's name.
        imp.config.borrow_mut().saved_prompts.push(SavedPrompt {
            name: "model".into(),
            body: "kept".into(),
            ..Default::default()
        });
        let group = imp.saved_prompts_group();
        // The toasts (Delete's Undo) go to the nearest overlay, as in the Settings dialog.
        let overlay = adw::ToastOverlay::new();
        overlay.set_child(Some(&group));
        let ctx = glib::MainContext::default();
        // Focus leaving a field, as moving to another one does.
        let leave = |widget: &gtk4::Widget| {
            let controllers = widget.observe_controllers();
            let focus = (0..controllers.n_items())
                .filter_map(|i| controllers.item(i))
                .find_map(|c| c.downcast::<gtk4::EventControllerFocus>().ok())
                .expect("a focus controller");
            focus.emit_by_name::<()>("leave", &[]);
            crate::testutil::settle(&ctx);
        };
        // In document order: the rows as listed, then the new-prompt form.
        let entries = || crate::testutil::find_all::<gtk4::Entry>(group.upcast_ref());
        let views = || crate::testutil::find_all::<gtk4::TextView>(group.upcast_ref());
        let buttons = || crate::testutil::find_all::<gtk4::Button>(group.upcast_ref());
        let expanders = || crate::testutil::find_all::<adw::ExpanderRow>(group.upcast_ref());
        let prompts = || imp.config.borrow().saved_prompts.clone();
        let commit = |entry: &gtk4::Entry, text: &str| {
            entry.set_text(text);
            entry.emit_by_name::<()>("activate", &[]);
            crate::testutil::settle(&ctx);
        };
        // The `n`th button (in list order) whose label is `label`.
        let nth = |label: &str, n: usize| {
            buttons()
                .into_iter()
                .filter(|b| b.label().as_deref() == Some(label))
                .nth(n)
                .unwrap_or_else(|| panic!("button {label} #{n}"))
        };

        // The invalid entry is listed, says why, and is kept.
        let rows = expanders();
        assert_eq!(rows.len(), 2, "the entry and the new-prompt form");
        assert_eq!(rows[0].title(), "/model");
        assert!(rows[0].subtitle().starts_with("Not offered"));
        assert!(rows[0].has_css_class("error"));
        assert!(entries()[0].has_css_class("error"), "its name says so too");

        // The new-prompt form: Add is offered only for a whole valid prompt.
        let [_, _, new_name, new_folder]: [gtk4::Entry; 4] =
            entries().try_into().expect("two rows of two entries");
        let new_body = views().pop().expect("the new text");
        let add = buttons()
            .into_iter()
            .find(|b| b.label().as_deref() == Some("Add"))
            .expect("the Add button");
        new_name.set_text("help");
        new_body.buffer().set_text("Review {file}");
        crate::testutil::settle(&ctx);
        assert!(!add.is_sensitive(), "a built-in's name");
        assert!(new_name.has_css_class("error"));
        new_name.set_text("review");
        new_folder.set_text("relative/dir");
        crate::testutil::settle(&ctx);
        assert!(!add.is_sensitive(), "a relative folder");
        assert!(new_folder.has_css_class("error"));
        new_folder.set_text("");
        crate::testutil::settle(&ctx);
        assert!(add.is_sensitive());
        assert_eq!(prompts().len(), 1, "typing saves nothing");
        add.emit_clicked();
        crate::testutil::settle(&ctx);
        assert_eq!(prompts().len(), 2);
        assert_eq!(prompts()[1].name, "review");
        assert_eq!(prompts()[1].body, "Review {file}");
        assert_eq!(prompts()[0].name, "model", "the invalid entry is kept");
        assert_eq!(new_name.text(), "", "the form is cleared");

        // The new prompt's own row: a rename to a taken or built-in name is refused and saves
        // nothing; a valid one is saved on Enter.
        let rows = expanders();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].title(), "/review");
        let name = entries()
            .into_iter()
            .find(|e| e.text() == "review")
            .expect("the review row's name");
        commit(&name, "new");
        assert_eq!(prompts()[1].name, "review");
        assert!(name.has_css_class("error"));
        name.set_text("half-typ");
        crate::testutil::settle(&ctx);
        assert_eq!(prompts()[1].name, "review", "nothing saved while typing");
        commit(&name, "review2");
        assert_eq!(prompts()[1].name, "review2");
        assert!(!name.has_css_class("error"));
        assert_eq!(rows[1].title(), "/review2");
        // Its controls are named after the new name.
        assert!(crate::testutil::has_accessible_label(
            nth("Duplicate", 1).upcast_ref(),
            "Duplicate /review2"
        ));
        assert!(crate::testutil::has_accessible_label(
            nth("Apply", 1).upcast_ref(),
            "Apply the text of /review2"
        ));
        // Focus leaving the name commits it, as Enter does.
        name.set_text("review3");
        leave(name.upcast_ref());
        assert_eq!(prompts()[1].name, "review3");
        name.set_text("review2");
        leave(name.upcast_ref());
        assert_eq!(prompts()[1].name, "review2");

        // The text: a blank one is refused on Apply; another is kept exactly.
        let view = views()
            .into_iter()
            .find(|v| view_text(v) == "Review {file}")
            .expect("the review row's text");
        view.buffer().set_text("   ");
        nth("Apply", 1).emit_clicked();
        crate::testutil::settle(&ctx);
        assert_eq!(prompts()[1].body, "Review {file}");
        assert!(view.has_css_class("error"));
        view.buffer().set_text("Notes for {branch}\n");
        assert_eq!(
            prompts()[1].body,
            "Review {file}",
            "nothing saved while typing"
        );
        // Focus leaving the text commits it, as Apply does.
        leave(view.upcast_ref());
        assert_eq!(prompts()[1].body, "Notes for {branch}\n");

        // The folder: relative refused, absolute saved, Everywhere clears it.
        let folder = entries()
            .into_iter()
            .filter(|e| e.placeholder_text().as_deref() == Some("every folder"))
            .nth(1)
            .expect("the review row's folder");
        commit(&folder, "not/absolute");
        assert_eq!(prompts()[1].folder, None);
        assert!(folder.has_css_class("error"));
        folder.set_text("/w/app");
        leave(folder.upcast_ref());
        assert_eq!(prompts()[1].folder.as_deref(), Some("/w/app"));
        assert!(!folder.has_css_class("error"));
        assert_eq!(rows[1].subtitle(), "Only in /w/app");
        nth("Everywhere", 1).emit_clicked();
        crate::testutil::settle(&ctx);
        assert_eq!(prompts()[1].folder, None);

        // Duplicate, then Delete; every icon-only button says what it does.
        nth("Duplicate", 1).emit_clicked();
        crate::testutil::settle(&ctx);
        assert_eq!(prompts().len(), 3);
        assert_eq!(prompts()[2].name, "review2-copy");
        assert_eq!(expanders()[2].title(), "/review2-copy");
        let unnamed = crate::testutil::icon_only_without_label(group.upcast_ref());
        assert!(unnamed.is_empty(), "{unnamed:?}");
        // Clicking Delete twice before the idle rebuild runs removes only that row, not the one
        // that shifted into its slot.
        let delete_mid = buttons()
            .into_iter()
            .filter(|b| b.icon_name().as_deref() == Some("at-user-trash-symbolic"))
            .nth(1)
            .expect("the middle Delete");
        delete_mid.emit_clicked();
        delete_mid.emit_clicked();
        crate::testutil::settle(&ctx);
        assert_eq!(prompts().len(), 2);
        assert_eq!(prompts()[1].name, "review2-copy");
        assert_eq!(expanders().len(), 3, "its row is gone");

        // A prompt of the deleted one's name is added meanwhile: its Undo is refused, and the
        // new one stays as it is, offered.
        new_name.set_text("review2");
        new_body.buffer().set_text("the new one");
        crate::testutil::settle(&ctx);
        add.emit_clicked();
        crate::testutil::settle(&ctx);
        assert_eq!(prompts().len(), 3);
        let before_undo = prompts();
        let undo = crate::testutil::pump_until(&ctx, 10, || {
            crate::testutil::find_all::<gtk4::Button>(overlay.upcast_ref())
                .iter()
                .any(|b| b.label().as_deref() == Some("Undo"))
        });
        assert!(undo, "Delete offers Undo");
        let undo = crate::testutil::find_all::<gtk4::Button>(overlay.upcast_ref())
            .into_iter()
            .find(|b| b.label().as_deref() == Some("Undo"))
            .expect("the Undo button");
        undo.emit_clicked();
        crate::testutil::settle(&ctx);
        assert_eq!(prompts(), before_undo, "a clashing Undo changes nothing");
        assert!(
            !expanders()[2].has_css_class("error"),
            "the new one is offered"
        );

        overlay.set_child(None::<&gtk4::Widget>);
        imp.config.borrow_mut().saved_prompts = before;
    }
}
