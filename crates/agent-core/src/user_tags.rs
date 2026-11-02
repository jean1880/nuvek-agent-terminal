//! The user's own model tags: the pure half of the file the app alone writes (the top layer over
//! the shared tag file and the built-in tags, see [`crate::model_tags`]).
//!
//! The file is the same v1 contract as the shared tag file. The app edits it as JSON, entry by
//! entry, so what it does not understand (a hand-added vocabulary, another field) survives a
//! write. An entry the app writes names one model exactly (`match` is the id, no `*`), with the
//! driver when the model belongs to one agent, so a Local model never catches a vendor model of the
//! same name. Every value is checked here, before anything is written: a tier must be one of
//! [`TIERS`], a tag must be in the vocabulary, and nothing may exceed the bounds the reader keeps.
//!
//! No I/O: the caller reads and writes the file.

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};

use crate::adapter::Driver;
use crate::model_tags::{
    MAX_BEST_FOR_BYTES, MAX_ENTRIES, MAX_MATCH_BYTES, MAX_PROFILE_BYTES, MAX_TAGS, MAX_TAG_BYTES,
    VERSION,
};

/// The tiers an entry may name, least to most capable.
pub const TIERS: [&str; 3] = ["fast", "standard", "deep"];

/// What a user entry says about one model.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserEntry {
    pub tier: Option<String>,
    pub tags: Vec<String>,
    pub best_for: Option<String>,
}

/// A change to an entry: `None` leaves a field as it is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Edit {
    pub tier: Option<String>,
    pub tags: Option<Vec<String>>,
    /// An empty text clears the note.
    pub best_for: Option<String>,
}

impl UserEntry {
    /// This entry with `edit` applied.
    pub fn with(mut self, edit: &Edit) -> UserEntry {
        if let Some(tier) = &edit.tier {
            self.tier = Some(tier.trim().to_owned());
        }
        if let Some(tags) = &edit.tags {
            self.tags = tags.clone();
        }
        if let Some(note) = &edit.best_for {
            let note = note.trim();
            self.best_for = (!note.is_empty()).then(|| note.to_owned());
        }
        self
    }
}

/// An empty user file.
pub fn new_document() -> Value {
    json!({"version": VERSION, "models": []})
}

/// `doc` is a v1 object with a `models` array, else why not: the app never rewrites a file it
/// cannot read back.
pub fn check_document(doc: &Value) -> Result<(), String> {
    if !doc.is_object() {
        return Err("not a JSON object".into());
    }
    match doc.get("version").and_then(Value::as_u64) {
        Some(VERSION) => {}
        Some(other) => return Err(format!("unsupported version {other} (expected {VERSION})")),
        None => return Err("missing version".into()),
    }
    if !doc.get("models").is_some_and(Value::is_array) {
        return Err("missing models array".into());
    }
    Ok(())
}

/// Why `tier` is not a tier.
pub fn check_tier(tier: &str) -> Result<(), String> {
    if TIERS.contains(&tier) {
        Ok(())
    } else {
        Err(format!(
            "unknown tier \"{tier}\" (use {})",
            TIERS.join(", ")
        ))
    }
}

/// Why `entry` cannot be written: a tier that is not one of [`TIERS`], a tag outside `vocabulary`,
/// a duplicate, or a value over the bounds the reader keeps.
pub fn check_entry(entry: &UserEntry, vocabulary: &BTreeMap<String, String>) -> Result<(), String> {
    if let Some(tier) = &entry.tier {
        check_tier(tier)?;
    }
    if entry.tags.len() > MAX_TAGS {
        return Err(format!("at most {MAX_TAGS} tags"));
    }
    for (i, tag) in entry.tags.iter().enumerate() {
        if tag.is_empty() || tag.len() > MAX_TAG_BYTES {
            return Err(format!("a tag is 1 to {MAX_TAG_BYTES} bytes"));
        }
        if !vocabulary.contains_key(tag) {
            let known: Vec<&str> = vocabulary.keys().map(String::as_str).collect();
            return Err(format!(
                "unknown tag \"{tag}\" (known: {})",
                known.join(", ")
            ));
        }
        if entry.tags[..i].contains(tag) {
            return Err(format!("tag \"{tag}\" is repeated"));
        }
    }
    if entry
        .best_for
        .as_ref()
        .is_some_and(|b| b.len() > MAX_BEST_FOR_BYTES)
    {
        return Err(format!("best-for is at most {MAX_BEST_FOR_BYTES} bytes"));
    }
    Ok(())
}

/// Why `id` cannot be an exact model id.
pub fn check_id(id: &str) -> Result<(), String> {
    if id.trim().is_empty() {
        return Err("a model id is required".into());
    }
    if id.contains('*') {
        return Err("a model id has no *: an entry names one model exactly".into());
    }
    if id.len() > MAX_MATCH_BYTES {
        return Err(format!("a model id is at most {MAX_MATCH_BYTES} bytes"));
    }
    Ok(())
}

/// What an entry is for besides the model id: one driver (or any) and one profile (or every
/// one). Together with the id it is the entry's identity. A bare `Option<Driver>` converts to a
/// scope for every profile.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Scope<'a> {
    pub driver: Option<Driver>,
    pub profile: Option<&'a str>,
}

impl From<Option<Driver>> for Scope<'_> {
    fn from(driver: Option<Driver>) -> Self {
        Scope {
            driver,
            profile: None,
        }
    }
}

/// A profile id as an entry carries it: 1 to [`MAX_PROFILE_BYTES`] bytes.
pub fn check_profile(profile: &str) -> Result<(), String> {
    if profile.trim().is_empty() || profile.len() > MAX_PROFILE_BYTES {
        return Err(format!("a profile id is 1 to {MAX_PROFILE_BYTES} bytes"));
    }
    Ok(())
}

fn same(entry: &Value, id: &str, scope: Scope<'_>) -> bool {
    let id_matches = entry
        .get("match")
        .and_then(Value::as_str)
        .is_some_and(|m| m.trim().eq_ignore_ascii_case(id.trim()));
    let driver_matches = match entry.get("driver") {
        None | Some(Value::Null) => scope.driver.is_none(),
        Some(Value::String(d)) => scope.driver.is_some_and(|dr| dr.info().key == d.as_str()),
        Some(_) => false,
    };
    let profile_matches = match entry.get("profile") {
        None | Some(Value::Null) => scope.profile.is_none(),
        Some(Value::String(p)) => scope.profile.is_some_and(|sp| sp == p.trim()),
        Some(_) => false,
    };
    id_matches && driver_matches && profile_matches
}

fn models(doc: &Value) -> &[Value] {
    doc.get("models")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// The entry of `doc` that names `id` exactly (for `driver`), as the app wrote or would read it.
pub fn find<'a>(doc: &Value, id: &str, scope: impl Into<Scope<'a>>) -> Option<UserEntry> {
    let scope = scope.into();
    let v = models(doc).iter().find(|e| same(e, id, scope))?;
    let text = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    Some(UserEntry {
        tier: text("tier"),
        tags: v
            .get("tags")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        best_for: text("best_for"),
    })
}

/// Writes `entry` for `id` into `doc`, replacing the entry that names it, else adding one first
/// (so it is found before any pattern). Fields of a replaced entry the app does not know stay.
/// `Err` leaves `doc` as it was.
pub fn upsert<'a>(
    doc: &mut Value,
    id: &str,
    scope: impl Into<Scope<'a>>,
    entry: &UserEntry,
) -> Result<(), String> {
    let scope = scope.into();
    check_id(id)?;
    if let Some(p) = scope.profile {
        check_profile(p)?;
    }
    check_document(doc)?;
    let id = id.trim();
    let mut v = match models(doc).iter().find(|e| same(e, id, scope)) {
        Some(Value::Object(o)) => o.clone(),
        _ => Map::new(),
    };
    v.insert("match".into(), json!(id));
    v.insert(
        "driver".into(),
        scope.driver.map_or(Value::Null, |d| json!(d.info().key)),
    );
    match scope.profile {
        Some(p) => v.insert("profile".into(), json!(p.trim())),
        None => v.remove("profile"),
    };
    for (key, value) in [
        ("tier", entry.tier.as_ref()),
        ("best_for", entry.best_for.as_ref()),
    ] {
        match value {
            Some(text) => v.insert(key.into(), json!(text)),
            None => v.remove(key),
        };
    }
    v.insert("tags".into(), json!(entry.tags));
    let list = doc
        .get_mut("models")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| String::from("missing models array"))?;
    match list.iter().position(|e| same(e, id, scope)) {
        Some(at) => list[at] = Value::Object(v),
        None => {
            if list.len() >= MAX_ENTRIES {
                return Err(format!("at most {MAX_ENTRIES} entries"));
            }
            list.insert(0, Value::Object(v));
        }
    }
    Ok(())
}

/// Removes the entry that names `id` (for `driver`); whether there was one.
pub fn remove<'a>(doc: &mut Value, id: &str, scope: impl Into<Scope<'a>>) -> bool {
    let scope = scope.into();
    let Some(list) = doc.get_mut("models").and_then(Value::as_array_mut) else {
        return false;
    };
    let before = list.len();
    list.retain(|e| !same(e, id, scope));
    list.len() != before
}

/// Every vocabulary tag the user file defines (the reader extends the shared vocabulary with it).
pub fn defined_vocabulary(doc: &Value) -> BTreeMap<String, String> {
    doc.get("vocabulary")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter(|(k, _)| !k.trim().is_empty())
        .map(|(k, v)| {
            (
                k.trim().to_owned(),
                v.as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

/// Splits a comma-separated tag list: trimmed, empty pieces dropped. `""` is no tags.
pub fn split_tags(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_tags::{parse, TagSource};

    fn vocab() -> BTreeMap<String, String> {
        ["code", "fast", "review"]
            .iter()
            .map(|t| ((*t).to_owned(), String::new()))
            .collect()
    }

    fn entry(tier: &str, tags: &[&str]) -> UserEntry {
        UserEntry {
            tier: Some(tier.into()),
            tags: tags.iter().map(|t| (*t).to_owned()).collect(),
            best_for: Some("quick checks".into()),
        }
    }

    #[test]
    fn an_entry_is_checked_before_it_is_written() {
        let ok = entry("fast", &["code", "fast"]);
        assert_eq!(check_entry(&ok, &vocab()), Ok(()));
        let bad_tier = check_entry(&entry("huge", &[]), &vocab()).unwrap_err();
        assert!(bad_tier.contains("unknown tier \"huge\"") && bad_tier.contains("fast, standard"));
        let bad_tag = check_entry(&entry("fast", &["magic"]), &vocab()).unwrap_err();
        assert!(bad_tag.contains("unknown tag \"magic\"") && bad_tag.contains("code, fast"));
        assert!(check_entry(&entry("fast", &["code", "code"]), &vocab())
            .unwrap_err()
            .contains("repeated"));
        let many: Vec<String> = (0..=MAX_TAGS).map(|i| format!("t{i}")).collect();
        let many = UserEntry {
            tags: many,
            ..UserEntry::default()
        };
        assert!(check_entry(&many, &vocab()).is_err());
        let long = UserEntry {
            best_for: Some("x".repeat(MAX_BEST_FOR_BYTES + 1)),
            ..UserEntry::default()
        };
        assert!(check_entry(&long, &vocab()).is_err());
        assert_eq!(check_entry(&UserEntry::default(), &vocab()), Ok(()));
    }

    #[test]
    fn ids_are_exact_and_bounded() {
        assert_eq!(check_id("qwen-x:9b"), Ok(()));
        assert!(check_id("  ").is_err());
        assert!(check_id("a*").is_err());
        assert!(check_id(&"m".repeat(MAX_MATCH_BYTES + 1)).is_err());
    }

    #[test]
    fn an_edit_changes_only_the_fields_it_names() {
        let base = entry("deep", &["code"]);
        let tier = base.clone().with(&Edit {
            tier: Some(" fast ".into()),
            ..Edit::default()
        });
        assert_eq!(tier.tier.as_deref(), Some("fast"));
        assert_eq!(tier.tags, ["code"]);
        assert_eq!(tier.best_for.as_deref(), Some("quick checks"));
        let cleared = base.with(&Edit {
            tags: Some(vec![]),
            best_for: Some("  ".into()),
            ..Edit::default()
        });
        assert!(cleared.tags.is_empty() && cleared.best_for.is_none());
        assert_eq!(cleared.tier.as_deref(), Some("deep"));
    }

    #[test]
    fn upsert_adds_first_replaces_in_place_and_keeps_unknown_fields() {
        let mut doc = new_document();
        assert_eq!(check_document(&doc), Ok(()));
        upsert(
            &mut doc,
            "m1",
            Some(Driver::Local),
            &entry("fast", &["fast"]),
        )
        .expect("add");
        upsert(&mut doc, "m1", None, &entry("deep", &["review"])).expect("add any");
        upsert(&mut doc, "m2", Some(Driver::Local), &entry("fast", &[])).expect("add 2");
        assert_eq!(models(&doc).len(), 3);
        assert_eq!(models(&doc)[0]["match"], "m2", "a new entry goes first");
        // The driver is part of the identity.
        assert_eq!(
            find(&doc, "M1", Some(Driver::Local)).and_then(|e| e.tier),
            Some("fast".into())
        );
        assert_eq!(
            find(&doc, "m1", None).and_then(|e| e.tier),
            Some("deep".into())
        );
        assert_eq!(find(&doc, "m1", Some(Driver::Claude)), None);
        // A replaced entry keeps a field the app does not know, and drops a cleared one.
        doc["models"][2]["future"] = json!(7);
        upsert(
            &mut doc,
            "m1",
            Some(Driver::Local),
            &UserEntry {
                tier: None,
                tags: vec![],
                best_for: None,
            },
        )
        .expect("replace");
        assert_eq!(models(&doc).len(), 3, "replaced, not added");
        let e = models(&doc)
            .iter()
            .find(|e| same(e, "m1", Some(Driver::Local).into()))
            .expect("entry");
        assert!(e.get("tier").is_none() && e.get("best_for").is_none());
        assert_eq!(e["driver"], "local");
        assert!(remove(&mut doc, "m1", Some(Driver::Local)));
        assert!(!remove(&mut doc, "m1", Some(Driver::Local)));
        assert_eq!(models(&doc).len(), 2);
    }

    /// A profile is part of an entry's identity: one model, two servers, two entries.
    #[test]
    fn a_profile_scoped_entry_is_its_own_entry_and_reads_back_for_that_profile_only() {
        let mut doc = new_document();
        let scoped = |p| Scope {
            driver: Some(Driver::Local),
            profile: Some(p),
        };
        upsert(&mut doc, "m", Some(Driver::Local), &entry("deep", &[])).expect("any server");
        upsert(&mut doc, "m", scoped("p-small"), &entry("fast", &["code"])).expect("small");
        upsert(&mut doc, "m", scoped("p-big"), &entry("standard", &[])).expect("big");
        assert_eq!(models(&doc).len(), 3);
        assert_eq!(
            find(&doc, "m", scoped("p-small")).and_then(|e| e.tier),
            Some("fast".into())
        );
        assert_eq!(
            find(&doc, "m", Some(Driver::Local)).and_then(|e| e.tier),
            Some("deep".into())
        );
        assert_eq!(find(&doc, "m", scoped("p-none")), None);
        upsert(&mut doc, "m", scoped("p-small"), &entry("deep", &[])).expect("replace");
        assert_eq!(models(&doc).len(), 3, "replaced in place");
        assert!(upsert(&mut doc, "m", scoped("  "), &entry("deep", &[])).is_err());
        let long = "p".repeat(65);
        assert!(upsert(&mut doc, "m", scoped(&long), &entry("deep", &[])).is_err());
        assert!(remove(&mut doc, "m", scoped("p-small")));
        assert!(
            find(&doc, "m", Some(Driver::Local)).is_some(),
            "the unscoped one stays"
        );
        let tags = parse(&doc.to_string()).expect("v1").tagged(TagSource::User);
        assert_eq!(
            tags.lookup_for(Driver::Local, "m", Some("p-big"))
                .and_then(|e| e.tier.as_deref()),
            Some("standard")
        );
    }

    #[test]
    fn what_is_written_reads_back_with_its_driver_and_tags() {
        let mut doc = new_document();
        upsert(
            &mut doc,
            "mymodel",
            Some(Driver::Local),
            &entry("fast", &["code"]),
        )
        .expect("add");
        let tags = parse(&doc.to_string()).expect("v1").tagged(TagSource::User);
        let found = tags.lookup(Driver::Local, "MyModel").expect("local");
        assert_eq!(found.tier.as_deref(), Some("fast"));
        assert_eq!(found.tags, ["code"]);
        assert_eq!(found.source, TagSource::User);
        assert!(
            tags.lookup(Driver::Claude, "mymodel").is_none(),
            "a Local entry never catches a vendor model of the same name"
        );
    }

    #[test]
    fn a_file_that_is_not_v1_is_never_edited() {
        let mut other = json!({"version": 2, "models": []});
        assert!(upsert(&mut other, "m", None, &UserEntry::default()).is_err());
        assert_eq!(other, json!({"version": 2, "models": []}));
        assert!(check_document(&json!([])).is_err());
        assert!(check_document(&json!({"models": []})).is_err());
        assert!(check_document(&json!({"version": 1})).is_err());
        assert!(!remove(&mut other, "m", None));
    }

    #[test]
    fn the_entry_count_is_bounded() {
        let mut doc = new_document();
        for i in 0..MAX_ENTRIES {
            upsert(&mut doc, &format!("m{i}"), None, &UserEntry::default()).expect("add");
        }
        assert!(upsert(&mut doc, "one-more", None, &UserEntry::default()).is_err());
        assert!(upsert(&mut doc, "m1", None, &entry("fast", &[])).is_ok());
    }

    #[test]
    fn the_user_vocabulary_and_tag_lists_are_read() {
        let doc = json!({"version": 1, "models": [], "vocabulary": {" mine ": "x", "": "y"}});
        assert_eq!(defined_vocabulary(&doc)["mine"], "x");
        assert_eq!(defined_vocabulary(&doc).len(), 1);
        assert_eq!(split_tags(" a, b ,,c"), ["a", "b", "c"]);
        assert!(split_tags("").is_empty());
    }
}
