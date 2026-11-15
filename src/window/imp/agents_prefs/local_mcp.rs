//! Explicit Local MCP configuration. Constructing these rows never starts a process.
use super::local_context::show_outcome;
use super::*;
use agent_core::local_mcp::{ApprovalPolicy, EnvironmentValue, McpServerConfig, McpToolPolicy};
use std::collections::BTreeMap;

type Refill = Rc<dyn Fn()>;
mod import;

fn new_server(id: &str) -> McpServerConfig {
    McpServerConfig {
        id: id.into(),
        label: id.into(),
        enabled: false,
        required: false,
        command: "mcp-server".into(),
        args: vec![],
        cwd: "$WORKSPACE".into(),
        env: BTreeMap::new(),
        tools: vec![],
        approval: ApprovalPolicy::Ask,
        startup_timeout_secs: 15,
        call_timeout_secs: 60,
    }
}

fn names(text: &str) -> Result<Vec<String>, String> {
    serde_json::from_str(text).map_err(|_| {
        "Enter a JSON array of exact names, for example [\"search\", \"read\"].".into()
    })
}

impl AgentTerminalWindow {
    #[allow(clippy::too_many_arguments)]
    fn mcp_entry(
        &self,
        dialog: &adw::PreferencesDialog,
        parent: &adw::ExpanderRow,
        server: &McpServerConfig,
        title: &str,
        help: &'static str,
        saved: String,
        edit: impl Fn(&mut McpServerConfig, &str) -> Result<(), String> + 'static,
        after: Option<Refill>,
    ) {
        let id = server.id.clone();
        let apply_id = id.clone();
        let (entry, error) = validated_entry(title, help, &saved);
        let (weak_entry, weak_error) = (entry.downgrade(), error.downgrade());
        self.commit_only_entry(
            &entry,
            &error,
            help,
            &self.agent_edit_timer(dialog),
            move |imp, text, commit| {
                let mut value = imp.config.borrow().local_defaults.clone();
                let Some(server) = value.mcp_servers.iter_mut().find(|s| s.id == id) else {
                    return FieldChange::Invalid("This server no longer exists.".into());
                };
                let parsed = edit(server, text)
                    .and_then(|()| local_context::validate_defaults(imp, &value))
                    .map(|()| value);
                crate::config::field_change(parsed, &imp.config.borrow().local_defaults, commit)
            },
            move |imp, value| {
                let Some(value) = value else {
                    return;
                };
                if !value.mcp_servers.iter().any(|s| s.id == apply_id) {
                    return;
                }
                match imp.save_local_defaults(value) {
                    Ok(()) => {
                        if let Some(after) = &after {
                            after();
                        }
                    }
                    Err(why) => {
                        if let (Some(entry), Some(error)) =
                            (weak_entry.upgrade(), weak_error.upgrade())
                        {
                            show_validation(&entry, &error, help, Some(&why));
                        }
                    }
                }
            },
        );
        parent.add_row(&entry);
        parent.add_row(&error);
    }

    fn mcp_policy(
        &self,
        parent: &adw::ExpanderRow,
        server_id: &str,
        name: Option<&str>,
        policy: ApprovalPolicy,
    ) {
        let obj = self.obj();
        let options = if name.is_some() {
            vec![
                ApprovalPolicy::Inherit,
                ApprovalPolicy::Ask,
                ApprovalPolicy::Allow,
                ApprovalPolicy::Deny,
            ]
        } else {
            vec![
                ApprovalPolicy::Ask,
                ApprovalPolicy::Allow,
                ApprovalPolicy::Deny,
            ]
        };
        let labels: Vec<_> = options
            .iter()
            .map(|p| match p {
                ApprovalPolicy::Inherit => "Inherit server",
                ApprovalPolicy::Ask => "Ask",
                ApprovalPolicy::Allow => "Allow",
                ApprovalPolicy::Deny => "Deny",
            })
            .collect();
        const POLICY_HELP: &str = "Deny always wins; a tool override only narrows the server's.";
        let row = adw::ComboRow::builder()
            .title("Approval Policy")
            .subtitle(POLICY_HELP)
            .model(&gtk4::StringList::new(&labels))
            .selected(options.iter().position(|p| *p == policy).unwrap_or(0) as u32)
            .build();
        let id = server_id.to_owned();
        let name = name.map(str::to_owned);
        row.connect_selected_notify(glib::clone!(
            #[weak]
            obj,
            move |row| {
                let Some(policy) = options.get(row.selected() as usize).copied() else {
                    return;
                };
                let imp = obj.imp();
                let saved = imp.config.borrow().local_defaults.clone();
                let mut value = saved.clone();
                let mut previous = None;
                if let Some(server) = value.mcp_servers.iter_mut().find(|s| s.id == id) {
                    if let Some(name) = &name {
                        if let Some(tool) = server.tools.iter_mut().find(|t| &t.name == name) {
                            previous = Some(std::mem::replace(&mut tool.approval, policy));
                        }
                    } else {
                        previous = Some(std::mem::replace(&mut server.approval, policy));
                    }
                }
                if value == saved {
                    // Nothing to save, or a revert's own notification.
                    return;
                }
                let outcome = imp.save_local_defaults(value);
                if outcome.is_err() {
                    if let Some(index) = previous.and_then(|p| options.iter().position(|o| *o == p))
                    {
                        row.set_selected(index as u32);
                    }
                }
                show_outcome(row.upcast_ref::<adw::ActionRow>(), POLICY_HELP, &outcome);
            }
        ));
        parent.add_row(&row);
    }

    fn mcp_tools(
        &self,
        dialog: &adw::PreferencesDialog,
        parent: &adw::ExpanderRow,
        server: &McpServerConfig,
    ) {
        let obj = self.obj();
        let tools=adw::ExpanderRow::builder().title("Allowed Tools, in Priority Order").subtitle("Exact names only. Read-only and approval are separate; server annotations grant neither.").build();
        let rows: Rc<RefCell<Vec<adw::ExpanderRow>>> = Rc::default();
        let refill: Rc<RefCell<Option<Refill>>> = Rc::default();
        let weak = Rc::downgrade(&refill);
        let id = server.id.clone();
        let fill: Refill = Rc::new(glib::clone!(
            #[weak]
            obj,
            #[weak]
            tools,
            #[strong]
            rows,
            move || {
                for row in rows.borrow_mut().drain(..) {
                    tools.remove(&row);
                }
                let Some(server) = obj
                    .imp()
                    .config
                    .borrow()
                    .local_defaults
                    .mcp_servers
                    .iter()
                    .find(|s| s.id == id)
                    .cloned()
                else {
                    return;
                };
                for (index, policy) in server.tools.iter().enumerate() {
                    let row = adw::ExpanderRow::builder()
                        .title(&policy.name)
                        .use_markup(false)
                        .build();
                    const READ_HELP: &str =
                        "Eligible in Plan; still asks unless separately approved";
                    let read = adw::SwitchRow::builder()
                        .title("Declared Read-only")
                        .subtitle(READ_HELP)
                        .active(policy.read_only)
                        .build();
                    let (sid, name) = (id.clone(), policy.name.clone());
                    read.connect_active_notify(glib::clone!(
                        #[weak]
                        obj,
                        move |r| {
                            let imp = obj.imp();
                            let saved = imp.config.borrow().local_defaults.clone();
                            let mut value = saved.clone();
                            if let Some(t) = value
                                .mcp_servers
                                .iter_mut()
                                .find(|s| s.id == sid)
                                .and_then(|s| s.tools.iter_mut().find(|t| t.name == name))
                            {
                                t.read_only = r.is_active();
                            }
                            if value == saved {
                                return;
                            }
                            let outcome = imp.save_local_defaults(value);
                            if outcome.is_err() {
                                r.set_active(!r.is_active());
                            }
                            show_outcome(r.upcast_ref::<adw::ActionRow>(), READ_HELP, &outcome);
                        }
                    ));
                    row.add_row(&read);
                    obj.imp()
                        .mcp_policy(&row, &id, Some(&policy.name), policy.approval);
                    for (label, delta) in [("↑", -1isize), ("↓", 1)] {
                        let b = gtk4::Button::builder()
                            .label(label)
                            .tooltip_text("Move tool priority")
                            .valign(gtk4::Align::Center)
                            .build();
                        let next = index.checked_add_signed(delta);
                        b.set_sensitive(next.is_some_and(|n| n < server.tools.len()));
                        let sid = id.clone();
                        let name = policy.name.clone();
                        let weak = weak.clone();
                        b.connect_clicked(glib::clone!(
                            #[weak]
                            obj,
                            #[weak]
                            row,
                            move |_| {
                                let mut value = obj.imp().config.borrow().local_defaults.clone();
                                if let Some(s) = value.mcp_servers.iter_mut().find(|s| s.id == sid)
                                {
                                    if s.tools.get(index).is_some_and(|t| t.name == name) {
                                        if let Some(n) = next.filter(|n| *n < s.tools.len()) {
                                            s.tools.swap(index, n);
                                        }
                                    }
                                }
                                let outcome = obj.imp().save_local_defaults(value);
                                show_outcome(&row, "", &outcome);
                                if outcome.is_err() {
                                    return;
                                }
                                if let Some(fill) = weak.upgrade().and_then(|r| r.borrow().clone())
                                {
                                    fill();
                                }
                            }
                        ));
                        row.add_suffix(&b);
                    }
                    tools.add_row(&row);
                    rows.borrow_mut().push(row);
                }
            }
        ));
        *refill.borrow_mut() = Some(fill.clone());
        fill();
        self.mcp_entry(dialog,parent,server,"Exact Tool Allowlist", "A JSON array of exact discovered tool names, in priority order. New entries start unreviewed: not read-only, approval inherited.",serde_json::to_string(&server.tools.iter().map(|t|&t.name).collect::<Vec<_>>()).unwrap_or_default(),|s,text| {
            let names=names(text)?;let old=std::mem::take(&mut s.tools);
            s.tools=names.into_iter().map(|name|old.iter().find(|t|t.name==name).cloned().unwrap_or(McpToolPolicy{name,read_only:false,approval:ApprovalPolicy::Inherit})).collect();Ok(())
        },Some(Rc::new(move ||{if let Some(fill)=refill.borrow().clone(){fill();}})));
        parent.add_row(&tools);
    }

    fn mcp_server_row(
        &self,
        dialog: &adw::PreferencesDialog,
        server: &McpServerConfig,
        profiles: Rc<Vec<String>>,
        discovery_profile: &adw::ComboRow,
        folder: &adw::EntryRow,
    ) -> adw::ExpanderRow {
        let obj = self.obj();
        let row=adw::ExpanderRow::builder().title(format!("{} ({})",server.label,server.id)).subtitle("Definitions are shared. Select this ID on each Local model profile. No process starts until a turn or explicit discovery.").use_markup(false).build();
        for (title, subtitle, active, required) in [
            (
                "Enabled",
                "Off in Settings stays off: a thread's session toggle cannot re-enable it",
                server.enabled,
                false,
            ),
            (
                "Required for Turns",
                "A turn is blocked when this server fails to start",
                server.required,
                true,
            ),
        ] {
            let toggle = adw::SwitchRow::builder()
                .title(title)
                .subtitle(subtitle)
                .active(active)
                .build();
            let id = server.id.clone();
            toggle.connect_active_notify(glib::clone!(
                #[weak]
                obj,
                move |t| {
                    let imp = obj.imp();
                    let saved = imp.config.borrow().local_defaults.clone();
                    let mut value = saved.clone();
                    if let Some(s) = value.mcp_servers.iter_mut().find(|s| s.id == id) {
                        if required {
                            s.required = t.is_active();
                        } else {
                            s.enabled = t.is_active();
                        }
                    }
                    if value == saved {
                        // Nothing to save, or a revert's own notification.
                        return;
                    }
                    let outcome = imp.save_local_defaults(value);
                    if outcome.is_err() {
                        t.set_active(!t.is_active());
                    }
                    show_outcome(t.upcast_ref::<adw::ActionRow>(), subtitle, &outcome);
                }
            ));
            row.add_row(&toggle);
        }
        self.mcp_entry(
            dialog,
            &row,
            server,
            "Label",
            "A unique display label.",
            server.label.clone(),
            |s, v| {
                s.label = v.trim().into();
                Ok(())
            },
            None,
        );
        self.mcp_entry(
            dialog,
            &row,
            server,
            "Executable",
            "An executable path or command name. Arguments run directly; no shell expansion.",
            server.command.clone(),
            |s, v| {
                s.command = v.trim().into();
                Ok(())
            },
            None,
        );
        self.mcp_entry(
            dialog,
            &row,
            server,
            "Arguments",
            "A JSON array of argument strings. Values are passed exactly; no shell expansion.",
            serde_json::to_string(&server.args).unwrap_or_default(),
            |s, v| {
                s.args = serde_json::from_str(v)
                    .map_err(|_| "Enter a JSON array of argument strings.")?;
                Ok(())
            },
            None,
        );
        self.mcp_entry(
            dialog,
            &row,
            server,
            "Working Folder",
            "An absolute path, a path relative to the thread folder, or $WORKSPACE.",
            server.cwd.clone(),
            |s, v| {
                s.cwd = v.trim().into();
                Ok(())
            },
            None,
        );
        let references: BTreeMap<_, _> = server
            .env
            .iter()
            .filter_map(|(name, v)| match v {
                EnvironmentValue::Reference { from_env } => Some((name, from_env)),
                _ => None,
            })
            .collect();
        self.mcp_entry(dialog,&row,server,"Environment References","A JSON object mapping exported variable names to source variable names. No values. Existing non-secret literals are retained unless replaced by a reference.",serde_json::to_string(&references).unwrap_or_default(),|s,v|{
            let refs:BTreeMap<String,String>=serde_json::from_str(v).map_err(|_|"Enter an object of variable names mapped to variable names, never credentials.")?;
            s.env.retain(|_,v|matches!(v,EnvironmentValue::Literal(_)));
            s.env.extend(refs.into_iter().map(|(k,from_env)|(k,EnvironmentValue::Reference{from_env})));Ok(())
        },None);
        for (title, n, startup) in [
            (
                "Startup Timeout (seconds)",
                server.startup_timeout_secs,
                true,
            ),
            ("Call Timeout (seconds)", server.call_timeout_secs, false),
        ] {
            self.mcp_entry(
                dialog,
                &row,
                server,
                title,
                "An integer; startup 1–300 seconds, calls 1–600 seconds.",
                n.to_string(),
                move |s, v| {
                    let n = v.trim().parse().map_err(|_| "Enter a positive integer.")?;
                    if startup {
                        s.startup_timeout_secs = n;
                    } else {
                        s.call_timeout_secs = n;
                    }
                    Ok(())
                },
                None,
            );
        }
        self.mcp_policy(&row, &server.id, None, server.approval);
        self.mcp_tools(dialog, &row, server);
        let output = adw::ActionRow::builder()
            .title("Discovery Preview")
            .subtitle(
                "No discovery has run. Names only; discovery does not allowlist or approve tools.",
            )
            .subtitle_lines(0)
            .use_markup(false)
            .build();
        let button = gtk4::Button::builder()
            .label("Discover tools")
            .valign(gtk4::Align::Center)
            .build();
        output.add_suffix(&button);
        row.add_row(&output);
        let id = server.id.clone();
        button.connect_clicked(glib::clone!(
            #[weak]
            obj,
            #[weak]
            output,
            #[weak]
            discovery_profile,
            #[weak]
            folder,
            move |button| {
                let imp = obj.imp();
                let profile = profiles
                    .get(discovery_profile.selected() as usize)
                    .and_then(|id| imp.config.borrow().profile_by_id(id).cloned());
                let Some(profile) = profile else {
                    output.set_subtitle("Choose a Local profile first.");
                    return;
                };
                let Some(mut server) = imp
                    .config
                    .borrow()
                    .local_defaults
                    .mcp_servers
                    .iter()
                    .find(|s| s.id == id)
                    .cloned()
                else {
                    return;
                };
                // This button is explicit permission to start this one temporary discovery process.
                server.enabled = true;
                let clear = imp.config.borrow().clear_env.clone();
                let path = folder.text().to_string();
                let button = button.downgrade();
                let output = output.downgrade();
                if let Some(b) = button.upgrade() {
                    b.set_sensitive(false);
                }
                glib::spawn_future_local(async move {
                    let profile = super::super::threads::resolve_agent(profile).await;
                    let env = super::super::threads::agent_env(profile.env, &clear);
                    let mut keys: Vec<String> = [
                        "PATH",
                        "HOME",
                        "LANG",
                        "LC_ALL",
                        "LC_CTYPE",
                        "TMPDIR",
                        "XDG_RUNTIME_DIR",
                        "SYSTEMROOT",
                    ]
                    .into_iter()
                    .map(str::to_owned)
                    .collect();
                    keys.extend(server.env.values().filter_map(|v| match v {
                        EnvironmentValue::Reference { from_env } => Some(from_env.clone()),
                        _ => None,
                    }));
                    let effective = keys
                        .into_iter()
                        .filter_map(|key| env.lookup(&key).map(|v| (key, v)))
                        .collect();
                    let path = agent_kit::paths::expand_tilde_with(
                        &path,
                        &env.lookup("HOME").unwrap_or_default(),
                    );
                    let result = off_main("MCP discovery", move || {
                        if path.trim().is_empty() {
                            return Err("Enter a discovery folder.".into());
                        }
                        let timeout =
                            std::time::Duration::from_secs(server.startup_timeout_secs + 5);
                        let mut session = agent_kit::local_mcp::Session::with_environment(
                            vec![server],
                            std::path::Path::new(&path),
                            effective,
                        )?;
                        let result = session
                            .discover(Mode::Ask)?
                            .recv_timeout(timeout)
                            .map_err(|_| "Discovery timed out.".to_owned());
                        session.shutdown();
                        result.map(|d| {
                            d.servers
                                .into_iter()
                                .map(|s| {
                                    format!(
                                        "{}\n{}\n{}",
                                        s.id,
                                        s.available_tools.join(", "),
                                        s.notices.join("\n")
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                    })
                    .await;
                    if let Some(o) = output.upgrade() {
                        o.set_subtitle(&result.unwrap_or_else(|e| e));
                    }
                    if let Some(b) = button.upgrade() {
                        b.set_sensitive(true);
                    }
                });
            }
        ));
        let remove = gtk4::Button::builder()
            .label("Remove definition")
            .valign(gtk4::Align::Center)
            .build();
        const REMOVE_HELP: &str =
            "Stops affected running Local sessions; removed definitions are no longer selected.";
        let remove_row = adw::ActionRow::builder()
            .title("Remove Server and Profile Selections")
            .subtitle(REMOVE_HELP)
            .subtitle_lines(0)
            .use_markup(false)
            .build();
        remove_row.add_suffix(&remove);
        row.add_row(&remove_row);
        let id = server.id.clone();
        remove.connect_clicked(glib::clone!(
            #[weak]
            obj,
            #[weak]
            row,
            #[weak]
            remove_row,
            move |_| {
                let imp = obj.imp();
                let mut config = imp.config.borrow().clone();
                config.local_defaults.mcp_servers.retain(|s| s.id != id);
                for profile in &mut config.profiles {
                    profile.local.mcp_server_ids.retain(|s| s != &id);
                }
                let outcome = config.local_defaults.validate().and_then(|()| {
                    config
                        .profiles
                        .iter()
                        .try_for_each(|p| config.local_defaults.launch(&p.local).map(|_| ()))
                });
                show_outcome(&remove_row, REMOVE_HELP, &outcome);
                if outcome.is_err() {
                    return;
                }
                *imp.config.borrow_mut() = config;
                imp.schedule_config_save();
                imp.refresh_local_contexts();
                row.set_visible(false);
            }
        ));
        row
    }

    pub(super) fn local_mcp_rows(
        &self,
        dialog: &adw::PreferencesDialog,
        group: &adw::PreferencesGroup,
    ) {
        let obj = self.obj();
        let profiles: Vec<_> = self
            .config
            .borrow()
            .local_servers()
            .iter()
            .map(|p| (p.id.clone(), p.name.clone()))
            .collect();
        let labels: Vec<_> = profiles.iter().map(|(_, name)| name.as_str()).collect();
        let profile = adw::ComboRow::builder()
            .title("Discovery Environment Profile")
            .model(&gtk4::StringList::new(&labels))
            .build();
        group.add(&profile);
        let ids = Rc::new(profiles.into_iter().map(|(id, _)| id).collect::<Vec<_>>());
        let folder = adw::EntryRow::builder()
            .title("Discovery Folder")
            .text(&self.config.borrow().starting_directory)
            .build();
        group.add(&folder);
        let servers=adw::ExpanderRow::builder().title("MCP Server Definitions").subtitle("Disabled definitions are safe to review. Discover tools is the only Settings action that starts a server.").build();
        group.add(&servers);
        for server in &self.config.borrow().local_defaults.mcp_servers {
            servers.add_row(&self.mcp_server_row(dialog, server, ids.clone(), &profile, &folder));
        }
        const ID_HELP: &str = "A unique stable ID: lowercase letters, digits and hyphens.";
        let (entry,error)=validated_entry("Add MCP Server ID","A unique stable ID: lowercase letters, digits and hyphens. New definitions are disabled and unreviewed.","");
        let weak = entry.downgrade();
        let weak_error = error.downgrade();
        let weak_dialog = dialog.downgrade();
        self.commit_only_entry(
            &entry,
            &error,
            ID_HELP,
            &self.agent_edit_timer(dialog),
            |imp, text, commit| {
                if text.trim().is_empty() {
                    return FieldChange::Keep;
                }
                let mut value = imp.config.borrow().local_defaults.clone();
                value.mcp_servers.push(new_server(text.trim()));
                crate::config::field_change(
                    local_context::validate_defaults(imp, &value).map(|()| value),
                    &imp.config.borrow().local_defaults,
                    commit,
                )
            },
            glib::clone!(
                #[weak]
                obj,
                #[weak]
                servers,
                #[weak]
                profile,
                #[weak]
                folder,
                move |imp: &AgentTerminalWindow,
                      value: Option<agent_core::local_context::LocalDefaults>| {
                    let Some(value) = value else {
                        return;
                    };
                    let last = value.mcp_servers.last().cloned();
                    if let Err(why) = imp.save_local_defaults(value) {
                        if let (Some(entry), Some(error)) = (weak.upgrade(), weak_error.upgrade()) {
                            show_validation(&entry, &error, ID_HELP, Some(&why));
                        }
                        return;
                    }
                    if let (Some(server), Some(dialog)) = (last, weak_dialog.upgrade()) {
                        servers.add_row(&obj.imp().mcp_server_row(
                            &dialog,
                            &server,
                            ids.clone(),
                            &profile,
                            &folder,
                        ));
                    }
                    if let Some(entry) = weak.upgrade() {
                        entry.set_text("");
                    }
                }
            ),
        );
        group.add(&entry);
        group.add(&error);
        self.local_codex_import_rows(dialog, group, &servers, &profile, &folder);
    }

    pub(super) fn local_profile_mcp_rows(
        &self,
        profile: &Profile,
        dialog: &adw::PreferencesDialog,
        parent: &adw::ExpanderRow,
    ) {
        let id = profile.id.clone();
        let apply_id = id.clone();
        let (entry,error)=validated_entry("Selected MCP Server IDs, in Priority Order","A JSON array of configured IDs. Built-ins fit first, then servers and their allowlisted tools in this order.",&serde_json::to_string(&profile.local.mcp_server_ids).unwrap_or_default());
        self.commit_only_entry(
            &entry,
            &error,
            "Exact configured server IDs, in priority order.",
            &self.agent_edit_timer(dialog),
            move |imp, text, commit| {
                let config = imp.config.borrow();
                let Some(profile) = config.profile_by_id(&id) else {
                    return FieldChange::Invalid("Profile no longer exists.".into());
                };
                let mut options = profile.local.clone();
                let parsed = names(text).and_then(|ids| {
                    options.mcp_server_ids = ids;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_mcp_settings_are_inert_and_validate_committed_fields() {
        crate::testutil::in_loop(|_| {
            let app =
                crate::window::imp::test_support::test_app("org.test.LocalMcpSettings", false);
            let window = crate::window::AgentTerminalWindow::new(&app);
            let folder = tempfile::tempdir().unwrap();
            let marker = folder.path().join("started");
            let mut server = new_server("fixture");
            server.enabled = true;
            server.command = "/bin/sh".into();
            server.args = vec!["-c".into(), format!("touch {}", marker.display())];
            window
                .imp()
                .config
                .borrow_mut()
                .local_defaults
                .mcp_servers
                .push(server.clone());
            let dialog = adw::PreferencesDialog::new();
            let group = window.imp().local_context_group(&dialog);
            let entries = crate::testutil::find_all::<adw::EntryRow>(group.upcast_ref());
            let entry = |title: &str| entries.iter().find(|e| e.title() == title).unwrap();
            assert!(
                !marker.exists(),
                "opening Settings must not launch a server"
            );
            entry("Arguments").set_text("not an argument array");
            entry("Arguments").emit_by_name::<()>("apply", &[]);
            entry("Call Timeout (seconds)").set_text("601");
            entry("Call Timeout (seconds)").emit_by_name::<()>("apply", &[]);
            let saved = window.imp().config.borrow().local_defaults.mcp_servers[0].clone();
            assert_eq!(saved, server);
            assert!(entry("Arguments").has_css_class("error"));
            entry("Label").set_text("Changed");
            entry("Label").emit_by_name::<()>("apply", &[]);
            assert_eq!(
                window.imp().config.borrow().local_defaults.mcp_servers[0].label,
                "Changed"
            );
            assert!(!marker.exists());
            window.close();
        });
    }

    #[test]
    fn refused_switch_and_remove_revert_the_widget_and_show_the_reason() {
        crate::testutil::in_loop(|_| {
            let app = crate::window::imp::test_support::test_app("org.test.LocalMcpRefused", false);
            let window = crate::window::AgentTerminalWindow::new(&app);
            // Three definitions sharing a label: every save, and every removal of one of them,
            // leaves a duplicate, so validation refuses it.
            for id in ["first", "second", "third"] {
                let mut server = new_server(id);
                server.label = "shared".into();
                window
                    .imp()
                    .config
                    .borrow_mut()
                    .local_defaults
                    .mcp_servers
                    .push(server);
            }
            let dialog = adw::PreferencesDialog::new();
            let group = window.imp().local_context_group(&dialog);
            let switches = crate::testutil::find_all::<adw::SwitchRow>(group.upcast_ref());
            let enabled = switches.iter().find(|s| s.title() == "Enabled").unwrap();
            let original = enabled.subtitle().unwrap().to_string();
            enabled.set_active(true);
            assert!(!enabled.is_active(), "a refused save reverts the switch");
            assert!(enabled.has_css_class("error"));
            assert!(enabled.subtitle().unwrap().contains("unique"));
            assert!(!window.imp().config.borrow().local_defaults.mcp_servers[0].enabled);
            let combos = crate::testutil::find_all::<adw::ComboRow>(group.upcast_ref());
            let policy = combos
                .iter()
                .find(|c| c.title() == "Approval Policy")
                .unwrap();
            let chosen = policy.selected();
            policy.set_selected(chosen + 1);
            assert_eq!(
                policy.selected(),
                chosen,
                "a refused save reverts the dropdown"
            );
            assert!(policy.has_css_class("error"));
            assert!(policy.subtitle().unwrap().contains("unique"));
            assert_eq!(
                window.imp().config.borrow().local_defaults.mcp_servers[0].approval,
                ApprovalPolicy::Ask
            );
            let buttons = crate::testutil::find_all::<gtk4::Button>(group.upcast_ref());
            let remove = buttons
                .iter()
                .find(|b| b.label().as_deref() == Some("Remove definition"))
                .unwrap();
            let rows = crate::testutil::find_all::<adw::ActionRow>(group.upcast_ref());
            let remove_row = rows
                .iter()
                .find(|r| r.title() == "Remove Server and Profile Selections")
                .unwrap();
            remove.emit_clicked();
            assert_eq!(
                window
                    .imp()
                    .config
                    .borrow()
                    .local_defaults
                    .mcp_servers
                    .len(),
                3
            );
            assert!(remove_row.has_css_class("error"));
            assert!(remove_row.subtitle().unwrap().contains("unique"));
            // Once the saved state is valid again, the next accepted save clears the notice.
            for (index, server) in window
                .imp()
                .config
                .borrow_mut()
                .local_defaults
                .mcp_servers
                .iter_mut()
                .enumerate()
            {
                server.label = format!("label-{index}");
            }
            enabled.set_active(true);
            assert!(enabled.is_active());
            assert!(!enabled.has_css_class("error"));
            assert_eq!(enabled.subtitle().unwrap(), original);
            assert!(window.imp().config.borrow().local_defaults.mcp_servers[0].enabled);
            window.close();
        });
    }

    #[test]
    fn new_definitions_are_disabled_and_unreviewed_and_names_remain_exact() {
        let server = new_server("example");
        assert!(server.validate().is_ok());
        assert!(!server.enabled);
        assert!(server.tools.is_empty());
        assert_eq!(
            names(r#"["search", "read tool"]"#).unwrap(),
            ["search", "read tool"]
        );
        assert!(names("search,read").is_err());
        assert!(names(r#"[1]"#).is_err());
    }
}
