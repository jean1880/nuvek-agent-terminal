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
//! after the fork point.

use serde::{Deserialize, Serialize};

use crate::compare::Side;

/// Where a fork came from: its source thread and the source's last stored event at the fork.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkLink {
    pub source_thread: String,
    pub fork_seq: i64,
}

impl ForkLink {
    /// A link read back from storage: `None` for anything malformed (not JSON of this shape, no
    /// source, a negative sequence). Unknown fields are ignored, so a later build's link reads.
    pub fn parse(json: &str) -> Option<Self> {
        let link: Self = serde_json::from_str(json).ok()?;
        (!link.source_thread.trim().is_empty() && link.fork_seq >= 0).then_some(link)
    }

    /// The link as stored.
    pub fn to_json(&self) -> String {
        // Two plain fields: serialising cannot fail.
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
        ForkLink {
            source_thread: source.to_owned(),
            fork_seq: seq,
        }
    }

    #[test]
    fn a_link_round_trips_and_a_malformed_one_reads_as_none() {
        let l = link("src", 42);
        assert_eq!(ForkLink::parse(&l.to_json()), Some(l.clone()));
        assert_eq!(
            ForkLink::parse(r#"{"source_thread":"src","fork_seq":42,"base_tree":"abc"}"#),
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
