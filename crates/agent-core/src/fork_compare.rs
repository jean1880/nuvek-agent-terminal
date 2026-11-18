//! Which threads a forked thread can be compared with, and from where: no GTK and no I/O.
//!
//! A fork records its source and the source's last stored event when it was made
//! ([`ForkLink`]). That pairs it with its source, each fork of a thread with the thread, and
//! forks of one source with each other ([`pairings`]). A fork of a fork pairs with its own
//! source only, never with that source's source. Only ids and a sequence number are kept, so the
//! link holds no text to redact.
//!
//! What a pairing compares ([`Pairing::sides`]): a fork's whole transcript (the context it
//! inherited is not its own work, and is left out by the caller), and its source's transcript
//! after the fork point. Files are compared against the fork's baseline
//! ([`ForkLink::base_tree`]: a snapshot of the source's working tree taken when the fork was
//! made; [`Pairing::bases`]), so each side's changes since the fork are told from what was
//! there before it.
//!
//! Keep This One settles a pair: [`settle`] names the links that record which thread was kept
//! over which ([`Kept`]), and [`Pairing::kept`] reads it back, so a settled pair is shown as
//! such and not offered again.

use serde::{Deserialize, Deserializer, Serialize};

use crate::compare::Side;

/// Where a fork came from: its source thread and the source's last stored event at the fork,
/// and the snapshot of the source's working tree taken then, when there was one.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ForkLink {
    pub source_thread: String,
    pub fork_seq: i64,
    /// The git tree of the source's working tree at the fork (tracked changes and untracked
    /// files, less what checkpoints skip), kept alive by the fork's baseline ref. `None`: not a
    /// repository, git was busy, or a link made before baselines were taken.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_tree: Option<String>,
    /// The commit recording [`Self::base_tree`], which the baseline ref points at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
    /// Which thread was kept over which, when the pair was settled with Keep This One. Read
    /// leniently: anything but two distinct thread ids reads as none, never losing the link.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient_kept"
    )]
    pub kept: Option<Kept>,
    /// The paths a fork in its own worktree took from HEAD rather than from the source's folder
    /// (secret-looking names, files hidden from git): its baseline holds HEAD's content there,
    /// so neither side's changes are compared at them. Read leniently: anything but a list of
    /// plain relative paths reads as none of them.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "lenient_paths"
    )]
    pub not_compared: Vec<String>,
}

/// Most paths a [`ForkLink::not_compared`] keeps (a larger list reads as none).
const MAX_NOT_COMPARED: usize = 100_000;

/// A stored [`ForkLink::not_compared`]: the strings of a list that are git-valid relative
/// paths (no empty, `.`, `..` or `.git` component, and no NUL); none at all for anything that is
/// not a list, or too long a one.
fn lenient_paths<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;
    let Some(list) = value.as_array().filter(|l| l.len() <= MAX_NOT_COMPARED) else {
        return Ok(Vec::new());
    };
    Ok(list
        .iter()
        .filter_map(serde_json::Value::as_str)
        .filter(|p| {
            !p.is_empty()
                && !p.contains('\0')
                && !p
                    .split('/')
                    .any(|part| part.is_empty() || part == "." || part == ".." || part == ".git")
        })
        .map(str::to_owned)
        .collect())
}

/// A settled pair: `thread` was kept and `over` archived (Keep This One).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Kept {
    pub thread: String,
    pub over: String,
}

/// Most characters of a thread id a [`Kept`] reads (a thread id is a UUID).
const MAX_KEPT_ID: usize = 128;

/// A stored [`ForkLink::kept`]: `None` for anything that is not two distinct, non-empty ids.
fn lenient_kept<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Kept>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;
    let id = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty() && s.len() <= MAX_KEPT_ID)
            .map(str::to_owned)
    };
    Ok(match (id("thread"), id("over")) {
        (Some(thread), Some(over)) if thread != over => Some(Kept { thread, over }),
        _ => None,
    })
}

/// Whether `id` reads as a git object id: 40 (SHA-1) or 64 (SHA-256) lowercase hex digits.
pub fn is_object_id(id: &str) -> bool {
    matches!(id.len(), 40 | 64) && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

impl ForkLink {
    /// A link with no baseline.
    pub fn new(source_thread: &str, fork_seq: i64) -> Self {
        Self {
            source_thread: source_thread.to_owned(),
            fork_seq,
            ..Self::default()
        }
    }

    /// A link read back from storage: `None` for anything malformed (not JSON of this shape, no
    /// source, a negative sequence). Unknown fields are ignored, so a later build's link reads,
    /// and one from an earlier build has no baseline. A baseline that is not a pair of object
    /// ids is dropped and the link kept: the fork still compares by transcript.
    pub fn parse(json: &str) -> Option<Self> {
        let mut link: Self = serde_json::from_str(json).ok()?;
        if link.source_thread.trim().is_empty() || link.fork_seq < 0 {
            return None;
        }
        let valid = |id: &Option<String>| id.as_deref().is_some_and(is_object_id);
        if !(valid(&link.base_tree) && valid(&link.base_commit)) {
            link.base_tree = None;
            link.base_commit = None;
            // Paths not compared against a baseline that is gone mean nothing.
            link.not_compared.clear();
        }
        Some(link)
    }

    /// The link as stored.
    pub fn to_json(&self) -> String {
        // Plain strings and a number: serialising cannot fail.
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// What the other thread of a [`Pairing`] is to the thread it was found for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relation {
    /// The thread this one was forked from.
    Source,
    /// A thread forked from this one.
    Fork,
    /// Another fork of this thread's source.
    Sibling,
}

impl Relation {
    /// How a picker says what the other thread is, after its title.
    pub fn label(self) -> &'static str {
        match self {
            Self::Source => "the thread it was forked from",
            Self::Fork => "forked from it",
            Self::Sibling => "forked from the same thread",
        }
    }
}

/// A thread another can be compared with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pairing {
    /// The other thread.
    pub other: String,
    pub relation: Relation,
    /// Where the source's transcript starts to count (its fork point); 0 for two siblings.
    pub fork_seq: i64,
}

impl Pairing {
    /// The two columns, source first: for `this` and [`Pairing::other`], the source's events
    /// after the fork point and the fork's whole transcript; two siblings, each whole, `this`
    /// first.
    pub fn sides(&self, this: &str) -> [Side; 2] {
        let whole = |thread: &str| Side {
            thread: thread.to_owned(),
            after: 0,
        };
        let from_fork = |thread: &str| Side {
            thread: thread.to_owned(),
            after: self.fork_seq,
        };
        match self.relation {
            Relation::Source => [from_fork(&self.other), whole(this)],
            Relation::Fork => [from_fork(this), whole(&self.other)],
            Relation::Sibling => [whole(this), whole(&self.other)],
        }
    }

    /// The baseline tree each of [`Self::sides`] is compared against, in the same order, from
    /// the link of the thread the pairing was found for (`own`) and the other's (`other`). A
    /// fork and its source share the fork's baseline; two siblings each have their own (they
    /// may have been forked at different times).
    pub fn bases(&self, own: Option<&ForkLink>, other: Option<&ForkLink>) -> [Option<String>; 2] {
        let base = |link: Option<&ForkLink>| link.and_then(|l| l.base_tree.clone());
        match self.relation {
            // This thread is the fork.
            Relation::Source => [base(own), base(own)],
            // The other one is.
            Relation::Fork => [base(other), base(other)],
            Relation::Sibling => [base(own), base(other)],
        }
    }

    /// The paths each of [`Self::sides`] is not compared at ([`ForkLink::not_compared`]), in the
    /// same order and from the same links as [`Self::bases`].
    pub fn not_compared(
        &self,
        own: Option<&ForkLink>,
        other: Option<&ForkLink>,
    ) -> [Vec<String>; 2] {
        let paths =
            |link: Option<&ForkLink>| link.map(|l| l.not_compared.clone()).unwrap_or_default();
        match self.relation {
            Relation::Source => [paths(own), paths(own)],
            Relation::Fork => [paths(other), paths(other)],
            Relation::Sibling => [paths(own), paths(other)],
        }
    }

    /// Which of `this` and [`Self::other`] was kept over the other, when the pair was settled
    /// ([`settle`]): read from the link of the pair's fork (both, for two siblings), and only when
    /// it names exactly these two threads.
    pub fn kept(
        &self,
        this: &str,
        own: Option<&ForkLink>,
        other: Option<&ForkLink>,
    ) -> Option<String> {
        let links: &[Option<&ForkLink>] = match self.relation {
            Relation::Source => &[own],
            Relation::Fork => &[other],
            Relation::Sibling => &[own, other],
        };
        links.iter().flatten().find_map(|link| {
            let kept = link.kept.as_ref()?;
            let pair = (kept.thread.as_str(), kept.over.as_str());
            (pair == (this, self.other.as_str()) || pair == (self.other.as_str(), this))
                .then(|| kept.thread.clone())
        })
    }
}

/// Why [`settle`] wrote nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettleRefused {
    /// The two are not a fork and its source, nor two forks of one thread (or are one thread).
    NotAPair,
    /// The pair is settled already.
    Settled,
    /// A link the pair would be recorded on already records another pair: never overwritten.
    LinkHoldsAnotherPair,
}

impl SettleRefused {
    /// What the user is told.
    pub fn reason(self) -> &'static str {
        match self {
            Self::NotAPair => {
                "These two threads are not a fork and its source, nor two forks of one thread"
            }
            Self::Settled => "One of these two threads was kept over the other already",
            Self::LinkHoldsAnotherPair => {
                "One of these forks already records a choice made with another thread, and that \
                 record is not replaced"
            }
        }
    }
}

/// The links to write when `kept` is kept over `other` (Keep This One), each with its fork's id
/// and [`ForkLink::kept`] set: the fork's own link when one was forked from the other, both links
/// when they are two forks of one source. Refused when the two are no pair, are settled already,
/// or a link to write already records another pair (which is never overwritten).
pub fn settle(
    kept: &str,
    kept_link: Option<&ForkLink>,
    other: &str,
    other_link: Option<&ForkLink>,
) -> Result<Vec<(String, ForkLink)>, SettleRefused> {
    if kept == other {
        return Err(SettleRefused::NotAPair);
    }
    let record = Kept {
        thread: kept.to_owned(),
        over: other.to_owned(),
    };
    let reversed = Kept {
        thread: other.to_owned(),
        over: kept.to_owned(),
    };
    let links: Vec<(&str, &ForkLink)> = match (kept_link, other_link) {
        (_, Some(o)) if o.source_thread == kept => vec![(other, o)],
        (Some(k), _) if k.source_thread == other => vec![(kept, k)],
        (Some(k), Some(o)) if k.source_thread == o.source_thread => vec![(kept, k), (other, o)],
        _ => return Err(SettleRefused::NotAPair),
    };
    for (_, link) in &links {
        match &link.kept {
            None => {}
            Some(k) if *k == record || *k == reversed => return Err(SettleRefused::Settled),
            Some(_) => return Err(SettleRefused::LinkHoldsAnotherPair),
        }
    }
    Ok(links
        .into_iter()
        .map(|(fork, link)| {
            (
                fork.to_owned(),
                ForkLink {
                    kept: Some(record.clone()),
                    ..link.clone()
                },
            )
        })
        .collect())
}

/// The links to write when a [`settle`] of `kept` over `other` is undone: those of the two that
/// still record exactly that, with [`ForkLink::kept`] cleared.
pub fn unsettle(
    kept: &str,
    kept_link: Option<&ForkLink>,
    other: &str,
    other_link: Option<&ForkLink>,
) -> Vec<(String, ForkLink)> {
    let record = Kept {
        thread: kept.to_owned(),
        over: other.to_owned(),
    };
    [(kept, kept_link), (other, other_link)]
        .into_iter()
        .filter_map(|(fork, link)| {
            let link = link?;
            (link.kept.as_ref() == Some(&record)).then(|| {
                (
                    fork.to_owned(),
                    ForkLink {
                        kept: None,
                        ..link.clone()
                    },
                )
            })
        })
        .collect()
}

/// The words of a fork's branch name, from its source's title: lowercase ASCII letters and
/// digits joined by `-`, at most 32 characters, never starting or ending with `-`; `thread` when
/// nothing is left. A fork in its own worktree is on `fork/<slug>-<n>`.
pub fn branch_slug(title: &str) -> String {
    let mut slug = String::new();
    for word in title
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        let sep = usize::from(!slug.is_empty());
        let room = 32usize.saturating_sub(slug.len() + sep);
        if room == 0 {
            break;
        }
        if sep == 1 {
            slug.push('-');
        }
        let word = word.to_ascii_lowercase();
        slug.push_str(&word[..word.len().min(room)]);
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "thread".to_owned()
    } else {
        slug.to_owned()
    }
}

/// Every thread `thread` can be compared with: its source (while `exists` says it does), then
/// the threads forked from it, then the other forks of its source. `own` is its link, if it is a
/// fork; `links` every stored link, by fork. Only threads `exists` accepts are paired, each
/// once, never `thread` itself.
pub fn pairings(
    thread: &str,
    own: Option<&ForkLink>,
    links: &[(String, ForkLink)],
    exists: impl Fn(&str) -> bool,
) -> Vec<Pairing> {
    let mut out: Vec<Pairing> = Vec::new();
    let mut push = |other: &str, relation: Relation, fork_seq: i64| {
        if other != thread && exists(other) && !out.iter().any(|p| p.other == other) {
            out.push(Pairing {
                other: other.to_owned(),
                relation,
                fork_seq,
            });
        }
    };
    if let Some(own) = own {
        push(&own.source_thread, Relation::Source, own.fork_seq);
    }
    for (fork, link) in links {
        if link.source_thread == thread {
            push(fork, Relation::Fork, link.fork_seq);
        }
    }
    if let Some(own) = own {
        for (fork, link) in links {
            if link.source_thread == own.source_thread {
                push(fork, Relation::Sibling, 0);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(source: &str, seq: i64) -> ForkLink {
        ForkLink::new(source, seq)
    }

    const TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    fn based(source: &str, seq: i64, tree: &str) -> ForkLink {
        ForkLink {
            base_tree: Some(tree.to_owned()),
            base_commit: Some(COMMIT.to_owned()),
            ..link(source, seq)
        }
    }

    #[test]
    fn a_link_round_trips_and_a_malformed_one_reads_as_none() {
        let l = link("src", 42);
        assert_eq!(ForkLink::parse(&l.to_json()), Some(l.clone()));
        assert_eq!(
            l.to_json(),
            r#"{"source_thread":"src","fork_seq":42}"#,
            "a link with no baseline is written as earlier builds wrote one"
        );
        assert_eq!(
            ForkLink::parse(r#"{"source_thread":"src","fork_seq":42,"later":"abc"}"#),
            Some(l),
            "a later build's extra fields are ignored"
        );
        for bad in [
            "",
            "not json",
            "{}",
            r#"{"source_thread":"","fork_seq":1}"#,
            r#"{"source_thread":"  ","fork_seq":1}"#,
            r#"{"source_thread":"s","fork_seq":-1}"#,
            r#"{"source_thread":"s"}"#,
            r#"{"source_thread":1,"fork_seq":1}"#,
        ] {
            assert_eq!(ForkLink::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_baseline_round_trips_and_a_malformed_one_is_dropped_not_the_link() {
        let l = based("src", 3, TREE);
        assert_eq!(ForkLink::parse(&l.to_json()), Some(l.clone()));
        let sha256 = "a".repeat(64);
        assert_eq!(
            ForkLink::parse(&based("src", 3, &sha256).to_json())
                .and_then(|l| l.base_tree)
                .as_deref(),
            Some(sha256.as_str()),
            "a SHA-256 repository's ids read"
        );
        let upper = TREE.to_uppercase();
        for (tree, commit) in [
            (Some("abc"), Some(COMMIT)),
            (Some(TREE), None),
            (None, Some(COMMIT)),
            (
                Some("-delete-me-0123456789abcdef0123456789abc"),
                Some(COMMIT),
            ),
            (Some(upper.as_str()), Some(COMMIT)),
        ] {
            let json = serde_json::json!({
                "source_thread": "src",
                "fork_seq": 3,
                "base_tree": tree,
                "base_commit": commit,
            })
            .to_string();
            assert_eq!(ForkLink::parse(&json), Some(link("src", 3)), "{json}");
        }
        assert!(is_object_id(TREE));
        assert!(!is_object_id(""));
    }

    #[test]
    fn a_kept_record_round_trips_and_a_malformed_one_is_dropped_not_the_link() {
        let kept = ForkLink {
            kept: Some(Kept {
                thread: "src".into(),
                over: "fork".into(),
            }),
            ..link("src", 3)
        };
        assert_eq!(ForkLink::parse(&kept.to_json()), Some(kept.clone()));
        assert!(
            !link("src", 3).to_json().contains("kept"),
            "an unsettled link is written as before"
        );
        for bad in [
            serde_json::json!("src"),
            serde_json::json!(5),
            serde_json::json!(null),
            serde_json::json!({"thread": "a"}),
            serde_json::json!({"thread": "a", "over": "a"}),
            serde_json::json!({"thread": "", "over": "b"}),
            serde_json::json!({"thread": "a", "over": 1}),
            serde_json::json!({"thread": "a", "over": "x".repeat(200)}),
        ] {
            let json = serde_json::json!({
                "source_thread": "src",
                "fork_seq": 3,
                "kept": bad,
            })
            .to_string();
            assert_eq!(ForkLink::parse(&json), Some(link("src", 3)), "{json}");
        }
    }

    #[test]
    fn paths_not_compared_round_trip_read_leniently_and_go_to_both_sides_of_a_fork() {
        let l = ForkLink {
            not_compared: vec![".env".into(), "keys/a.pem".into()],
            ..based("src", 3, TREE)
        };
        assert_eq!(ForkLink::parse(&l.to_json()), Some(l.clone()));
        assert!(
            !based("src", 3, TREE).to_json().contains("not_compared"),
            "an empty list is not written"
        );
        let with = |value: serde_json::Value| {
            let json = serde_json::json!({
                "source_thread": "src",
                "fork_seq": 3,
                "base_tree": TREE,
                "base_commit": COMMIT,
                "not_compared": value,
            })
            .to_string();
            ForkLink::parse(&json)
                .expect("the link is kept")
                .not_compared
        };
        assert!(with(serde_json::json!("not a list")).is_empty());
        assert!(with(serde_json::json!(7)).is_empty());
        assert_eq!(
            with(serde_json::json!([
                ".env",
                3,
                "/abs",
                "../up",
                "a/../b",
                ".",
                "./.env",
                "a//b",
                "a/",
                ".git/HEAD",
                "a/.git/x",
                "",
                "ok/x"
            ])),
            [".env", "ok/x"]
        );
        // With no baseline they mean nothing, and are dropped with it.
        let json = serde_json::json!({
            "source_thread": "src",
            "fork_seq": 3,
            "not_compared": [".env"],
        })
        .to_string();
        assert!(ForkLink::parse(&json).unwrap().not_compared.is_empty());
        // A fork and its source both leave out the fork's paths; siblings each their own.
        let p = |relation, other: &str| Pairing {
            other: other.to_owned(),
            relation,
            fork_seq: 3,
        };
        let both = [l.not_compared.clone(), l.not_compared.clone()];
        assert_eq!(
            p(Relation::Source, "src").not_compared(Some(&l), None),
            both
        );
        assert_eq!(p(Relation::Fork, "f").not_compared(None, Some(&l)), both);
        assert_eq!(
            p(Relation::Sibling, "s").not_compared(Some(&l), None),
            [l.not_compared.clone(), Vec::new()]
        );
    }

    #[test]
    fn settling_records_the_pair_on_the_forks_link_and_reads_back_only_for_that_pair() {
        let fork = link("a", 10);
        let sibling = link("a", 20);
        // A source kept over its fork, and a fork kept over its source: the fork's link.
        let s = settle("a", None, "b", Some(&fork)).expect("a pair");
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].0, "b");
        assert_eq!(
            s[0].1.kept,
            Some(Kept {
                thread: "a".into(),
                over: "b".into()
            })
        );
        assert_eq!(s[0].1.fork_seq, 10, "the rest of the link is kept");
        let s = settle("b", Some(&fork), "a", None).expect("a pair");
        assert_eq!(s.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(), ["b"]);
        // Two siblings: both links.
        let s = settle("b", Some(&fork), "c", Some(&sibling)).expect("siblings");
        assert_eq!(
            s.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(),
            ["b", "c"]
        );
        // No pair, or one thread: nothing.
        let refused = |r| Err::<Vec<(String, ForkLink)>, _>(r);
        assert_eq!(
            settle("a", None, "x", None),
            refused(SettleRefused::NotAPair)
        );
        assert_eq!(
            settle("b", Some(&fork), "y", Some(&link("x", 1))),
            refused(SettleRefused::NotAPair)
        );
        assert_eq!(
            settle("b", Some(&fork), "b", Some(&fork)),
            refused(SettleRefused::NotAPair)
        );

        // Read back from either side, for this pair only.
        let settled = settle("b", Some(&fork), "a", None)
            .expect("a pair")
            .remove(0)
            .1;
        // Settled already, either way round: refused, the record left as it is.
        assert_eq!(
            settle("b", Some(&settled), "a", None),
            refused(SettleRefused::Settled)
        );
        assert_eq!(
            settle("a", None, "b", Some(&settled)),
            refused(SettleRefused::Settled)
        );
        // A link that records another pair is never overwritten: the fork kept over its source
        // cannot then be kept over a sibling, nor the sibling over it.
        assert_eq!(
            settle("b", Some(&settled), "c", Some(&sibling)),
            refused(SettleRefused::LinkHoldsAnotherPair)
        );
        assert_eq!(
            settle("c", Some(&sibling), "b", Some(&settled)),
            refused(SettleRefused::LinkHoldsAnotherPair)
        );
        assert!(!SettleRefused::LinkHoldsAnotherPair.reason().is_empty());
        let from_fork = Pairing {
            other: "a".into(),
            relation: Relation::Source,
            fork_seq: 10,
        };
        assert_eq!(
            from_fork.kept("b", Some(&settled), None).as_deref(),
            Some("b")
        );
        let from_source = Pairing {
            other: "b".into(),
            relation: Relation::Fork,
            fork_seq: 10,
        };
        assert_eq!(
            from_source.kept("a", None, Some(&settled)).as_deref(),
            Some("b")
        );
        // The same link settled against a sibling says nothing of the source.
        let over_sibling = settle("b", Some(&fork), "c", Some(&sibling))
            .expect("siblings")
            .remove(0)
            .1;
        assert_eq!(from_fork.kept("b", Some(&over_sibling), None), None);
        let siblings = Pairing {
            other: "c".into(),
            relation: Relation::Sibling,
            fork_seq: 0,
        };
        assert_eq!(
            siblings
                .kept("b", Some(&over_sibling), Some(&sibling))
                .as_deref(),
            Some("b")
        );

        // Undone: only links that still record exactly that are cleared.
        let undone = unsettle("b", Some(&over_sibling), "c", Some(&sibling));
        assert_eq!(undone.len(), 1);
        assert_eq!(undone[0], ("b".to_owned(), fork.clone()));
        assert!(unsettle("c", Some(&sibling), "b", Some(&over_sibling)).is_empty());
    }

    #[test]
    fn a_branch_slug_is_safe_for_git_and_never_empty() {
        assert_eq!(branch_slug("Fix the login bug"), "fix-the-login-bug");
        assert_eq!(branch_slug("  --Déjà vu: v2!  "), "d-j-vu-v2");
        assert_eq!(branch_slug(""), "thread");
        assert_eq!(branch_slug("✨"), "thread");
        let long = branch_slug(&"word ".repeat(40));
        assert!(long.len() <= 32, "{long}");
        assert!(!long.starts_with('-') && !long.ends_with('-'), "{long}");
        assert!(branch_slug("-rf /").starts_with("rf"));
    }

    #[test]
    fn a_fork_and_its_source_share_the_forks_baseline_and_siblings_keep_their_own() {
        let fork = based("a", 10, TREE);
        let other_tree = "b".repeat(40);
        let sibling = based("a", 20, &other_tree);
        let p = |relation, other: &str| Pairing {
            other: other.to_owned(),
            relation,
            fork_seq: 10,
        };
        let shared = [Some(TREE.to_owned()), Some(TREE.to_owned())];
        // Seen from the fork, then from its source.
        assert_eq!(p(Relation::Source, "a").bases(Some(&fork), None), shared);
        assert_eq!(p(Relation::Fork, "b").bases(None, Some(&fork)), shared);
        assert_eq!(
            p(Relation::Sibling, "c").bases(Some(&fork), Some(&sibling)),
            [Some(TREE.to_owned()), Some(other_tree)]
        );
        // A link with no baseline (an earlier build's, or no repository) gives none.
        assert_eq!(
            p(Relation::Source, "a").bases(Some(&link("a", 10)), None),
            [None, None]
        );
    }

    #[test]
    fn pairs_siblings_and_parent_child_and_never_unrelated() {
        // a ← b, a ← c, b ← d (a fork of a fork); x ← y is unrelated.
        let links = vec![
            ("b".to_owned(), link("a", 10)),
            ("c".to_owned(), link("a", 20)),
            ("d".to_owned(), link("b", 5)),
            ("y".to_owned(), link("x", 1)),
        ];
        let all = |_: &str| true;
        let own = |t: &str| links.iter().find(|(f, _)| f == t).map(|(_, l)| l.clone());
        let found = |t: &str| -> Vec<(String, Relation, i64)> {
            pairings(t, own(t).as_ref(), &links, all)
                .into_iter()
                .map(|p| (p.other, p.relation, p.fork_seq))
                .collect()
        };
        assert_eq!(
            found("a"),
            [
                ("b".into(), Relation::Fork, 10),
                ("c".into(), Relation::Fork, 20)
            ]
        );
        assert_eq!(
            found("b"),
            [
                ("a".into(), Relation::Source, 10),
                ("d".into(), Relation::Fork, 5),
                ("c".into(), Relation::Sibling, 0)
            ]
        );
        assert_eq!(
            found("d"),
            [("b".into(), Relation::Source, 5)],
            "a fork of a fork pairs with its own source only"
        );
        assert!(found("x").iter().all(|(t, _, _)| t == "y"));
        assert!(found("lonely").is_empty());
    }

    #[test]
    fn a_thread_that_is_gone_is_never_paired() {
        let links = vec![
            ("b".to_owned(), link("a", 10)),
            ("c".to_owned(), link("a", 20)),
        ];
        // The source deleted: the forks still pair with each other.
        let p = pairings("b", Some(&links[0].1), &links, |t| t != "a");
        assert_eq!(p.len(), 1);
        assert_eq!(
            (p[0].other.as_str(), p[0].relation),
            ("c", Relation::Sibling)
        );
        // A fork deleted: its source no longer offers it.
        let p = pairings("a", None, &links, |t| t != "c");
        assert_eq!(
            p.iter().map(|p| p.other.as_str()).collect::<Vec<_>>(),
            ["b"]
        );
        // A link naming the thread itself pairs nothing.
        let own = link("b", 1);
        assert!(pairings("b", Some(&own), &[("b".into(), own.clone())], |_| true).is_empty());
    }

    #[test]
    fn sides_slice_the_source_after_the_fork_point_and_put_it_first() {
        let source = Pairing {
            other: "a".into(),
            relation: Relation::Source,
            fork_seq: 10,
        };
        assert_eq!(
            source.sides("b"),
            [
                Side {
                    thread: "a".into(),
                    after: 10
                },
                Side {
                    thread: "b".into(),
                    after: 0
                }
            ]
        );
        let fork = Pairing {
            relation: Relation::Fork,
            other: "b".into(),
            fork_seq: 10,
        };
        assert_eq!(
            fork.sides("a"),
            source.sides("b"),
            "the same pair either way"
        );
        let sibling = Pairing {
            other: "c".into(),
            relation: Relation::Sibling,
            fork_seq: 0,
        };
        let [left, right] = sibling.sides("b");
        assert_eq!((left.thread.as_str(), left.after), ("b", 0));
        assert_eq!((right.thread.as_str(), right.after), ("c", 0));
    }
}
