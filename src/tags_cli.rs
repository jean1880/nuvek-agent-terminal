//! `agent-terminal tags …`: the headless way to read and edit the user's model tags, run before
//! any GTK setup like the app's other command-line roles.
//!
//! ```text
//! agent-terminal tags list [--json]
//! agent-terminal tags show  <model> [--driver D] [--profile P] [--json]
//! agent-terminal tags set   <model> [--driver D] [--profile P] [--tier T] [--tags a,b] [--best-for TEXT]
//! agent-terminal tags unset <model> [--driver D] [--profile P]
//! ```
//!
//! It writes only the user's tag file. No agent tool changes tags (the delegation tools only read
//! them); the file is the user's, so any process with the user's shell can still run this command
//! or edit the file.

use std::fmt::Write as _;
use std::path::Path;

use agent_core::adapter::Driver;
use agent_core::model_tags::{ModelTags, TagSource};
use agent_core::user_tags::Edit;
use serde_json::{json, Value};

use crate::config::{Profile, TerminalConfig};
use crate::model_tags_file::load_now;
use crate::tag_edit::{self, Target};

const USAGE: &str = "usage:
  agent-terminal tags list [--json]
  agent-terminal tags show <model> [--driver D] [--profile P] [--json]
  agent-terminal tags set <model> [--driver D] [--profile P] [--tier T] [--tags a,b] [--best-for TEXT]
  agent-terminal tags unset <model> [--driver D] [--profile P]

A model is its exact id. --driver is claude, agy, codex or local; --profile is a profile id or
name (a profile scopes the entry to its server and implies its driver). Only the user's tag file
is written. No agent tool changes tags, but any process with your shell can run this command.";

/// The files a run reads and writes.
pub struct Paths<'a> {
    /// The shared tag file (read only).
    pub file: &'a Path,
    /// The user's tag file (the only one written).
    pub user: &'a Path,
}

/// What the arguments after `tags` asked for.
#[derive(Debug, Default, PartialEq, Eq)]
struct Options {
    command: String,
    model: Option<String>,
    driver: Option<String>,
    profile: Option<String>,
    tier: Option<String>,
    tags: Option<String>,
    best_for: Option<String>,
    json: bool,
}

fn parse_args(args: &[String]) -> Result<Options, String> {
    let mut it = args.iter();
    let command = it.next().ok_or("a command is required")?.clone();
    let mut o = Options {
        command,
        ..Options::default()
    };
    while let Some(arg) = it.next() {
        let mut value = |name: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match arg.as_str() {
            "--json" => o.json = true,
            "--driver" => o.driver = Some(value("--driver")?),
            "--profile" => o.profile = Some(value("--profile")?),
            "--tier" => o.tier = Some(value("--tier")?),
            "--tags" => o.tags = Some(value("--tags")?),
            "--best-for" => o.best_for = Some(value("--best-for")?),
            flag if flag.starts_with("--") => return Err(format!("unknown option {flag}")),
            model if o.model.is_none() => o.model = Some(model.to_owned()),
            extra => return Err(format!("unexpected argument {extra}")),
        }
    }
    Ok(o)
}

fn driver_named(name: &str) -> Result<Driver, String> {
    Driver::ALL
        .into_iter()
        .find(|d| d.info().key.eq_ignore_ascii_case(name.trim()))
        .ok_or_else(|| {
            let keys: Vec<&str> = Driver::ALL.iter().map(|d| d.info().key).collect();
            format!("unknown driver \"{name}\" (use {})", keys.join(", "))
        })
}

/// The profile `text` names: its id, else its name (case-insensitive); never ambiguous.
pub fn resolve_profile<'a>(profiles: &'a [Profile], text: &str) -> Result<&'a Profile, String> {
    let text = text.trim();
    if let Some(p) = profiles.iter().find(|p| p.id == text) {
        return Ok(p);
    }
    let mut named = profiles
        .iter()
        .filter(|p| p.name.eq_ignore_ascii_case(text));
    match (named.next(), named.next()) {
        (Some(p), None) => Ok(p),
        (Some(_), Some(_)) => Err(format!(
            "more than one profile is named \"{text}\"; use its id"
        )),
        _ => Err(format!("no profile \"{text}\"")),
    }
}

/// The target a model argument and the scope options name.
struct Resolved {
    id: String,
    driver: Option<Driver>,
    profile: Option<String>,
}

fn resolve(o: &Options, profiles: &[Profile]) -> Result<Resolved, String> {
    let id = o
        .model
        .clone()
        .ok_or("a model id is required")?
        .trim()
        .to_owned();
    let mut driver = o.driver.as_deref().map(driver_named).transpose()?;
    let mut profile = None;
    if let Some(text) = &o.profile {
        let p = resolve_profile(profiles, text)?;
        let pd = crate::config::profile_driver(p)
            .ok_or_else(|| format!("profile \"{}\" has no chat agent", p.name))?;
        if driver.is_some_and(|d| d != pd) {
            return Err(format!(
                "profile \"{}\" runs {}, not {}",
                p.name,
                pd.info().key,
                driver.map_or("", |d| d.info().key)
            ));
        }
        driver = Some(pd);
        profile = Some(p.id.clone());
    }
    Ok(Resolved {
        id,
        driver,
        profile,
    })
}

fn target(r: &Resolved) -> Target<'_> {
    Target {
        id: &r.id,
        driver: r.driver,
        profile: r.profile.as_deref(),
    }
}

fn source_name(s: TagSource) -> &'static str {
    s.as_str()
}

fn entry_json(e: &tag_edit::Effective) -> Value {
    json!({
        "tier": e.tier,
        "tags": e.tags,
        "best_for": e.best_for,
        "tags_source": source_name(e.source),
        "profile_scoped": e.scoped,
    })
}

/// One model's effective tags as lines, per driver that has any.
fn show_lines(tags: &ModelTags, r: &Resolved, json_out: bool) -> Result<String, String> {
    let drivers: Vec<Driver> = match r.driver {
        Some(d) => vec![d],
        None => Driver::ALL.to_vec(),
    };
    let found: Vec<(Driver, tag_edit::Effective)> = drivers
        .into_iter()
        .filter_map(|d| {
            tag_edit::effective(
                tags,
                Target {
                    driver: Some(d),
                    ..target(r)
                },
            )
            .map(|e| (d, e))
        })
        .collect();
    if found.is_empty() {
        return Err(format!("no tags for \"{}\"", r.id));
    }
    if json_out {
        let items: Vec<Value> = found
            .iter()
            .map(|(d, e)| {
                let mut v = entry_json(e);
                v["model"] = json!(r.id);
                v["driver"] = json!(d.info().key);
                v
            })
            .collect();
        return Ok(format!(
            "{}\n",
            serde_json::to_string_pretty(&items).map_err(|e| e.to_string())?
        ));
    }
    let mut out = String::new();
    for (d, e) in &found {
        let _ = writeln!(
            out,
            "{} ({}) [{}{}]",
            r.id,
            d.info().key,
            source_name(e.source),
            if e.scoped { ", one server" } else { "" }
        );
        let _ = writeln!(out, "  tier: {}", e.tier.as_deref().unwrap_or("-"));
        let _ = writeln!(
            out,
            "  tags: {}",
            if e.tags.is_empty() {
                "-".to_owned()
            } else {
                e.tags.join(", ")
            }
        );
        let _ = writeln!(out, "  best for: {}", e.best_for.as_deref().unwrap_or("-"));
    }
    Ok(out)
}

/// Every entry of every layer, user's first.
fn list_text(tags: &ModelTags, json_out: bool) -> Result<String, String> {
    let rows: Vec<Value> = tags
        .entries
        .iter()
        .map(|e| {
            json!({
                "match": e.pattern.text(),
                "driver": e.driver.map(|d| d.info().key),
                "profile": e.profile,
                "tier": e.tier,
                "tags": e.tags,
                "best_for": e.best_for,
                "tags_source": source_name(e.source),
            })
        })
        .collect();
    if json_out {
        return Ok(format!(
            "{}\n",
            serde_json::to_string_pretty(&rows).map_err(|e| e.to_string())?
        ));
    }
    let mut out = String::new();
    for r in &rows {
        let text = |k: &str| r[k].as_str().unwrap_or("-").to_owned();
        let tags_list = r["tags"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "{:<8} {:<28} driver={} profile={} tier={} tags={}",
            text("tags_source"),
            text("match"),
            text("driver"),
            text("profile"),
            text("tier"),
            if tags_list.is_empty() {
                "-"
            } else {
                &tags_list
            }
        );
    }
    Ok(out)
}

/// Runs `args` (everything after `tags`) against `paths`, with `profiles` to name `--profile`.
/// `Ok` is what to print; `Err` is the message for a non-zero exit.
pub fn run(args: &[String], paths: &Paths<'_>, profiles: &[Profile]) -> Result<String, String> {
    let o = parse_args(args)?;
    let state = load_now(paths.file, paths.user);
    let tags = state.tags.as_deref().cloned().unwrap_or_default();
    match o.command.as_str() {
        "list" => {
            if o.model.is_some() {
                return Err("list takes no model".into());
            }
            list_text(&tags, o.json)
        }
        "show" => show_lines(&tags, &resolve(&o, profiles)?, o.json),
        "set" => {
            let r = resolve(&o, profiles)?;
            let edit = Edit {
                tier: o.tier.clone(),
                tags: o.tags.as_deref().map(agent_core::user_tags::split_tags),
                best_for: o.best_for.clone(),
            };
            if edit == Edit::default() {
                return Err("set needs --tier, --tags or --best-for".into());
            }
            tag_edit::set(paths.user, &tags, target(&r), &edit)?;
            // What now applies, read back from the files.
            let now = load_now(paths.file, paths.user).tags.unwrap_or_default();
            show_lines(&now, &r, false)
        }
        "unset" => {
            let r = resolve(&o, profiles)?;
            if o.tier.is_some() || o.tags.is_some() || o.best_for.is_some() {
                return Err(
                    "unset removes the whole entry; it takes no --tier, --tags or --best-for"
                        .into(),
                );
            }
            if tag_edit::reset(paths.user, target(&r))? {
                let now = load_now(paths.file, paths.user).tags.unwrap_or_default();
                Ok(show_lines(&now, &r, false)
                    .unwrap_or_else(|_| format!("removed; \"{}\" has no tags now\n", r.id)))
            } else {
                Err(format!("no user entry for \"{}\"", r.id))
            }
        }
        other => Err(format!("unknown command \"{other}\"")),
    }
}

/// The entry point for `agent-terminal tags …`.
pub fn main(args: &[String]) -> i32 {
    if args
        .first()
        .is_some_and(|a| matches!(a.as_str(), "-h" | "--help" | "help"))
    {
        println!("{USAGE}");
        return 0;
    }
    let config = TerminalConfig::load();
    let (file, user) = (config.model_tags_file(), config.user_model_tags_file());
    match run(
        args,
        &Paths {
            file: &file,
            user: &user,
        },
        &config.profiles,
    ) {
        Ok(text) => {
            print!("{text}");
            0
        }
        Err(message) => {
            eprintln!("agent-terminal tags: {message}\n\n{USAGE}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use agent_core::provider::{ModelProvider, ProviderKind};

    use super::*;

    fn words(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_owned).collect()
    }

    fn server(id: &str, name: &str) -> Profile {
        Profile {
            id: id.into(),
            name: name.into(),
            driver: Some(Driver::Local),
            provider: Some(ModelProvider::new(ProviderKind::Ollama)),
            ..Profile::default()
        }
    }

    struct Env {
        _dir: tempfile::TempDir,
        file: std::path::PathBuf,
        user: std::path::PathBuf,
        profiles: Vec<Profile>,
    }

    fn env() -> Env {
        let dir = tempfile::tempdir().expect("tmp");
        Env {
            file: dir.path().join("model-tags.json"),
            user: dir.path().join("model-tags.user.json"),
            profiles: vec![server("p-small", "Small card"), server("p-big", "Big card")],
            _dir: dir,
        }
    }

    impl Env {
        fn run(&self, line: &str) -> Result<String, String> {
            run(
                &words(line),
                &Paths {
                    file: &self.file,
                    user: &self.user,
                },
                &self.profiles,
            )
        }
    }

    #[test]
    fn set_show_and_unset_write_only_the_user_file_and_print_the_effective_tags() {
        let e = env();
        let shared = r#"{"version": 1, "models": [{"match": "tiny", "driver": "local", "tier": "deep", "tags": ["code"]}]}"#;
        std::fs::write(&e.file, shared).expect("write");
        let out = e
            .run("set tiny --driver local --tier fast --tags fast,bulk --best-for cheap-sweeps")
            .expect("set");
        assert!(out.contains("tiny (local) [user]"), "{out}");
        assert!(
            out.contains("tier: fast") && out.contains("tags: fast, bulk"),
            "{out}"
        );
        assert!(out.contains("best for: cheap-sweeps"), "{out}");
        assert_eq!(
            std::fs::read_to_string(&e.file).expect("read"),
            shared,
            "the shared file is never written"
        );
        let json = e.run("show tiny --driver local --json").expect("json");
        let v: Value = serde_json::from_str(&json).expect("json");
        assert_eq!(v[0]["tags_source"], "user");
        assert_eq!(v[0]["tier"], "fast");
        // Without a driver, every driver with an entry is shown.
        let any = e.run("show tiny").expect("any");
        assert_eq!(any.matches("tiny (").count(), 1, "{any}");
        // Unset: the shared file's entry applies again.
        let back = e.run("unset tiny --driver local").expect("unset");
        assert!(
            back.contains("[file]") && back.contains("tier: deep"),
            "{back}"
        );
        assert!(e
            .run("unset tiny --driver local")
            .unwrap_err()
            .contains("no user entry"));
    }

    #[test]
    fn a_profile_scopes_an_entry_and_implies_its_driver() {
        let e = env();
        // A name with a space is one argument.
        let named: Vec<String> = ["set", "m", "--profile", "SMALL card", "--tier", "fast"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let paths = Paths {
            file: &e.file,
            user: &e.user,
        };
        let by_name = run(&named, &paths, &e.profiles).expect("by name");
        assert!(by_name.contains("one server"), "{by_name}");
        assert!(e.run("unset m --profile p-small").is_ok());
        let small = e
            .run("set m --profile p-small --tier fast --tags fast")
            .expect("small");
        assert!(small.contains("m (local) [user, one server]"), "{small}");
        let big = e
            .run("set m --profile p-big --tier deep --tags code")
            .expect("big");
        assert!(big.contains("tier: deep"), "{big}");
        let by_name = e.run("show m --profile big").unwrap_err();
        assert!(
            by_name.contains("no profile"),
            "names must match whole: {by_name}"
        );
        assert!(e.run("show m --profile small").is_err());
        let json: Value =
            serde_json::from_str(&e.run("show m --profile p-small --json").expect("show"))
                .expect("json");
        assert_eq!(json[0]["tier"], "fast");
        assert_eq!(json[0]["profile_scoped"], true);
        // No profile given: the scoped entries never apply.
        assert!(e
            .run("show m --driver local")
            .unwrap_err()
            .contains("no tags"));
        let list = e.run("list").expect("list");
        assert!(
            list.contains("profile=p-small") && list.contains("profile=p-big"),
            "{list}"
        );
        assert!(e.run("unset m --profile p-small").is_ok());
        assert!(e.run("show m --profile p-small").is_err());
        assert!(e.run("show m --profile p-big").is_ok());
        // A profile whose agent differs from --driver is refused.
        assert!(e
            .run("set m --profile p-big --driver claude --tier fast")
            .unwrap_err()
            .contains("runs local"));
    }

    #[test]
    fn profile_names_resolve_whole_and_case_insensitively() {
        let e = env();
        let mut profiles = e.profiles.clone();
        assert_eq!(
            resolve_profile(&profiles, "p-big").map(|p| p.id.as_str()),
            Ok("p-big")
        );
        assert_eq!(
            resolve_profile(&profiles, "BIG CARD").map(|p| p.id.as_str()),
            Ok("p-big")
        );
        profiles.push(server("p-dup", "Big card"));
        assert!(resolve_profile(&profiles, "big card")
            .unwrap_err()
            .contains("more than one"));
        assert_eq!(
            resolve_profile(&profiles, "p-dup").map(|p| p.id.as_str()),
            Ok("p-dup")
        );
    }

    #[test]
    fn bad_input_is_refused_with_a_clear_message_and_writes_nothing() {
        let e = env();
        for (line, needle) in [
            ("set m --driver local --tags magic", "unknown tag \"magic\""),
            ("set m --driver local --tier huge", "unknown tier \"huge\""),
            (
                "set m --driver nowhere --tier fast",
                "unknown driver \"nowhere\"",
            ),
            ("set m --driver local", "needs --tier"),
            ("set --tier fast", "model id is required"),
            ("set a*b --driver local --tier fast", "no *"),
            ("set m --bogus 1", "unknown option --bogus"),
            ("set m --tier", "--tier needs a value"),
            ("set m extra --tier fast", "unexpected argument extra"),
            ("show", "model id is required"),
            ("show nothing-known --driver local", "no tags for"),
            ("unset m --driver local --tier fast", "takes no --tier"),
            ("list m", "takes no model"),
            ("frob", "unknown command"),
            ("", "a command is required"),
        ] {
            let err = e.run(line).expect_err(line);
            assert!(err.contains(needle), "{line}: {err}");
        }
        assert!(!e.user.exists(), "nothing was written");
        // A user file that cannot be read back is never replaced.
        std::fs::write(&e.user, "not json").expect("write");
        let err = e.run("set m --driver local --tier fast").unwrap_err();
        assert!(err.contains("not valid JSON"), "{err}");
        assert_eq!(std::fs::read_to_string(&e.user).expect("read"), "not json");
    }

    #[test]
    fn the_user_vocabulary_is_accepted_and_clearing_tags_works() {
        let e = env();
        std::fs::write(
            &e.user,
            r#"{"version": 1, "vocabulary": {"private": "mine"}, "models": []}"#,
        )
        .expect("write");
        let out = e
            .run("set m --driver local --tags private --tier fast")
            .expect("set");
        assert!(out.contains("tags: private"), "{out}");
        let cleared = e.run("set m --driver local --tags=").unwrap_err();
        assert!(cleared.contains("unknown option --tags="), "{cleared}");
        let mut args = words("set m --driver local --tags");
        args.push(String::new());
        let cleared = run(
            &args,
            &Paths {
                file: &e.file,
                user: &e.user,
            },
            &e.profiles,
        )
        .expect("clear");
        assert!(cleared.contains("tags: -"), "{cleared}");
    }

    #[test]
    fn list_shows_every_layer_with_its_source() {
        let e = env();
        e.run("set mine --driver local --tier fast --tags fast")
            .expect("set");
        let text = e.run("list").expect("list");
        assert!(
            text.lines().next().is_some_and(|l| l.starts_with("user")),
            "{text}"
        );
        assert!(text.contains("built_in"), "{text}");
        let json: Value = serde_json::from_str(&e.run("list --json").expect("json")).expect("json");
        assert_eq!(json[0]["tags_source"], "user");
        assert_eq!(json[0]["match"], "mine");
        // The built-in tags name no local model.
        assert!(json
            .as_array()
            .expect("array")
            .iter()
            .filter(|r| r["tags_source"] == "built_in")
            .all(|r| r["driver"] != "local"));
    }
}
