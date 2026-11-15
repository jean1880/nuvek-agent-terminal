//! Preferences → Editor: the external editor the "Open in …" buttons open a file in, at a line.
//!
//! A preset dropdown (VS Code, Zed, JetBrains, GNOME Text Editor, Custom) and an editable argv
//! field, apart from the diff tool's group. The field is validated as it is typed
//! ([`agent_kit::editor::validate`]) and an invalid value is never persisted. Both apply on
//! change. A preset's program is marked installed, missing or (until the probe answers)
//! unknown: the probe runs off the main thread, absolute locations only. Running the editor in
//! a terminal is not offered yet.

use std::cell::Cell;
use std::rc::Rc;

use agent_kit::editor::{self, EditorTool, PRESETS};

use super::diff_prefs::{format_argv, parse_argv};
use super::*;
use crate::editor_tool::EditorTools;

/// Dropdown rows: "None", every preset, then "Custom".
const NONE_INDEX: u32 = 0;
const CUSTOM_INDEX: u32 = PRESETS.len() as u32 + 1;

/// The Command field's title while its text is wrong. Row titles are Pango markup, and `why`
/// can quote what was typed (`&`, `<`, an unbalanced quote), so it is escaped.
fn command_error_title(why: &str) -> String {
    format!("Command — {}", gtk4::glib::markup_escape_text(why))
}

/// The editor a field's text describes: a preset when the argv is exactly one, else a custom
/// editor named after its program. `Err` is the reason to show; `Ok(None)` is an empty field.
pub(super) fn tool_from_text(text: &str) -> Result<Option<EditorTool>, String> {
    let argv = parse_argv(text)?;
    if argv.is_empty() {
        return Ok(None);
    }
    let mut tool = EditorTool {
        name: String::new(),
        argv,
        terminal: false,
    };
    tool.name = match editor::preset_index(&tool) {
        Some(i) => PRESETS[i].name.to_owned(),
        None => std::path::Path::new(&tool.argv[0]).file_name().map_or_else(
            || tool.argv[0].clone(),
            |n| n.to_string_lossy().into_owned(),
        ),
    };
    editor::validate(&tool)?;
    Ok(Some(tool))
}

/// Which dropdown row shows `tool`.
pub(super) fn row_of(tool: Option<&EditorTool>) -> u32 {
    match tool {
        None => NONE_INDEX,
        Some(t) => editor::preset_index(t).map_or(CUSTOM_INDEX, |i| i as u32 + 1),
    }
}

impl AgentTerminalWindow {
    /// Adds the "Editor" group to the Git page, beside the diff tool's.
    pub(super) fn add_editor_group(&self, page: &adw::PreferencesPage) {
        let obj = self.obj();
        let current = self.config.borrow().editor.clone();

        let group = adw::PreferencesGroup::builder()
            .title("Editor")
            .description(
                "Opened from a file edit's “Open in …” button and the diff panel, at the first \
                 changed line. Placeholders: {file} (required), {line}, {column}, {dir}. Never \
                 run through a shell. Only files inside the thread's folder open. With no editor \
                 set, the system's default app opens the file, which cannot jump to a line.",
            )
            .build();

        let mut names: Vec<String> = vec!["None".to_owned()];
        names.extend(PRESETS.iter().map(|p| p.name.to_owned()));
        names.push("Custom".to_owned());
        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let model = gtk4::StringList::new(&name_refs);
        let preset_row = adw::ComboRow::builder()
            .title("Editor")
            .subtitle("Choose a preset, or edit the command below")
            .model(&model)
            .selected(row_of(current.as_ref()))
            .build();

        let command_row = adw::EntryRow::builder()
            .title("Command")
            .tooltip_text(
                "The editor's command line, with the placeholders above, e.g. code -g \
                 {file}:{line}:{column}",
            )
            .text(
                current
                    .as_ref()
                    .map(|t| format_argv(&t.argv))
                    .unwrap_or_default(),
            )
            .build();
        group.add(&preset_row);
        group.add(&command_row);
        page.add(&group);

        // Set by code that moves one control because the other changed, so the move is not
        // taken for the user's own edit.
        let syncing = Rc::new(Cell::new(false));

        preset_row.connect_selected_notify(glib::clone!(
            #[weak]
            obj,
            #[weak]
            command_row,
            #[strong]
            syncing,
            move |row| {
                if syncing.get() {
                    return;
                }
                let imp = obj.imp();
                match row.selected() {
                    NONE_INDEX => {
                        syncing.set(true);
                        command_row.set_text("");
                        syncing.set(false);
                        command_row.set_title("Command");
                        command_row.remove_css_class("error");
                        imp.set_editor(None);
                    }
                    CUSTOM_INDEX => {
                        // Keep what is typed; a custom editor is whatever the field holds.
                        command_row.grab_focus();
                    }
                    n => {
                        let Some(preset) = PRESETS.get((n - 1) as usize) else {
                            return;
                        };
                        let tool = preset.tool();
                        syncing.set(true);
                        command_row.set_text(&format_argv(&tool.argv));
                        syncing.set(false);
                        command_row.set_title("Command");
                        command_row.remove_css_class("error");
                        imp.set_editor(Some(tool));
                    }
                }
            }
        ));

        // The field says at once whether it is valid, but saves only when committed (Enter, its
        // apply button, or focus leaving it), from what it then holds.
        command_row.connect_changed(glib::clone!(
            #[strong]
            syncing,
            move |entry| {
                if syncing.get() {
                    return;
                }
                let widget = entry.upcast_ref::<gtk4::Widget>();
                match tool_from_text(&entry.text()) {
                    Err(why) => {
                        let title = command_error_title(&why);
                        entry.add_css_class("error");
                        entry.set_title(&title);
                        widget.update_property(&[gtk4::accessible::Property::Description(&title)]);
                        widget.update_state(&[gtk4::accessible::State::Invalid(
                            gtk4::AccessibleInvalidState::True,
                        )]);
                    }
                    Ok(_) => {
                        entry.remove_css_class("error");
                        entry.set_title("Command");
                        widget.update_property(&[gtk4::accessible::Property::Description("")]);
                        widget.update_state(&[gtk4::accessible::State::Invalid(
                            gtk4::AccessibleInvalidState::False,
                        )]);
                    }
                }
            }
        ));
        let commit = Rc::new(glib::clone!(
            #[weak]
            obj,
            #[weak]
            preset_row,
            #[weak]
            command_row,
            #[strong]
            syncing,
            move || {
                // Still invalid when committed: nothing is saved.
                let Ok(tool) = tool_from_text(&command_row.text()) else {
                    return;
                };
                // A typed argv that is a preset's selects it; otherwise it is Custom.
                let want = row_of(tool.as_ref());
                if preset_row.selected() != want {
                    syncing.set(true);
                    preset_row.set_selected(want);
                    syncing.set(false);
                }
                obj.imp().set_editor(tool);
            }
        ));
        command_row.set_show_apply_button(true);
        let on_apply = commit.clone();
        command_row.connect_apply(move |_| on_apply());
        let on_enter = commit.clone();
        command_row.connect_entry_activated(move |_| on_enter());
        let focus = gtk4::EventControllerFocus::new();
        focus.connect_leave(move |_| commit());
        command_row.add_controller(focus);

        // Mark presets whose program is not installed, once the (off-thread) probe answers.
        // Until then a preset is unknown: listed under its own name.
        glib::MainContext::default().spawn_local(glib::clone!(
            #[weak]
            model,
            #[weak]
            preset_row,
            async move {
                let installed = crate::editor_tool::installed_presets().await;
                if installed.len() != PRESETS.len() {
                    return;
                }
                let mut missing = Vec::new();
                for (i, present) in installed.iter().enumerate() {
                    if !present {
                        missing.push(PRESETS[i].name);
                        model.splice(
                            i as u32 + 1,
                            1,
                            &[&format!("{} (not installed)", PRESETS[i].name)],
                        );
                    }
                }
                if !missing.is_empty() {
                    preset_row.set_subtitle(&format!(
                        "Choose a preset, or edit the command below. Not found on this system: {}",
                        missing.join(", ")
                    ));
                }
            }
        ));
    }

    /// Applies a validated editor (or none): the config, its save, and every card and panel row.
    pub(super) fn set_editor(&self, tool: Option<EditorTool>) {
        if self.config.borrow().editor == tool {
            return;
        }
        self.config.borrow_mut().editor = tool.clone();
        EditorTools::shared().set(tool);
        self.schedule_config_save();
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// The Editor group, driven as a user drives it: what is typed or chosen is what is saved,
    /// and an invalid command is shown and never saved. Run from the window test.
    pub(in crate::window) fn the_group_saves_only_what_is_valid_and_applies_live(
        window: &crate::window::AgentTerminalWindow,
    ) {
        let imp = window.imp();
        let page = adw::PreferencesPage::new();
        imp.add_editor_group(&page);
        let widgets = crate::testutil::descendants(page.upcast_ref());
        let find = |pred: &dyn Fn(&gtk4::Widget) -> bool| {
            widgets.iter().find(|w| pred(w)).cloned().expect("row")
        };
        let combo = find(&|w| w.is::<adw::ComboRow>())
            .downcast::<adw::ComboRow>()
            .expect("combo");
        let entry = find(&|w| w.is::<adw::EntryRow>())
            .downcast::<adw::EntryRow>()
            .expect("entry");
        let saved = || imp.config.borrow().editor.clone();
        let live = || EditorTools::shared().get();
        let ctx = glib::MainContext::default();
        let type_in = |text: &str| {
            entry.set_text(text);
            entry.emit_by_name::<()>("apply", &[]);
            crate::testutil::settle(&ctx);
        };

        // Typing alone saves nothing, even a valid prefix of a command.
        entry.set_text("z");
        crate::testutil::settle(&ctx);
        assert_eq!(saved(), None, "a half-typed command is never saved");
        // A preset typed out selects it, is saved, and is what the buttons now use.
        type_in("zed {file}:{line}:{column}");
        assert_eq!(combo.selected(), 2);
        assert_eq!(saved().map(|t| t.name).as_deref(), Some("Zed"));
        assert_eq!(live().map(|t| t.name).as_deref(), Some("Zed"));
        // An edit that breaks it is shown on the field, and what was saved is left alone.
        let end = entry.text().chars().count() as i32;
        entry.delete_text(end - 1, end);
        crate::testutil::settle(&ctx);
        assert!(entry.has_css_class("error"));
        assert_ne!(entry.title(), "Command", "the title says what is wrong");
        assert_eq!(saved().map(|t| t.name).as_deref(), Some("Zed"));
        // A refused paste keeps the saved editor, and names the placeholder.
        type_in("zed {fiel}");
        assert!(entry.title().contains("{fiel}"), "{}", entry.title());
        assert_eq!(saved().map(|t| t.name).as_deref(), Some("Zed"));
        // No {file}, or a placeholder program: refused.
        type_in("zed --new-window");
        assert_eq!(saved().map(|t| t.name).as_deref(), Some("Zed"));
        type_in("{file} {line}");
        assert_eq!(saved().map(|t| t.name).as_deref(), Some("Zed"));
        // Clearing the field on purpose is "no editor", and is saved and applied.
        type_in("");
        assert_eq!(saved(), None);
        assert_eq!(live(), None);
        assert_eq!(combo.selected(), NONE_INDEX);
        // Anything else valid is a custom editor, named after its program, never in a terminal.
        type_in("/opt/bin/kate -l {line} {file}");
        assert!(!entry.has_css_class("error"));
        assert_eq!(entry.title(), "Command");
        assert_eq!(combo.selected(), CUSTOM_INDEX);
        assert_eq!(saved().map(|t| t.name).as_deref(), Some("kate"));
        assert_eq!(saved().map(|t| t.terminal), Some(false));
        // Choosing a preset writes its command into the field and saves it.
        combo.set_selected(1);
        let first = PRESETS[0].tool();
        assert_eq!(entry.text(), format_argv(&first.argv));
        assert_eq!(saved(), Some(first.clone()));
        assert_eq!(live(), Some(first));
        // Custom keeps what is there; None clears both.
        combo.set_selected(CUSTOM_INDEX);
        assert_eq!(saved(), Some(PRESETS[0].tool()));
        combo.set_selected(NONE_INDEX);
        assert_eq!(entry.text(), "");
        assert_eq!(saved(), None);

        // Before or after the installed-programs probe answers, a preset is listed under its
        // own name or marked as not installed, never anything else.
        crate::testutil::settle(&glib::MainContext::default());
        let model = combo
            .model()
            .and_then(|m| m.downcast::<gtk4::StringList>().ok())
            .expect("list");
        for (i, preset) in PRESETS.iter().enumerate() {
            let label = model.string(i as u32 + 1).expect("label").to_string();
            assert!(
                label == preset.name || label == format!("{} (not installed)", preset.name),
                "{label}"
            );
        }
    }

    #[test]
    fn typing_a_preset_selects_it_and_anything_else_is_a_custom_editor_named_after_its_program() {
        let t = tool_from_text("zed {file}:{line}:{column}")
            .expect("ok")
            .expect("tool");
        assert_eq!(t.name, "Zed");
        assert_eq!(row_of(Some(&t)), 2);
        let t = tool_from_text("/opt/bin/kate {file}")
            .expect("ok")
            .expect("tool");
        assert_eq!(t.name, "kate");
        assert_eq!(row_of(Some(&t)), CUSTOM_INDEX);
        assert_eq!(tool_from_text("  ").expect("ok"), None);
        assert_eq!(row_of(None), NONE_INDEX);
        assert_eq!(CUSTOM_INDEX as usize, PRESETS.len() + 1);
    }

    #[test]
    fn a_bad_field_says_why_and_is_not_an_editor() {
        assert!(tool_from_text("zed {fiel}")
            .expect_err("placeholder")
            .contains("{fiel}"));
        assert!(tool_from_text("zed").is_err(), "names no file");
        assert!(tool_from_text("{file}").is_err(), "the program is fixed");
        assert!(tool_from_text("-x {file}").is_err());
        assert!(tool_from_text("zed 'unclosed").is_err(), "unbalanced quote");
    }
}
