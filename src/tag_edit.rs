//! Editing the user's model tags, shared by Settings → Agents → Model Tags and the headless
//! `agent-terminal tags` command: what a model's effective tags are, and the two writes (set an
//! entry, reset it) that go to the user's file and nowhere else. Blocking file I/O: callers run it
//! off the main thread. Validation is [`agent_core::user_tags`]; nothing invalid is written.

use std::path::Path;

use agent_core::adapter::Driver;
use agent_core::model_tags::{ModelTags, TagSource};
use agent_core::user_tags::{self, Edit, Scope, UserEntry};

/// A model whose tags are read or written: its id, the agent that lists it, and the profile
/// whose server it runs on (`None`: every profile).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target<'a> {
    pub id: &'a str,
    pub driver: Option<Driver>,
    pub profile: Option<&'a str>,
}

impl<'a> Target<'a> {
    fn scope(self) -> Scope<'a> {
        Scope {
            driver: self.driver,
            profile: self.profile,
        }
    }
}

/// What a model's tags are, and which layer said so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effective {
    pub tier: Option<String>,
    pub tags: Vec<String>,
    pub best_for: Option<String>,
    pub source: TagSource,
    /// The entry is for one profile only.
    pub scoped: bool,
}

/// The effective tags of `target` under `tags` (all layers); `None` when nothing matches or the
/// target has no driver to look up.
pub fn effective(tags: &ModelTags, target: Target<'_>) -> Option<Effective> {
    let entry = tags.lookup_for(target.driver?, target.id, target.profile)?;
    Some(Effective {
        tier: entry.tier.clone(),
        tags: entry.tags.clone(),
        best_for: entry.best_for.clone(),
        source: entry.source,
        scoped: entry.profile.is_some(),
    })
}

/// Writes `edit` to the user's file `path` as the entry of `target`. A field the edit leaves
/// alone keeps the entry's value; a new entry starts from the model's effective tags (so setting
/// the tier alone does not blank the tags below it). Checked against the vocabulary of `tags`
/// before anything is written. Returns the entry as written.
pub fn set(
    path: &Path,
    tags: &ModelTags,
    target: Target<'_>,
    edit: &Edit,
) -> Result<UserEntry, String> {
    user_tags::check_id(target.id)?;
    let seed = effective(tags, target).map(|e| UserEntry {
        tier: e.tier,
        tags: e.tags,
        best_for: e.best_for,
    });
    agent_kit::user_tags_file::update(path, |doc| {
        let current = user_tags::find(doc, target.id, target.scope())
            .or(seed)
            .unwrap_or_default();
        let mut vocabulary = tags.vocabulary.clone();
        vocabulary.extend(user_tags::defined_vocabulary(doc));
        // Tags inherited from a lower layer are carried through as they are, even when the
        // vocabulary does not name them: an edit of the tier must not fail on a tag the user did
        // not touch. Tags the edit names are checked.
        if edit.tags.is_none() {
            vocabulary.extend(current.tags.iter().map(|t| (t.clone(), String::new())));
        }
        let entry = current.with(edit);
        user_tags::check_entry(&entry, &vocabulary)?;
        user_tags::upsert(doc, target.id, target.scope(), &entry)?;
        Ok(entry)
    })
}

/// Moves the user's entry of `from` to `to` (another profile scope of the same model) in one
/// write under the file's lock: nothing is changed unless both halves succeed. Whether there was
/// an entry to move.
pub fn rescope(
    path: &Path,
    tags: &ModelTags,
    from: Target<'_>,
    to: Target<'_>,
) -> Result<bool, String> {
    if from == to || !path.exists() {
        return Ok(false);
    }
    agent_kit::user_tags_file::update(path, |doc| {
        let Some(entry) = user_tags::find(doc, from.id, from.scope()) else {
            return Ok(false);
        };
        let mut vocabulary = tags.vocabulary.clone();
        vocabulary.extend(user_tags::defined_vocabulary(doc));
        vocabulary.extend(entry.tags.iter().map(|t| (t.clone(), String::new())));
        user_tags::check_entry(&entry, &vocabulary)?;
        user_tags::upsert(doc, to.id, to.scope(), &entry)?;
        user_tags::remove(doc, from.id, from.scope());
        Ok(true)
    })
}

/// Removes the user's entry of `target` (the model falls back to the layers below); whether there
/// was one. A file that is not there has none.
pub fn reset(path: &Path, target: Target<'_>) -> Result<bool, String> {
    if !path.exists() {
        return Ok(false);
    }
    agent_kit::user_tags_file::update(path, |doc| {
        Ok(user_tags::remove(doc, target.id, target.scope()))
    })
}

#[cfg(test)]
mod tests {
    use agent_core::model_tags::{built_in, parse};

    use super::*;

    fn tags() -> ModelTags {
        built_in()
    }

    fn local<'a>(id: &'a str, profile: Option<&'a str>) -> Target<'a> {
        Target {
            id,
            driver: Some(Driver::Local),
            profile,
        }
    }

    #[test]
    fn a_new_entry_starts_from_the_effective_tags_and_an_edit_keeps_the_rest() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("model-tags.user.json");
        let haiku = Target {
            id: "claude-haiku-9",
            driver: Some(Driver::Claude),
            profile: None,
        };
        let built = effective(&tags(), haiku).expect("built in");
        assert_eq!(built.source, TagSource::BuiltIn);
        let written = set(
            &path,
            &tags(),
            haiku,
            &Edit {
                tier: Some("standard".into()),
                ..Edit::default()
            },
        )
        .expect("set");
        assert_eq!(written.tier.as_deref(), Some("standard"));
        assert_eq!(written.tags, built.tags, "the tags below it are kept");
        // Read back through the layers: the user's entry is found first.
        let user = parse(&std::fs::read_to_string(&path).expect("read"))
            .expect("v1")
            .tagged(TagSource::User);
        let all = user.over(tags());
        let now = effective(&all, haiku).expect("now");
        assert_eq!(
            (now.source, now.tier.as_deref()),
            (TagSource::User, Some("standard"))
        );
        // Reset: the built-in tags apply again.
        assert_eq!(reset(&path, haiku), Ok(true));
        assert_eq!(reset(&path, haiku), Ok(false));
        let after = parse(&std::fs::read_to_string(&path).expect("read"))
            .expect("v1")
            .tagged(TagSource::User)
            .over(tags());
        assert_eq!(
            effective(&after, haiku).expect("back").source,
            TagSource::BuiltIn
        );
    }

    /// A tag inherited from a lower layer that the vocabulary does not name is carried through a
    /// tier edit; naming it in an edit is checked.
    #[test]
    fn a_tier_edit_never_fails_on_an_inherited_unknown_tag() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("t.json");
        let shared = parse(
            r#"{"version": 1, "models": [{"match": "m", "driver": "local", "tier": "deep",
                "tags": ["code", "from-elsewhere"]}]}"#,
        )
        .expect("v1")
        .tagged(TagSource::File)
        .over(tags());
        let tier = Edit {
            tier: Some("fast".into()),
            ..Edit::default()
        };
        let written = set(&path, &shared, local("m", None), &tier).expect("tier only");
        assert_eq!(written.tags, ["code", "from-elsewhere"]);
        let named = Edit {
            tags: Some(vec!["from-elsewhere".into()]),
            ..Edit::default()
        };
        assert!(set(&path, &shared, local("m", None), &named).is_err());
    }

    #[test]
    fn a_rescope_is_one_write_that_moves_the_entry_or_changes_nothing() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("t.json");
        assert_eq!(
            rescope(&path, &tags(), local("m", Some("p-a")), local("m", None)),
            Ok(false)
        );
        let edit = Edit {
            tier: Some("fast".into()),
            tags: Some(vec!["code".into()]),
            ..Edit::default()
        };
        set(&path, &tags(), local("m", Some("p-a")), &edit).expect("scoped");
        assert_eq!(
            rescope(&path, &tags(), local("m", Some("p-a")), local("m", None)),
            Ok(true)
        );
        let doc = agent_kit::user_tags_file::load(&path).expect("load");
        assert!(user_tags::find(&doc, "m", Some(Driver::Local)).is_some());
        assert!(user_tags::find(
            &doc,
            "m",
            Scope {
                driver: Some(Driver::Local),
                profile: Some("p-a")
            }
        )
        .is_none());
        // Moving to where it already is changes nothing (and never deletes the entry).
        assert_eq!(
            rescope(&path, &tags(), local("m", None), local("m", None)),
            Ok(false)
        );
        let doc = agent_kit::user_tags_file::load(&path).expect("load");
        assert!(user_tags::find(&doc, "m", Some(Driver::Local)).is_some());
        // A refused move (a bad profile id) leaves the entry where it was.
        let long = "p".repeat(65);
        assert!(rescope(&path, &tags(), local("m", None), local("m", Some(&long))).is_err());
        let doc = agent_kit::user_tags_file::load(&path).expect("load");
        assert!(user_tags::find(&doc, "m", Some(Driver::Local)).is_some());
    }

    #[test]
    fn nothing_invalid_is_written() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("t.json");
        let bad_tag = set(
            &path,
            &tags(),
            local("m", None),
            &Edit {
                tags: Some(vec!["telepathy".into()]),
                ..Edit::default()
            },
        );
        assert!(bad_tag
            .expect_err("tag")
            .contains("unknown tag \"telepathy\""));
        let bad_tier = set(
            &path,
            &tags(),
            local("m", None),
            &Edit {
                tier: Some("enormous".into()),
                ..Edit::default()
            },
        );
        assert!(bad_tier.expect_err("tier").contains("unknown tier"));
        assert!(set(&path, &tags(), local("a*", None), &Edit::default()).is_err());
        assert!(!path.exists(), "a refused edit leaves no file");
        assert_eq!(reset(&path, local("m", None)), Ok(false));
    }

    /// The user file's own vocabulary may extend the shared one.
    #[test]
    fn the_user_files_vocabulary_extends_what_a_tag_may_be() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("t.json");
        std::fs::write(
            &path,
            r#"{"version": 1, "vocabulary": {"private": "mine"}, "models": []}"#,
        )
        .expect("write");
        let edit = Edit {
            tags: Some(vec!["private".into(), "code".into()]),
            tier: Some("fast".into()),
            ..Edit::default()
        };
        let entry = set(&path, &tags(), local("m", None), &edit).expect("set");
        assert_eq!(entry.tags, ["private", "code"]);
        let doc = agent_kit::user_tags_file::load(&path).expect("load");
        assert_eq!(doc["vocabulary"]["private"], "mine", "kept on a write");
    }

    #[test]
    fn a_profile_scoped_write_leaves_the_unscoped_entry_alone() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("t.json");
        let all = Edit {
            tier: Some("deep".into()),
            tags: Some(vec!["code".into()]),
            ..Edit::default()
        };
        set(&path, &tags(), local("m", None), &all).expect("unscoped");
        let small = Edit {
            tier: Some("fast".into()),
            ..Edit::default()
        };
        let current = parse(&std::fs::read_to_string(&path).expect("read"))
            .expect("v1")
            .tagged(TagSource::User)
            .over(tags());
        let written = set(&path, &current, local("m", Some("p-small")), &small).expect("scoped");
        assert_eq!(written.tags, ["code"], "seeded from the entry that applied");
        let layered = parse(&std::fs::read_to_string(&path).expect("read"))
            .expect("v1")
            .tagged(TagSource::User)
            .over(tags());
        let tier = |p| effective(&layered, local("m", p)).and_then(|e| e.tier);
        assert_eq!(tier(Some("p-small")).as_deref(), Some("fast"));
        assert_eq!(tier(Some("p-big")).as_deref(), Some("deep"));
        assert_eq!(tier(None).as_deref(), Some("deep"));
        assert!(
            effective(&layered, local("m", Some("p-small")))
                .expect("e")
                .scoped
        );
        assert_eq!(reset(&path, local("m", Some("p-small"))), Ok(true));
        assert_eq!(
            effective(&layered, local("m", None))
                .and_then(|e| e.tier)
                .as_deref(),
            Some("deep")
        );
    }
}
