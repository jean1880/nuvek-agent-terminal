//! Settings → Agents → Local Model Servers: the Local agent's servers, as many as the user adds.
//!
//! Each server is a profile of the Local agent (`Driver::Local`) with its own model server, name,
//! defaults and on/off switch; a thread opened on one keeps it. Every row applies as it is made
//! and edits that server's profile by id. "Models on the Server" reads the server's model list
//! (one GET, off the main thread) and says what it found.

use super::*;

const LOCAL_GROUP_HELP: &str = "Run threads on models from your own servers: Ollama, LM Studio, \
     vLLM, llama.cpp, LiteLLM or any OpenAI-compatible one. Add as many servers as you like; \
     each lists its models and has its own entry under New Thread With.";
const SERVER_NAME_HELP: &str = "How this server is named in menus, e.g. NAS Ollama";
const SERVER_TYPE_HELP: &str = "Sets the default address; every type is reached through its \
     OpenAI-compatible API";
const SERVER_URL_HELP: &str = "The server's address, e.g. http://192.0.2.10:11434 or \
     http://localhost:8000 (a trailing /v1 is fine). Blank: the server type's default on this \
     machine; Other needs one";

/// Fills the server list again (after an add or a removal).
type Refill = Rc<dyn Fn()>;

/// What "Models on the Server" says about a scan of `base` that found `models`: how many a
/// thread can run on, which (marked when the server says one cannot call tools), and how many
/// embedding models were left out. `Err` when there is none to chat with.
pub(super) fn scan_summary(
    base: &str,
    models: &[agent_core::provider::ModelInfo],
) -> Result<String, String> {
    let (chat, embed): (Vec<_>, Vec<_>) = models.iter().partition(|m| m.chat);
    if chat.is_empty() {
        return Err(if models.is_empty() {
            format!("{base} lists no models. Pull or load one on the server, then Scan Now.")
        } else {
            format!("{base} lists only embedding models, which cannot hold a conversation.")
        });
    }
    let named: Vec<String> = chat
        .iter()
        .take(8)
        .map(|m| match m.tools {
            Some(false) => format!("{} (no tools)", m.id),
            _ => m.id.clone(),
        })
        .collect();
    let shown: Vec<&str> = named.iter().map(String::as_str).collect();
    let mut text = format!(
        "{} model{} at {base}: {}",
        chat.len(),
        if chat.len() == 1 { "" } else { "s" },
        shown.join(", ")
    );
    if chat.len() > shown.len() {
        text.push_str(&format!(" and {} more", chat.len() - shown.len()));
    }
    if !embed.is_empty() {
        text.push_str(&format!(
            " ({} embedding model{} not offered)",
            embed.len(),
            if embed.len() == 1 { "" } else { "s" }
        ));
    }
    Ok(text)
}

/// Reads `server`'s model list off the main thread, with the key its threads would use (its env
/// file and the session's environment): the chat models, or why there are none.
async fn scan_server(server: Profile) -> Result<(String, Vec<String>), String> {
    let Some(provider) = crate::config::profile_provider(&server, Driver::Local) else {
        return Err("No server is set.".to_owned());
    };
    let base = provider.base()?;
    let hint = provider.kind.tool_setup_hint();
    let key = match provider.key_env()? {
        Some(name) => {
            let clear = super::super::SharedConfig::default()
                .borrow()
                .clear_env
                .clone();
            let resolved = super::super::threads::resolve_agent(server).await;
            match super::super::threads::agent_env(resolved.env, &clear).lookup(name) {
                Some(value) => Some(value),
                None => return Err(format!("{name} is not set, so the server has no API key.")),
            }
        }
        None => None,
    };
    let models = off_main("the scan", move || {
        agent_kit::provider_models::fetch_info(
            &provider,
            key.as_deref(),
            crate::model_catalog::USER_AGENT,
        )
    })
    .await?;
    let mut summary = scan_summary(&base, &models)?;
    if let Some(hint) = hint {
        summary.push_str(&format!(". Note: {hint}."));
    }
    let chat = models
        .into_iter()
        .filter(|m| m.chat)
        .map(|m| m.id)
        .collect();
    Ok((summary, chat))
}

impl AgentTerminalWindow {
    /// The server profile `id`, as saved now (`None` once removed).
    fn server_now(&self, id: &str) -> Option<Profile> {
        self.config.borrow().profile_by_id(id).cloned()
    }

    /// Applies `edit` to the server profile `id`, saves, and rescans: what is offered follows.
    fn edit_server(&self, id: &str, edit: impl FnOnce(&mut Profile)) {
        let changed = {
            let mut config = self.config.borrow_mut();
            config
                .profiles
                .iter_mut()
                .find(|p| p.id == id)
                .map(edit)
                .is_some()
        };
        if !changed {
            return;
        }
        self.schedule_config_save();
        if let Some(supervisor) = crate::delegation::supervisor() {
            supervisor.config_changed(id);
        }
        if !self.is_closing() {
            self.refresh_profile_selection();
            self.refresh_agent_data(super::super::threads::Probes::Force);
        }
    }

    /// The Local Model Servers group: one expandable row per server, and Add Server.
    pub(super) fn local_servers_group(
        &self,
        dialog: &adw::PreferencesDialog,
    ) -> adw::PreferencesGroup {
        let obj = self.obj();
        let add = gtk4::Button::builder()
            .icon_name("at-list-add-symbolic")
            .tooltip_text("Add a Server")
            .valign(gtk4::Align::Center)
            .css_classes(["flat"])
            .build();
        let group = adw::PreferencesGroup::builder()
            .title("Local Model Servers")
            .description(LOCAL_GROUP_HELP)
            .header_suffix(&add)
            .build();
        let rows: Rc<RefCell<Vec<gtk4::Widget>>> = Rc::default();
        // Rebuilt whole after an add or a removal: the list is short, and each row's closures
        // hold only its server's id.
        let rebuild: Rc<RefCell<Option<Refill>>> = Rc::default();
        let fill: Rc<dyn Fn()> = Rc::new(glib::clone!(
            #[weak]
            obj,
            #[weak]
            group,
            #[weak]
            dialog,
            #[strong]
            rows,
            #[strong]
            rebuild,
            move || {
                for row in rows.borrow_mut().drain(..) {
                    group.remove(&row);
                }
                let servers: Vec<Profile> = obj
                    .imp()
                    .config
                    .borrow()
                    .local_servers()
                    .into_iter()
                    .cloned()
                    .collect();
                if servers.is_empty() {
                    let empty = adw::ActionRow::builder()
                        .title("No servers yet")
                        .subtitle("Add one with the + button: an Ollama on this machine to start")
                        .build();
                    group.add(&empty);
                    rows.borrow_mut().push(empty.upcast());
                }
                let again = rebuild.borrow().clone();
                for server in servers {
                    let row = obj.imp().server_row(&server, &dialog, again.clone());
                    group.add(&row);
                    rows.borrow_mut().push(row.upcast());
                }
            }
        ));
        *rebuild.borrow_mut() = Some(fill.clone());
        fill();
        add.connect_clicked(glib::clone!(
            #[weak]
            obj,
            #[strong]
            fill,
            move |_| {
                let imp = obj.imp();
                imp.config
                    .borrow_mut()
                    .add_local_server(agent_core::provider::ProviderKind::Ollama);
                imp.schedule_config_save();
                imp.refresh_profile_selection();
                imp.refresh_agent_data(super::super::threads::Probes::Force);
                fill();
            }
        ));
        group
    }

    /// One server: its name, switch, type, address, key, models, defaults and Remove.
    fn server_row(
        &self,
        server: &Profile,
        dialog: &adw::PreferencesDialog,
        rebuild: Option<Rc<dyn Fn()>>,
    ) -> adw::ExpanderRow {
        let obj = self.obj();
        let id: Rc<str> = server.id.as_str().into();
        let provider = crate::config::profile_provider(server, Driver::Local)
            .unwrap_or_else(|| ModelProvider::new(ProviderKind::Ollama));
        let row = adw::ExpanderRow::builder()
            .title(&server.name)
            .subtitle("Checking…")
            .use_markup(false)
            .build();
        let brand = gtk4::Image::from_gicon(&crate::icons::driver_icon(Driver::Local));
        brand.set_pixel_size(20);
        row.add_prefix(&brand);

        let switch = gtk4::Switch::builder()
            .active(!server.disabled)
            .valign(gtk4::Align::Center)
            .tooltip_text("Offer this server for new threads")
            .build();
        row.add_suffix(&switch);
        switch.connect_active_notify(glib::clone!(
            #[weak]
            obj,
            #[strong]
            id,
            move |s| {
                let on = s.is_active();
                obj.imp().edit_server(&id, |p| p.disabled = !on);
            }
        ));

        // The models the last scan found, for the Default Model row.
        let models: Rc<RefCell<Vec<String>>> = Rc::default();
        let model_row = adw::ComboRow::builder()
            .title("Default Model")
            .subtitle("The model this server's new threads start on")
            .build();
        let filling = Rc::new(std::cell::Cell::new(false));
        let fill_models = Rc::new(glib::clone!(
            #[weak]
            obj,
            #[weak]
            model_row,
            #[strong]
            models,
            #[strong]
            filling,
            #[strong]
            id,
            move || {
                let saved = obj.imp().server_now(&id).and_then(|p| p.default_model);
                let mut labels = vec!["The server's first chat model".to_owned()];
                labels.extend(models.borrow().iter().cloned());
                // A saved model the server no longer lists stays visible, marked.
                if let Some(saved) = saved.as_ref().filter(|s| !models.borrow().contains(s)) {
                    labels.push(format!("{saved} (not listed)"));
                }
                let selected = saved
                    .as_ref()
                    .and_then(|s| {
                        models
                            .borrow()
                            .iter()
                            .position(|m| m == s)
                            .map(|i| i + 1)
                            .or(Some(labels.len() - 1))
                    })
                    .unwrap_or(0);
                let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
                filling.set(true);
                model_row.set_model(Some(&gtk4::StringList::new(&refs)));
                model_row.set_selected(selected as u32);
                filling.set(false);
            }
        ));
        model_row.connect_selected_notify(glib::clone!(
            #[weak]
            obj,
            #[strong]
            models,
            #[strong]
            filling,
            #[strong]
            id,
            move |r| {
                if filling.get() {
                    return;
                }
                let at = r.selected() as usize;
                let listed = models.borrow().len();
                // The last entry past the list is the saved, unlisted model: unchanged.
                if at > listed {
                    return;
                }
                let choice = (at > 0).then(|| models.borrow()[at - 1].clone());
                obj.imp().edit_server(&id, |p| p.default_model = choice);
            }
        ));

        let scan_row = adw::ActionRow::builder()
            .title("Models on the Server")
            .subtitle("Checking…")
            .subtitle_selectable(true)
            .use_markup(false)
            .build();
        let scan_button = gtk4::Button::builder()
            .label("Scan Now")
            .valign(gtk4::Align::Center)
            .tooltip_text("Read the server's model list again")
            .build();
        scan_row.add_suffix(&scan_button);
        let generation = Rc::new(crate::window::Generation::new());
        let scan: Rc<dyn Fn()> = Rc::new(glib::clone!(
            #[weak]
            obj,
            #[weak]
            scan_row,
            #[weak]
            row,
            #[strong]
            generation,
            #[strong]
            models,
            #[strong]
            fill_models,
            #[strong]
            id,
            move || {
                let Some(server) = obj.imp().server_now(&id) else {
                    return;
                };
                let current = generation.next();
                scan_row.set_subtitle("Checking…");
                row.set_subtitle("Checking…");
                scan_row.remove_css_class("error");
                let generation = generation.clone();
                let models = models.clone();
                let fill_models = fill_models.clone();
                glib::MainContext::default().spawn_local(glib::clone!(
                    #[weak]
                    scan_row,
                    #[weak]
                    row,
                    async move {
                        let found = scan_server(server).await;
                        if !generation.is_current(current) {
                            return;
                        }
                        match found {
                            Ok((summary, chat)) => {
                                row.set_subtitle(&format!(
                                    "Ready: {} model{}",
                                    chat.len(),
                                    if chat.len() == 1 { "" } else { "s" }
                                ));
                                scan_row.set_subtitle(&summary);
                                *models.borrow_mut() = chat;
                            }
                            Err(why) => {
                                row.set_subtitle("Not reachable");
                                scan_row.add_css_class("error");
                                scan_row.set_subtitle(&why);
                                models.borrow_mut().clear();
                            }
                        }
                        fill_models();
                    }
                ));
            }
        ));
        scan_button.connect_clicked({
            let scan = scan.clone();
            move |_| scan()
        });

        // Name: unique, so menus and the default profile can tell servers apart.
        let (name, name_error) = validated_entry("Name", SERVER_NAME_HELP, &server.name);
        let timer = self.agent_edit_timer(dialog);
        let name_id = id.clone();
        let apply_id = id.clone();
        self.commit_only_entry(
            &name,
            &name_error,
            SERVER_NAME_HELP,
            &timer,
            move |imp, text, commit| {
                let config = imp.config.borrow();
                let saved = config
                    .profile_by_id(&name_id)
                    .map(|p| p.name.clone())
                    .unwrap_or_default();
                let parsed =
                    match crate::config::profile_name_problem(&config.profiles, &name_id, text) {
                        Some(problem) => Err(problem),
                        None => Ok(text.trim().to_owned()),
                    };
                crate::config::field_change(parsed, &saved, commit)
            },
            glib::clone!(
                #[weak]
                row,
                move |imp: &AgentTerminalWindow, value: Option<String>| {
                    if let Some(name) = value {
                        row.set_title(&name);
                        imp.edit_server(&apply_id, |p| p.name = name);
                    }
                }
            ),
        );
        row.add_row(&name);
        row.add_row(&name_error);

        let kind_id = id.clone();
        let kind_scan = scan.clone();
        let kind_row = self.choice_row(
            "Server Type",
            SERVER_TYPE_HELP,
            ProviderKind::LOCAL
                .into_iter()
                .map(|k| (k, k.label().to_owned()))
                .collect(),
            provider.kind,
            move |imp, kind| {
                let Some(kind) = kind else { return };
                imp.edit_server(&kind_id, |p| {
                    // What was typed stays, so trying another type keeps the address.
                    let current = crate::config::profile_provider(p, Driver::Local)
                        .unwrap_or_else(|| ModelProvider::new(kind));
                    p.provider = Some(ModelProvider { kind, ..current });
                });
                kind_scan();
            },
        );
        row.add_row(&kind_row);

        let (url, url_error) = validated_entry(
            "Server URL",
            SERVER_URL_HELP,
            provider.base_url.as_deref().unwrap_or(""),
        );
        let timer = self.agent_edit_timer(dialog);
        let (url_id, url_apply, url_scan) = (id.clone(), id.clone(), scan.clone());
        self.commit_only_entry(
            &url,
            &url_error,
            SERVER_URL_HELP,
            &timer,
            move |imp, text, commit| {
                let saved = imp
                    .server_now(&url_id)
                    .and_then(|p| crate::config::profile_provider(&p, Driver::Local))
                    .and_then(|p| p.base_url);
                provider_url_edit(text, saved.as_deref(), commit)
            },
            move |imp, value| {
                if let Some(value) = value {
                    imp.edit_server(&url_apply, |p| {
                        let mut provider = crate::config::profile_provider(p, Driver::Local)
                            .unwrap_or_else(|| ModelProvider::new(ProviderKind::Ollama));
                        provider.base_url = value;
                        p.provider = Some(provider);
                    });
                    if !imp.is_closing() {
                        url_scan();
                    }
                }
            },
        );
        row.add_row(&url);
        row.add_row(&url_error);

        let (key, key_error) = validated_entry(
            "API Key Variable",
            PROVIDER_KEY_HELP,
            provider.api_key_env.as_deref().unwrap_or(""),
        );
        let timer = self.agent_edit_timer(dialog);
        let (key_id, key_apply, key_scan) = (id.clone(), id.clone(), scan.clone());
        self.commit_only_entry(
            &key,
            &key_error,
            PROVIDER_KEY_HELP,
            &timer,
            move |imp, text, commit| {
                let saved = imp
                    .server_now(&key_id)
                    .and_then(|p| crate::config::profile_provider(&p, Driver::Local))
                    .and_then(|p| p.api_key_env);
                provider_key_edit(text, saved.as_deref(), commit)
            },
            move |imp, value| {
                if let Some(value) = value {
                    imp.edit_server(&key_apply, |p| {
                        let mut provider = crate::config::profile_provider(p, Driver::Local)
                            .unwrap_or_else(|| ModelProvider::new(ProviderKind::Ollama));
                        provider.api_key_env = value;
                        p.provider = Some(provider);
                    });
                    if !imp.is_closing() {
                        key_scan();
                    }
                }
            },
        );
        row.add_row(&key);
        row.add_row(&key_error);

        let (env_file, env_error) = validated_entry(
            "Environment File",
            ENV_FILE_HELP,
            server.env_file.as_deref().unwrap_or(""),
        );
        let timer = self.agent_edit_timer(dialog);
        let (env_id, env_apply, env_scan) = (id.clone(), id.clone(), scan.clone());
        self.commit_only_entry(
            &env_file,
            &env_error,
            ENV_FILE_HELP,
            &timer,
            move |imp, text, commit| {
                let saved = imp.server_now(&env_id).and_then(|p| p.env_file);
                env_file_edit(text, saved.as_deref(), commit)
            },
            move |imp, value| {
                if let Some(value) = value {
                    imp.edit_server(&env_apply, |p| p.env_file = value);
                    if !imp.is_closing() {
                        env_scan();
                    }
                }
            },
        );
        row.add_row(&env_file);
        row.add_row(&env_error);

        row.add_row(&scan_row);
        fill_models();
        row.add_row(&model_row);

        let mode_id = id.clone();
        let mode_row = self.choice_row(
            "Default Mode",
            "New threads start here; a thread keeps its own",
            MODES.iter().map(|m| (*m, m.label().to_owned())).collect(),
            server.default_mode.unwrap_or_default(),
            move |imp, mode| {
                imp.edit_server(&mode_id, |p| {
                    p.default_mode = mode.filter(|m| *m != Mode::Ask);
                });
            },
        );
        row.add_row(&mode_row);

        // A row with a button (libadwaita 1.5 has no button row).
        let remove_row = adw::ActionRow::builder()
            .title("Remove Server")
            .subtitle("Its threads stay")
            .build();
        let remove = gtk4::Button::builder()
            .label("Remove…")
            .valign(gtk4::Align::Center)
            .css_classes(["destructive-action"])
            .build();
        remove_row.add_suffix(&remove);
        remove.connect_clicked(glib::clone!(
            #[weak]
            obj,
            #[strong]
            id,
            move |button| {
                let Some(server) = obj.imp().server_now(&id) else {
                    return;
                };
                let confirm = adw::AlertDialog::builder()
                    .heading(format!("Remove “{}”?", server.name))
                    .body(
                        "Its threads stay. Opened again, they run on another Local server, or \
                         wait for one.",
                    )
                    .build();
                confirm.add_responses(&[("cancel", "Cancel"), ("remove", "Remove")]);
                confirm.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
                confirm.set_default_response(Some("cancel"));
                confirm.set_close_response("cancel");
                let rebuild = rebuild.clone();
                let id = id.clone();
                confirm.connect_response(
                    None,
                    glib::clone!(
                        #[weak]
                        obj,
                        move |_, response| {
                            if response != "remove" {
                                return;
                            }
                            let imp = obj.imp();
                            if imp.config.borrow_mut().remove_profile(&id) {
                                imp.schedule_config_save();
                                imp.refresh_profile_selection();
                                imp.refresh_agent_data(super::super::threads::Probes::Force);
                            }
                            if let Some(rebuild) = &rebuild {
                                rebuild();
                            }
                        }
                    ),
                );
                confirm.present(Some(button));
            }
        ));
        row.add_row(&remove_row);

        scan();
        row
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scan_names_the_chat_models_and_leaves_embedding_ones_out() {
        use agent_core::provider::ModelInfo;
        let models = vec![
            ModelInfo {
                id: "qwen3.5:9b".into(),
                chat: true,
                tools: Some(true),
            },
            ModelInfo {
                id: "nomic-embed-text:latest".into(),
                chat: false,
                tools: Some(false),
            },
            ModelInfo {
                id: "qwen2.5:0.5b".into(),
                chat: true,
                tools: Some(false),
            },
        ];
        assert_eq!(
            scan_summary("http://192.0.2.1:11434", &models).unwrap(),
            "2 models at http://192.0.2.1:11434: qwen3.5:9b, qwen2.5:0.5b (no tools) \
             (1 embedding model not offered)"
        );
        let many: Vec<ModelInfo> = (0..10)
            .map(|i| ModelInfo::by_name(&format!("m{i}")))
            .collect();
        assert!(scan_summary("b", &many).unwrap().ends_with(" and 2 more"));
        assert!(scan_summary("b", &[])
            .unwrap_err()
            .contains("lists no models"));
        assert!(scan_summary("b", &[ModelInfo::by_name("bge-embed")])
            .unwrap_err()
            .contains("only embedding"));
    }
}
