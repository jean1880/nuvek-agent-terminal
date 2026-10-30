//! Local instruction settings and an off-main-thread preview of the effective request policy.

use super::*;
use agent_core::local_context::{ContextBudget, LocalDefaults};

const SETTINGS_HELP: &str = "Changes are saved when committed. A running turn keeps its snapshot; a resumed runner adopts changes before the next prompt. Files are reloaded each turn. MCP processes and tool discovery are not available yet.";
const PATH_HELP: &str = "An additional instruction file; absolute, folder-relative, ~/ or $XDG_CONFIG_HOME/ paths. A missing configured file blocks the turn.";

fn validate_defaults(imp: &AgentTerminalWindow, value: &LocalDefaults) -> Result<(), String> {
    value.validate()?;
    for profile in &imp.config.borrow().profiles {
        value.launch(&profile.local)?;
    }
    Ok(())
}

fn budget_value(budget: &ContextBudget, field: usize) -> usize {
    match field {
        0 => budget.context_tokens,
        1 => budget.response_tokens,
        2 => budget.safety_tokens,
        _ => budget.static_context_bytes,
    }
}

fn set_budget_value(budget: &mut ContextBudget, field: usize, value: usize) {
    match field {
        0 => budget.context_tokens = value,
        1 => budget.response_tokens = value,
        2 => budget.safety_tokens = value,
        _ => budget.static_context_bytes = value,
    }
}

impl AgentTerminalWindow {
    fn save_local_defaults(&self, value: LocalDefaults) {
        if validate_defaults(self, &value).is_err() {
            return;
        }
        self.config.borrow_mut().local_defaults = value;
        self.schedule_config_save();
        self.refresh_local_contexts();
    }

    fn local_number_row(
        &self,
        dialog: &adw::PreferencesDialog,
        group: &adw::PreferencesGroup,
        title: &str,
        text: String,
        edit: impl Fn(&mut LocalDefaults, usize) + 'static,
    ) {
        const HELP: &str = "A positive integer. Invalid budgets are not saved; counts are token estimates and byte ceilings.";
        let (entry, error) = validated_entry(title, HELP, &text);
        self.commit_only_entry(
            &entry,
            &error,
            HELP,
            &self.agent_edit_timer(dialog),
            move |imp, text, commit| {
                let mut value = imp.config.borrow().local_defaults.clone();
                let parsed = text
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| "Enter a positive integer.".to_owned())
                    .and_then(|number| {
                        edit(&mut value, number);
                        validate_defaults(imp, &value)?;
                        Ok(value)
                    });
                crate::config::field_change(parsed, &imp.config.borrow().local_defaults, commit)
            },
            |imp, value| {
                if let Some(value) = value {
                    imp.save_local_defaults(value);
                }
            },
        );
        group.add(&entry);
        group.add(&error);
    }

    pub(super) fn local_context_group(
        &self,
        dialog: &adw::PreferencesDialog,
    ) -> adw::PreferencesGroup {
        let obj = self.obj();
        let saved = self.config.borrow().local_defaults.clone();
        let group = adw::PreferencesGroup::builder()
            // A group title is Pango markup: a bare ampersand renders it blank.
            .title("Local Instructions &amp; Tools")
            .description(SETTINGS_HELP)
            .build();
        let native = adw::ActionRow::builder().title("Native Global Instructions").subtitle("$XDG_CONFIG_HOME/agent-terminal/AGENTS.md (or ~/.config/agent-terminal/AGENTS.md), when present, is always first.").use_markup(false).subtitle_lines(0).build();
        group.add(&native);
        self.local_global_files(&group, dialog);
        let ancestors = adw::SwitchRow::builder()
            .title("Discover Ancestor Instructions")
            .subtitle(
                "From / to the canonical working folder: AGENTS.override.md, otherwise AGENTS.md",
            )
            .active(saved.instructions.discover_ancestors)
            .build();
        ancestors.connect_active_notify(glib::clone!(
            #[weak]
            obj,
            move |row| {
                let mut value = obj.imp().config.borrow().local_defaults.clone();
                value.instructions.discover_ancestors = row.is_active();
                obj.imp().save_local_defaults(value);
            }
        ));
        group.add(&ancestors);
        let fallback = adw::SwitchRow::builder()
            .title("Use CLAUDE.md as a Fallback")
            .subtitle("Only when neither AGENTS.override.md nor AGENTS.md exists in that directory")
            .active(saved.instructions.claude_fallback)
            .build();
        fallback.connect_active_notify(glib::clone!(
            #[weak]
            obj,
            move |row| {
                let mut value = obj.imp().config.borrow().local_defaults.clone();
                value.instructions.claude_fallback = row.is_active();
                obj.imp().save_local_defaults(value);
            }
        ));
        group.add(&fallback);
        self.local_number_row(
            dialog,
            &group,
            "Instruction File Cap (bytes)",
            saved.instructions.max_file_bytes.to_string(),
            |value, n| value.instructions.max_file_bytes = n,
        );
        self.local_number_row(
            dialog,
            &group,
            "Total Instruction Cap (bytes)",
            saved.instructions.max_total_bytes.to_string(),
            |value, n| value.instructions.max_total_bytes = n,
        );
        for (field, title) in [
            "Context Tokens (estimated)",
            "Response Reserve (tokens)",
            "Safety Margin (tokens)",
            "Static Context Cap (bytes)",
        ]
        .into_iter()
        .enumerate()
        {
            self.local_number_row(
                dialog,
                &group,
                title,
                budget_value(&saved.budget, field).to_string(),
                move |value, n| set_budget_value(&mut value.budget, field, n),
            );
        }
        self.local_preview_rows(&group);
        group
    }

    fn local_global_files(&self, group: &adw::PreferencesGroup, dialog: &adw::PreferencesDialog) {
        let obj = self.obj();
        type Refill = Rc<dyn Fn()>;
        let refill: Rc<RefCell<Option<Refill>>> = Rc::default();
        let weak_refill = Rc::downgrade(&refill);
        let files = adw::ExpanderRow::builder()
            .title("Additional Global Files, in Order")
            .subtitle("Applied after the native global file and before project instructions")
            .build();
        let rows: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::default();
        let fill: Refill = Rc::new(glib::clone!(
            #[weak]
            obj,
            #[weak]
            files,
            #[strong]
            rows,
            move || {
                for row in rows.borrow_mut().drain(..) {
                    files.remove(&row);
                }
                let paths = obj
                    .imp()
                    .config
                    .borrow()
                    .local_defaults
                    .instructions
                    .global_files
                    .clone();
                for (index, path) in paths.iter().enumerate() {
                    let row = adw::ActionRow::builder()
                        .title(format!("{}. {}", index + 1, path))
                        .use_markup(false)
                        .title_lines(0)
                        .build();
                    for (label, action) in [("↑", 0), ("↓", 1), ("Remove", 2)] {
                        let button = gtk4::Button::builder()
                            .label(label)
                            .tooltip_text(match action {
                                0 => "Move earlier",
                                1 => "Move later",
                                _ => "Remove this instruction file",
                            })
                            .valign(gtk4::Align::Center)
                            .build();
                        button.set_sensitive(
                            action == 2
                                || (action == 0 && index > 0)
                                || (action == 1 && index + 1 < paths.len()),
                        );
                        let weak_refill = weak_refill.clone();
                        let expected = path.clone();
                        button.connect_clicked(glib::clone!(
                            #[weak]
                            obj,
                            move |_| {
                                let mut value = obj.imp().config.borrow().local_defaults.clone();
                                let paths = &mut value.instructions.global_files;
                                if paths.get(index) != Some(&expected) {
                                    return;
                                }
                                match action {
                                    0 if index > 0 => paths.swap(index, index - 1),
                                    1 if index + 1 < paths.len() => paths.swap(index, index + 1),
                                    2 => {
                                        paths.remove(index);
                                    }
                                    _ => return,
                                }
                                obj.imp().save_local_defaults(value);
                                if let Some(fill) =
                                    weak_refill.upgrade().and_then(|r| r.borrow().clone())
                                {
                                    fill();
                                }
                            }
                        ));
                        row.add_suffix(&button);
                    }
                    files.add_row(&row);
                    rows.borrow_mut().push(row);
                }
            }
        ));
        *refill.borrow_mut() = Some(fill.clone());
        fill();
        group.add(&files);
        let (add, error) = validated_entry("Add Global Instruction File", PATH_HELP, "");
        let weak_add = add.downgrade();
        self.commit_only_entry(
            &add,
            &error,
            PATH_HELP,
            &self.agent_edit_timer(dialog),
            |imp, text, commit| {
                if text.trim().is_empty() {
                    return FieldChange::Keep;
                }
                let mut value = imp.config.borrow().local_defaults.clone();
                value.instructions.global_files.push(text.trim().into());
                let parsed = validate_defaults(imp, &value).map(|_| value);
                crate::config::field_change(parsed, &imp.config.borrow().local_defaults, commit)
            },
            move |imp, value| {
                if let Some(value) = value {
                    imp.save_local_defaults(value);
                    if let Some(add) = weak_add.upgrade() {
                        add.set_text("");
                    }
                    if let Some(fill) = refill.borrow().clone() {
                        fill();
                    }
                }
            },
        );
        group.add(&add);
        group.add(&error);
        for path in ["~/.codex/AGENTS.md", "~/.claude/CLAUDE.md"] {
            let quick = adw::ActionRow::builder()
                .title(format!("Quick Add {path}"))
                .use_markup(false)
                .title_lines(0)
                .build();
            let button = gtk4::Button::builder()
                .label("Add")
                .tooltip_text("Add this explicit instruction file")
                .valign(gtk4::Align::Center)
                .build();
            button.connect_clicked(glib::clone!(
                #[weak]
                add,
                move |_| {
                    add.set_text(path);
                    add.emit_by_name::<()>("apply", &[]);
                }
            ));
            quick.add_suffix(&button);
            group.add(&quick);
        }
    }

    fn local_preview_rows(&self, group: &adw::PreferencesGroup) {
        let obj = self.obj();
        let profiles: Vec<_> = self
            .config
            .borrow()
            .local_servers()
            .iter()
            .map(|p| (p.id.clone(), p.name.clone()))
            .collect();
        let mut labels = vec!["App Defaults".to_owned()];
        labels.extend(profiles.iter().map(|(_, name)| name.clone()));
        let choices: Vec<_> = labels.iter().map(String::as_str).collect();
        let profile_row = adw::ComboRow::builder()
            .title("Preview Profile")
            .model(&gtk4::StringList::new(&choices))
            .build();
        let mode_row = adw::ComboRow::builder()
            .title("Preview Mode")
            .model(&gtk4::StringList::new(&["Ask", "Accept Edits", "Plan"]))
            .build();
        group.add(&profile_row);
        group.add(&mode_row);
        let folder = adw::EntryRow::builder()
            .title("Preview for Folder")
            .text(self.config.borrow().starting_directory.clone())
            .build();
        let button = gtk4::Button::builder()
            .label("Preview")
            .valign(gtk4::Align::Center)
            .build();
        folder.add_suffix(&button);
        group.add(&folder);
        let output = adw::ActionRow::builder().title("Instruction Preview").subtitle("Shows source paths, order and estimated costs; document bodies are never shown. Descendant folders are not discovered.").use_markup(false).subtitle_lines(0).build();
        group.add(&output);
        button.connect_clicked(glib::clone!(
            #[weak]
            obj,
            #[weak]
            folder,
            #[weak]
            profile_row,
            #[weak]
            mode_row,
            #[weak]
            output,
            move |button| {
                let imp = obj.imp();
                let selected_profile = profiles
                    .get(profile_row.selected().saturating_sub(1) as usize)
                    .filter(|_| profile_row.selected() > 0)
                    .and_then(|(id, _)| imp.config.borrow().profile_by_id(id).cloned());
                let clear = imp.config.borrow().clear_env.clone();
                let context = {
                    let config = imp.config.borrow();
                    let selected = profile_row.selected() as usize;
                    if selected == 0 {
                        config.local_defaults.launch(&Default::default())
                    } else {
                        profiles
                            .get(selected - 1)
                            .and_then(|(id, _)| config.profile_by_id(id))
                            .ok_or_else(|| {
                                "That profile no longer exists; reopen Settings.".to_owned()
                            })
                            .and_then(|p| config.local_defaults.launch(&p.local))
                    }
                };
                let context = match context {
                    Ok(context) => context,
                    Err(why) => {
                        output.set_subtitle(&why);
                        return;
                    }
                };
                let path = folder.text().to_string();
                let mode = MODES
                    .get(mode_row.selected() as usize)
                    .copied()
                    .unwrap_or(Mode::Ask);
                let button = button.downgrade();
                let output = output.downgrade();
                if let Some(button) = button.upgrade() {
                    button.set_sensitive(false);
                }
                glib::spawn_future_local(async move {
                    let paths = if let Some(profile) = selected_profile {
                        let resolved = super::super::threads::resolve_agent(profile).await;
                        let env = super::super::threads::agent_env(resolved.env, &clear);
                        agent_kit::local_instructions::InstructionPaths::new(
                            env.lookup("HOME").map(std::path::PathBuf::from),
                            env.lookup("XDG_CONFIG_HOME").map(std::path::PathBuf::from),
                        )
                    } else {
                        agent_kit::local_instructions::InstructionPaths::from_env()
                    };
                    let path = paths.home.as_ref().map_or_else(
                        || path.clone(),
                        |home| {
                            agent_kit::paths::expand_tilde_with(&path, &home.display().to_string())
                        },
                    );
                    let result = off_main("the instruction preview", move || {
                        if path.trim().is_empty() {
                            return Err("Enter a folder to preview.".to_owned());
                        }
                        agent_kit::local_instructions::preview(
                            &context,
                            std::path::Path::new(&path),
                            &paths,
                            mode,
                        )
                    })
                    .await;
                    if let Some(output) = output.upgrade() {
                        output.set_subtitle(&agent_core::redact::redact(
                            &result.unwrap_or_else(|why| why),
                        ));
                    }
                    if let Some(button) = button.upgrade() {
                        button.set_sensitive(true);
                    }
                });
            }
        ));
    }

    pub(super) fn local_profile_budget_rows(
        &self,
        server: &Profile,
        dialog: &adw::PreferencesDialog,
        parent: &adw::ExpanderRow,
    ) {
        let obj = self.obj();
        let id = server.id.clone();
        let toggle = adw::SwitchRow::builder().title("Override Request Budget").subtitle("Off follows app defaults. On stores all four values for this profile; changes apply before the next prompt.").active(server.local.budget.is_some()).build();
        toggle.connect_active_notify(glib::clone!(
            #[weak]
            obj,
            move |row| {
                let imp = obj.imp();
                {
                    let mut config = imp.config.borrow_mut();
                    let defaults = config.local_defaults.budget.clone();
                    if let Some(profile) = config.profiles.iter_mut().find(|p| p.id == id) {
                        profile.local.budget = row.is_active().then_some(defaults);
                    }
                }
                imp.schedule_config_save();
                imp.refresh_local_contexts();
            }
        ));
        parent.add_row(&toggle);
        for (field, title) in [
            "Context Tokens Override",
            "Response Reserve Override",
            "Safety Margin Override",
            "Static Byte Cap Override",
        ]
        .into_iter()
        .enumerate()
        {
            const HELP: &str = "A positive integer for this profile budget. Turn off Override Request Budget to follow all app defaults again.";
            let defaults = self.config.borrow().local_defaults.budget.clone();
            let saved =
                budget_value(server.local.budget.as_ref().unwrap_or(&defaults), field).to_string();
            let (entry, error) = validated_entry(title, HELP, &saved);
            entry.set_sensitive(toggle.is_active());
            let id = server.id.clone();
            let apply_id = id.clone();
            let toggle_id = id.clone();
            toggle.connect_active_notify(glib::clone!(
                #[weak]
                obj,
                #[weak]
                entry,
                move |row| {
                    entry.set_sensitive(row.is_active());
                    let config = obj.imp().config.borrow();
                    let budget = config
                        .profile_by_id(&toggle_id)
                        .and_then(|p| p.local.budget.as_ref())
                        .unwrap_or(&config.local_defaults.budget);
                    entry.set_text(&budget_value(budget, field).to_string());
                }
            ));
            self.commit_only_entry(
                &entry,
                &error,
                HELP,
                &self.agent_edit_timer(dialog),
                move |imp, text, commit| {
                    let config = imp.config.borrow();
                    let Some(profile) = config.profile_by_id(&id) else {
                        return FieldChange::Invalid("This profile no longer exists.".into());
                    };
                    let Some(mut budget) = profile.local.budget.clone() else {
                        return FieldChange::Keep;
                    };
                    let mut options = profile.local.clone();
                    let parsed = text
                        .trim()
                        .parse::<usize>()
                        .map_err(|_| "Enter a positive integer.".to_owned())
                        .and_then(|n| {
                            set_budget_value(&mut budget, field, n);
                            options.budget = Some(budget);
                            config.local_defaults.launch(&options)?;
                            Ok(options)
                        });
                    crate::config::field_change(parsed, &profile.local, commit)
                },
                move |imp, value| {
                    if let Some(value) = value {
                        if let Some(profile) = imp
                            .config
                            .borrow_mut()
                            .profiles
                            .iter_mut()
                            .find(|p| p.id == apply_id)
                        {
                            profile.local = value;
                        }
                        imp.schedule_config_save();
                        imp.refresh_local_contexts();
                    }
                },
            );
            parent.add_row(&entry);
            parent.add_row(&error);
        }
    }
}
