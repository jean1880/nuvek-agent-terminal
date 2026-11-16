//! Indexes, once at start-up, every thread whose events the search index has not taken yet.
//!
//! It runs on a thread of its own and writes only through the store writer, one job at a time
//! ([`Store::index_thread`] with [`INDEX_BUDGET`]): it waits for each job's answer before it
//! queues the next, and while the writer has more than [`YIELD_AT`] writes waiting it waits
//! first, so the app's own writes never queue behind it. The most recently updated threads go
//! first. Nothing records that it ran: the watermarks are its progress, so a run cut short (the
//! app quit) is taken up by the next from where it stopped.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::store::{Store, INDEX_BUDGET};
use crate::store_writer::StoreWriter;

/// More writes than this waiting, and the backfill waits before it queues its next job.
pub const YIELD_AT: usize = 64;
/// How long it waits each time.
const YIELD_FOR: Duration = Duration::from_millis(50);

/// A running (or finished) backfill.
#[derive(Clone)]
pub struct Backfill {
    remaining: Arc<AtomicUsize>,
    finished: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl Backfill {
    /// Threads still to index.
    pub fn remaining(&self) -> usize {
        self.remaining.load(Ordering::SeqCst)
    }

    /// Whether it has ended: every thread indexed, stopped, or the writer gone.
    pub fn finished(&self) -> bool {
        self.finished.load(Ordering::SeqCst)
    }

    /// Asks it to stop before its next job.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Starts the backfill on a new thread.
pub fn spawn(writer: StoreWriter) -> std::io::Result<Backfill> {
    let backfill = Backfill {
        remaining: Arc::new(AtomicUsize::new(0)),
        finished: Arc::new(AtomicBool::new(false)),
        stop: Arc::new(AtomicBool::new(false)),
    };
    let running = backfill.clone();
    std::thread::Builder::new()
        .name("search-backfill".into())
        .spawn(move || {
            run(&writer, &running);
            running.finished.store(true, Ordering::SeqCst);
        })?;
    Ok(backfill)
}

/// Whether the backfill should end now.
fn ending(writer: &StoreWriter, backfill: &Backfill) -> bool {
    backfill.stop.load(Ordering::SeqCst) || writer.is_closed()
}

fn run(writer: &StoreWriter, backfill: &Backfill) {
    if ending(writer, backfill) {
        return;
    }
    let pending = match writer
        .call_result(
            "list the threads to index for search",
            Store::search_pending,
        )
        .recv()
    {
        Ok(Ok(pending)) => pending,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "could not list the threads to index for search");
            return;
        }
        Err(_) => return,
    };
    if pending.is_empty() {
        return;
    }
    tracing::info!(threads = pending.len(), "indexing threads for search");
    backfill.remaining.store(pending.len(), Ordering::SeqCst);
    for thread in pending {
        loop {
            while writer.pending() > YIELD_AT {
                if ending(writer, backfill) {
                    return;
                }
                std::thread::sleep(YIELD_FOR);
            }
            if ending(writer, backfill) {
                return;
            }
            let t = thread.clone();
            let answer = writer
                .call_result("index a thread for search", move |s| {
                    s.index_thread(&t, INDEX_BUDGET)
                })
                .recv();
            match answer {
                Ok(Ok(progress)) if progress.more => continue,
                Ok(Ok(_)) => break,
                Ok(Err(e)) => {
                    tracing::warn!(thread, error = %e, "could not index a thread for search");
                    break;
                }
                // The writer is gone, refused the job (counted there) or it panicked.
                Err(_) => return,
            }
        }
        backfill.remaining.fetch_sub(1, Ordering::SeqCst);
    }
    tracing::info!("indexing threads for search: done");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SearchResults;
    use agent_core::event::{Envelope, Event, ItemKind, ItemStatus, StreamKind, TurnState};
    use std::time::Instant;

    fn reply(store: &Store, thread: &str, item: &str, text: &str) {
        let events = [
            Envelope::new(Event::ItemStarted {
                kind: ItemKind::AssistantMessage,
                title: String::new(),
                input: None,
                parent: None,
            })
            .item(item),
            Envelope::new(Event::ContentDelta {
                stream: StreamKind::Assistant,
                text: text.to_owned(),
            })
            .item(item),
            Envelope::new(Event::ItemCompleted {
                status: ItemStatus::Completed,
                output: None,
                error: None,
            })
            .item(item),
            Envelope::turn_completed(TurnState::Completed, None),
        ];
        for env in events {
            store.append_event(thread, None, &env).expect("event");
        }
    }

    fn wait_for(backfill: &Backfill) {
        let started = Instant::now();
        while !backfill.finished() {
            assert!(started.elapsed() < Duration::from_secs(30), "backfill hung");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn the_backfill_indexes_every_pending_thread_through_the_writer() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("threads.db");
        let threads: Vec<String> = {
            // Written before any writer exists, as a store from an older build would be.
            let store = Store::open(&path).expect("open");
            (0..3)
                .map(|n| {
                    let t = store.create_thread("/w", None).expect("thread");
                    reply(&store, &t, "a", &format!("backfilled number{n}"));
                    t
                })
                .collect()
        };
        let writer = StoreWriter::spawn(&path).expect("writer");
        let reader = Store::open(&path).expect("reader");
        assert_eq!(reader.search_pending().expect("pending").len(), 3);

        let backfill = spawn(writer.clone()).expect("spawn");
        wait_for(&backfill);
        assert_eq!(backfill.remaining(), 0);
        assert!(reader.search_pending().expect("pending").is_empty());
        let SearchResults::Hits(hits) = reader.search("backfilled", 10, false).expect("search")
        else {
            panic!("the index exists");
        };
        let mut found: Vec<_> = hits.into_iter().map(|h| h.thread_id).collect();
        found.sort();
        let mut want = threads.clone();
        want.sort();
        assert_eq!(found, want);
        assert_eq!(writer.failures().0, 0);

        // A second run finds nothing to do.
        let again = spawn(writer.clone()).expect("spawn");
        wait_for(&again);
        assert_eq!(again.remaining(), 0);
        assert_eq!(writer.failures().0, 0);
    }

    #[test]
    fn a_stopped_writer_ends_the_backfill_without_a_failure() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("threads.db");
        {
            let store = Store::open(&path).expect("open");
            let t = store.create_thread("/w", None).expect("thread");
            reply(&store, &t, "a", "words");
        }
        let writer = StoreWriter::spawn(&path).expect("writer");
        assert_eq!(
            writer.shutdown(Duration::from_secs(5)),
            crate::store_writer::Shutdown::Drained
        );
        let backfill = spawn(writer.clone()).expect("spawn");
        wait_for(&backfill);
        assert!(writer.is_closed());
    }
}
