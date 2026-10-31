//! A user-selected, one-shot import preview. No import path is saved or read at launch.
use super::*;
use agent_kit::local_mcp::import::{default_codex_path, read_codex, ImportPreview};

impl AgentTerminalWindow {
    pub(super) fn local_codex_import_rows(
        &self,
        dialog: &adw::PreferencesDialog,
        group: &adw::PreferencesGroup,
        servers: &adw::ExpanderRow,
        discovery_profile: &adw::ComboRow,
        folder: &adw::EntryRow,
    ) {
        let obj = self.obj();
        let codex_home = std::env::var_os("CODEX_HOME").map(std::path::PathBuf::from);
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let default = default_codex_path(codex_home.as_deref(), home.as_deref())
            .ok()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let source = adw::EntryRow::builder()
            .title("Codex Configuration to Import")
            .text(default)
            .build();
        let choose = gtk4::Button::builder()
            .label("Choose file")
            .valign(gtk4::Align::Center)
            .build();
        source.add_suffix(&choose);
        choose.connect_clicked(glib::clone!(
            #[weak]
            obj,
            #[weak]
            source,
            move |_| {
                let picker = gtk4::FileDialog::builder()
                    .title("Choose Codex configuration")
                    .build();
                let obj = obj.downgrade();
                let source = source.downgrade();
                glib::spawn_future_local(async move {
                    if let Some(obj) = obj.upgrade() {
                        if let Ok(file) = picker.open_future(Some(&obj)).await {
                            if let (Some(path), Some(source)) = (file.path(), source.upgrade()) {
                                source.set_text(&path.display().to_string());
                            }
                        }
                    }
                });
            }
        ));
        let preview_button = gtk4::Button::builder()
            .label("Preview import")
            .valign(gtk4::Align::Center)
            .build();
        source.add_suffix(&preview_button);
        group.add(&source);
        let preview_row=adw::ExpanderRow::builder().title("Codex Import Preview").subtitle("Stdio only. No approval exemptions or read-only declarations are imported. Opening Settings reads no Codex configuration and starts no MCP server.").build();
        group.add(&preview_row);
        let summary=adw::ActionRow::builder().title("Import Status").subtitle("Choose a file, then Preview import. Environment names only; credential values are never shown.").subtitle_lines(0).use_markup(false).build();
        preview_row.add_row(&summary);
        let confirm = gtk4::Button::builder()
            .label("Import selected")
            .sensitive(false)
            .valign(gtk4::Align::Center)
            .build();
        summary.add_suffix(&confirm);
        let data: Rc<RefCell<Option<ImportPreview>>> = Rc::default();
        let selections: Rc<RefCell<Vec<(adw::SwitchRow, adw::EntryRow, adw::ActionRow)>>> =
            Rc::default();
        let weak_dialog = dialog.downgrade();
        preview_button.connect_clicked(glib::clone!(#[weak]source,#[weak]folder,#[weak]preview_row,#[weak]summary,#[weak]confirm,#[strong]data,#[strong]selections,move |button| {
            confirm.set_sensitive(false);data.borrow_mut().take();
            for (row,entry,error) in selections.borrow_mut().drain(..){preview_row.remove(&row);preview_row.remove(&entry);preview_row.remove(&error);}
            let path=std::path::PathBuf::from(source.text().as_str());let cwd=std::path::PathBuf::from(folder.text().as_str());
            let button=button.downgrade();let output=summary.downgrade();let preview_row=preview_row.downgrade();let confirm=confirm.downgrade();let data=data.clone();let selections=selections.clone();
            if let Some(button)=button.upgrade(){button.set_sensitive(false);}
            glib::spawn_future_local(async move {
                let result=off_main("Codex import preview",move || {
                    let expanded=agent_kit::paths::expand_tilde_with(&cwd.display().to_string(),&std::env::var("HOME").unwrap_or_default());
                    let cwd=std::fs::canonicalize(expanded).map_err(|_|"Choose an existing fallback folder for imported definitions.".to_owned())?;
                    read_codex(&path,&cwd).map_err(str::to_owned)
                }).await;
                if let (Some(output),Some(parent),Some(confirm))=(output.upgrade(),preview_row.upgrade(),confirm.upgrade()) {match result {
                    Ok(preview)=>{
                        output.set_subtitle(&preview.summary());
                        for server in &preview.servers {
                            let row=adw::SwitchRow::builder().title(format!("Import {}",server.id)).subtitle(format!("Executable name: {}; exact tools: {}; environment names: {}; {}",server.command_name,server.tools.join(", "),server.environment_names.join(", "),if server.reviewed{"Explicit allowlist; still no read-only declarations or approval exemptions"}else{"Disabled and unreviewed; discover and select tools before enabling"})).use_markup(false).active(false).build();
                            let refs:BTreeMap<_,_>=server.required_env_refs.iter().map(|name|(name.clone(),String::new())).collect();
                            let entry=adw::EntryRow::builder().title(format!("{}: Environment Reference Names",server.id)).text(serde_json::to_string(&refs).unwrap_or_default()).build();
                            let error=adw::ActionRow::builder().title("Reference Mapping").subtitle("Map each required exported name to a source variable name, never a credential value.").use_markup(false).subtitle_lines(0).build();
                            parent.add_row(&row);parent.add_row(&entry);parent.add_row(&error);selections.borrow_mut().push((row,entry,error));
                        }
                        confirm.set_sensitive(!preview.servers.is_empty());*data.borrow_mut()=Some(preview);
                    }
                    Err(why)=>output.set_subtitle(&why),
                }}
                if let Some(button)=button.upgrade(){button.set_sensitive(true);}
            });
        }));
        confirm.connect_clicked(glib::clone!(#[weak]obj,#[weak]summary,#[weak]servers,#[weak]discovery_profile,#[weak]folder,#[strong]data,#[strong]selections,move |button| {
            // Validate every mapping before consuming the one-shot preview or changing settings.
            let mut mappings=Vec::new();let mut selected=Vec::new();
            for (row,entry,error) in selections.borrow().iter() {
                if !row.is_active(){mappings.push(BTreeMap::new());selected.push(false);continue;}
                let parsed=serde_json::from_str::<BTreeMap<String,String>>(&entry.text()).ok().filter(|m|m.values().all(|name|agent_core::provider::valid_env_name(name)));
                let Some(parsed)=parsed else {error.set_subtitle("Invalid mapping: enter variable names only; blank references cannot be imported.");entry.add_css_class("error");return;};
                entry.remove_css_class("error");mappings.push(parsed);selected.push(true);
            }
            if !selected.iter().any(|s|*s){summary.set_subtitle("Select at least one previewed definition.");return;}
            let borrowed=data.borrow();let Some(preview)=borrowed.as_ref() else {return;};
            let mut defaults=obj.imp().config.borrow().local_defaults.clone();let mut added=Vec::new();
            for (index,server) in preview.servers.iter().cloned().enumerate() {
                if !selected[index]{continue;}
                match server.into_config(&mappings[index]) {Ok(server)=>{added.push(server.clone());defaults.mcp_servers.push(server);},Err(why)=>{let (_,entry,error)=&selections.borrow()[index];entry.add_css_class("error");error.set_subtitle(why);return;}}
            }
            if let Err(why)=local_context::validate_defaults(obj.imp(),&defaults){summary.set_subtitle(&why);return;}
            drop(borrowed);
            if let Err(why)=obj.imp().save_local_defaults(defaults){summary.set_subtitle(&why);return;}
            data.borrow_mut().take();
            if let Some(dialog)=weak_dialog.upgrade() {
                let ids=Rc::new(obj.imp().config.borrow().local_servers().iter().map(|p|p.id.clone()).collect::<Vec<_>>());
                for server in &added {servers.add_row(&obj.imp().mcp_server_row(&dialog,server,ids.clone(),&discovery_profile,&folder));}
            }
            summary.set_subtitle("Imported selected definitions once. Select their IDs on Local model profiles; unreviewed definitions remain disabled. The source file will not be reread.");button.set_sensitive(false);
        }));
    }
}
