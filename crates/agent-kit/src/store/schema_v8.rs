//! Schema v8: reference table, linking and indexes.
//!
//! - `drivers` is the one list of agent drivers. `provider_threads.driver` and
//!   `delegation_workers.driver` refer to it, so adding a driver is one `INSERT` in a later step
//!   (the CHECK lists it replaces were written per table, and one was missed when `local` came).
//! - Rows that hold another row's id refer to it: `threads.active_provider_thread` is a foreign
//!   key, and the delegation tables name their owners by composite keys, so an operation, message
//!   or link can only name workers of its own root, and a run of its own worker. Rows owned by a
//!   root, worker, run, operation, event or provider thread go with it (`ON DELETE CASCADE`).
//!   Three references stay `NO ACTION` on purpose. A worker's backing thread and a message's
//!   event are only removed through `release_delegation_for_delete`, which first refuses live
//!   workers. A message's `reply_to` has no length limit on its chain, and SQLite nests one
//!   foreign-key action per link up to a fixed depth (1000), so a cascade could not delete a long
//!   conversation: the release path deletes a chain's messages in one statement instead.
//! - Ownership a foreign key cannot express without nulling an owner column is a trigger: a
//!   thread's active provider thread and an event's provider thread belong to that thread.
//!   Triggers only judge later writes, and older schemas allowed either link to name another
//!   thread's provider thread; such a link is wrong by definition, so the step clears it (the
//!   thread then shows its latest provider thread, and the event names none) rather than refuse
//!   the upgrade and leave the store unopenable.
//! - Every foreign-key child column is indexed (a parent delete or `SET NULL` otherwise scans the
//!   child table), with indexes for the lookups and orderings the queries use.
//! - Text primary keys are `NOT NULL` (a rowid table's `TEXT PRIMARY KEY` otherwise allows NULL).
//!
//! `events` is not rebuilt (it is by far the largest table): it gains an index and the ownership
//! triggers only. Those triggers, and the one on `threads`, name `provider_threads`: a later step
//! that rebuilds `provider_threads` must drop them first and create them again after its rename,
//! or the rename fails on a trigger naming a table that is briefly gone. Every rebuilt rowid table keeps its rowids, since queries order by them.
//! Runs inside the migration's reserved transaction with foreign keys off, then the caller's
//! foreign-key check proves every kept row is still linked.

/// The v8 migration step.
pub(crate) const SCHEMA_V8: &str = "
CREATE TABLE drivers (key TEXT PRIMARY KEY NOT NULL) WITHOUT ROWID;
INSERT INTO drivers (key) VALUES ('claude'), ('agy'), ('codex'), ('local');

CREATE TABLE threads_v8 (
    id TEXT PRIMARY KEY NOT NULL,
    title TEXT NOT NULL,
    cwd TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    archived INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0, 1)),
    active_provider_thread TEXT REFERENCES provider_threads(id) ON DELETE SET NULL,
    read_seq INTEGER NOT NULL DEFAULT 0 CHECK (read_seq >= 0)
);
INSERT INTO threads_v8
    (rowid, id, title, cwd, created_at, updated_at, archived, active_provider_thread, read_seq)
SELECT rowid, id, title, cwd, created_at, updated_at, archived, active_provider_thread, read_seq
FROM threads;

CREATE TABLE provider_threads_v8 (
    id TEXT PRIMARY KEY NOT NULL,
    thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    driver TEXT NOT NULL REFERENCES drivers(key),
    model TEXT NOT NULL,
    native_id TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
INSERT INTO provider_threads_v8
    (rowid, id, thread_id, driver, model, native_id, created_at, updated_at)
SELECT rowid, id, thread_id, driver, model, native_id, created_at, updated_at
FROM provider_threads;

CREATE TABLE meta_v8 (
    key TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
);
INSERT INTO meta_v8 (rowid, key, value) SELECT rowid, key, value FROM meta;

CREATE TABLE delegation_roots_v8 (
    id TEXT PRIMARY KEY NOT NULL,
    root_thread_id TEXT NOT NULL UNIQUE REFERENCES threads(id),
    root_worker_id TEXT NOT NULL UNIQUE,
    task_state TEXT NOT NULL CHECK (task_state IN ('active', 'stopped', 'closed')),
    authority_epoch INTEGER NOT NULL DEFAULT 0 CHECK (authority_epoch >= 0),
    policy_revision INTEGER NOT NULL DEFAULT 0 CHECK (policy_revision >= 0),
    stream_epoch INTEGER NOT NULL DEFAULT 0 CHECK (stream_epoch >= 0),
    policy_json TEXT NOT NULL,
    continuation_budget INTEGER NOT NULL CHECK (continuation_budget >= 0),
    continuation_reserved INTEGER NOT NULL DEFAULT 0 CHECK (continuation_reserved IN (0, 1)),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (id, root_worker_id) REFERENCES delegation_workers(root_id, id)
        DEFERRABLE INITIALLY DEFERRED
);
INSERT INTO delegation_roots_v8 (rowid, id, root_thread_id, root_worker_id, task_state,
    authority_epoch, policy_revision, stream_epoch, policy_json, continuation_budget,
    continuation_reserved, created_at, updated_at)
SELECT rowid, id, root_thread_id, root_worker_id, task_state,
    authority_epoch, policy_revision, stream_epoch, policy_json, continuation_budget,
    continuation_reserved, created_at, updated_at
FROM delegation_roots;

CREATE TABLE delegation_workers_v8 (
    id TEXT PRIMARY KEY NOT NULL,
    root_id TEXT NOT NULL REFERENCES delegation_roots(id) ON DELETE CASCADE,
    parent_id TEXT,
    origin TEXT NOT NULL CHECK (origin IN ('root', 'native', 'supervised')),
    depth INTEGER NOT NULL CHECK (depth BETWEEN 0 AND 2),
    name TEXT NOT NULL,
    driver TEXT NOT NULL REFERENCES drivers(key),
    profile_id TEXT NOT NULL,
    launch_json TEXT NOT NULL,
    backing_thread_id TEXT UNIQUE REFERENCES threads(id),
    state TEXT NOT NULL CHECK (state IN ('starting', 'running', 'waiting', 'completed', 'failed',
                                         'stopping', 'stopped', 'closed', 'unknown')),
    task_mode TEXT CHECK (task_mode IN ('ask', 'accept_edits', 'plan')),
    workspace_json TEXT NOT NULL,
    identity_json TEXT,
    delegation_status TEXT NOT NULL CHECK (delegation_status IN ('available', 'workspace_untrusted',
        'rebind_required', 'restricted', 'bridge_disabled', 'unavailable')),
    delegation_reason TEXT CHECK (length(CAST(delegation_reason AS BLOB)) <= 256),
    delegation_paths TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(delegation_paths)
        AND json_type(delegation_paths) = 'array' AND json_array_length(delegation_paths) <= 16),
    last_activity TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (root_id, parent_id) REFERENCES delegation_workers(root_id, id)
        ON DELETE CASCADE,
    CHECK ((origin = 'root') = (parent_id IS NULL)),
    CHECK ((origin = 'root') = (depth = 0)),
    CHECK (origin = 'supervised' OR backing_thread_id IS NULL)
);
INSERT INTO delegation_workers_v8 (rowid, id, root_id, parent_id, origin, depth, name, driver,
    profile_id, launch_json, backing_thread_id, state, task_mode, workspace_json, identity_json,
    delegation_status, delegation_reason, delegation_paths, last_activity, created_at, updated_at)
SELECT rowid, id, root_id, parent_id, origin, depth, name, driver,
    profile_id, launch_json, backing_thread_id, state, task_mode, workspace_json, identity_json,
    delegation_status, delegation_reason, delegation_paths, last_activity, created_at, updated_at
FROM delegation_workers;

CREATE TABLE delegation_runs_v8 (
    id TEXT PRIMARY KEY NOT NULL,
    worker_id TEXT NOT NULL REFERENCES delegation_workers(id) ON DELETE CASCADE,
    process_generation INTEGER NOT NULL CHECK (process_generation >= 0),
    provider_thread_id TEXT REFERENCES provider_threads(id) ON DELETE SET NULL,
    native_turn_id TEXT,
    authority_epoch INTEGER NOT NULL CHECK (authority_epoch >= 0),
    state TEXT NOT NULL CHECK (state IN ('reserved', 'starting', 'running', 'stopping',
                                         'completed', 'interrupted', 'failed', 'unknown')),
    terminal_reason TEXT,
    usage_json TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE (worker_id, id)
);
INSERT INTO delegation_runs_v8 (rowid, id, worker_id, process_generation, provider_thread_id,
    native_turn_id, authority_epoch, state, terminal_reason, usage_json, created_at, updated_at)
SELECT rowid, id, worker_id, process_generation, provider_thread_id,
    native_turn_id, authority_epoch, state, terminal_reason, usage_json, created_at, updated_at
FROM delegation_runs;

CREATE TABLE delegation_operations_v8 (
    id TEXT PRIMARY KEY NOT NULL,
    root_id TEXT NOT NULL REFERENCES delegation_roots(id) ON DELETE CASCADE,
    principal_worker_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (length(kind) BETWEEN 1 AND 32),
    idempotency_key TEXT NOT NULL CHECK (length(idempotency_key) BETWEEN 1 AND 128),
    authority_epoch INTEGER NOT NULL CHECK (authority_epoch >= 0),
    canonical_request TEXT NOT NULL,
    resolution_json TEXT NOT NULL,
    receipt_json TEXT NOT NULL,
    dispatch_state TEXT NOT NULL CHECK (dispatch_state IN ('accepted', 'dispatch_reserved',
        'submitted', 'confirmed', 'failed', 'unknown')),
    error_json TEXT,
    target_worker_id TEXT,
    run_id TEXT REFERENCES delegation_runs(id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE (principal_worker_id, kind, idempotency_key),
    CHECK (dispatch_state <> 'failed' OR error_json IS NOT NULL),
    FOREIGN KEY (root_id, principal_worker_id) REFERENCES delegation_workers(root_id, id)
        ON DELETE CASCADE,
    FOREIGN KEY (root_id, target_worker_id) REFERENCES delegation_workers(root_id, id)
        ON DELETE CASCADE
);
INSERT INTO delegation_operations_v8 (rowid, id, root_id, principal_worker_id, kind,
    idempotency_key, authority_epoch, canonical_request, resolution_json, receipt_json,
    dispatch_state, error_json, target_worker_id, run_id, created_at, updated_at)
SELECT rowid, id, root_id, principal_worker_id, kind,
    idempotency_key, authority_epoch, canonical_request, resolution_json, receipt_json,
    dispatch_state, error_json, target_worker_id, run_id, created_at, updated_at
FROM delegation_operations;

CREATE TABLE delegation_messages_v8 (
    id TEXT PRIMARY KEY NOT NULL,
    root_id TEXT NOT NULL REFERENCES delegation_roots(id) ON DELETE CASCADE,
    operation_id TEXT REFERENCES delegation_operations(id) ON DELETE CASCADE,
    sender_worker_id TEXT NOT NULL,
    recipient_worker_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('instruction', 'report', 'question', 'reply')),
    reply_to TEXT REFERENCES delegation_messages(id),
    body TEXT NOT NULL,
    delivery_policy TEXT NOT NULL CHECK (length(delivery_policy) BETWEEN 1 AND 32),
    target_run_id TEXT,
    delivery_state TEXT NOT NULL CHECK (delivery_state IN ('queued', 'queued_next_turn',
        'dispatch_reserved', 'submitted', 'provider_confirmed', 'misdelivered', 'failed',
        'cancelled', 'unknown')),
    recipient_run_id TEXT,
    queued_at INTEGER NOT NULL,
    submitted_at INTEGER,
    confirmed_at INTEGER,
    presented_at INTEGER,
    acknowledged_at INTEGER,
    updated_at INTEGER NOT NULL,
    event_seq INTEGER UNIQUE REFERENCES events(seq),
    FOREIGN KEY (root_id, sender_worker_id) REFERENCES delegation_workers(root_id, id)
        ON DELETE CASCADE,
    FOREIGN KEY (root_id, recipient_worker_id) REFERENCES delegation_workers(root_id, id)
        ON DELETE CASCADE,
    FOREIGN KEY (recipient_worker_id, target_run_id) REFERENCES delegation_runs(worker_id, id)
        ON DELETE CASCADE,
    FOREIGN KEY (recipient_worker_id, recipient_run_id) REFERENCES delegation_runs(worker_id, id)
        ON DELETE CASCADE
);
INSERT INTO delegation_messages_v8 (rowid, id, root_id, operation_id, sender_worker_id,
    recipient_worker_id, kind, reply_to, body, delivery_policy, target_run_id, delivery_state,
    recipient_run_id, queued_at, submitted_at, confirmed_at, presented_at, acknowledged_at,
    updated_at, event_seq)
SELECT rowid, id, root_id, operation_id, sender_worker_id,
    recipient_worker_id, kind, reply_to, body, delivery_policy, target_run_id, delivery_state,
    recipient_run_id, queued_at, submitted_at, confirmed_at, presented_at, acknowledged_at,
    updated_at, event_seq
FROM delegation_messages;

CREATE TABLE delegation_event_links_v8 (
    event_seq INTEGER NOT NULL REFERENCES events(seq) ON DELETE CASCADE,
    worker_id TEXT NOT NULL,
    root_id TEXT NOT NULL,
    run_id TEXT,
    category TEXT NOT NULL CHECK (category IN ('lifecycle', 'activity', 'result', 'approval',
                                               'message', 'error')),
    invocation_id TEXT,
    process_generation INTEGER NOT NULL DEFAULT 0 CHECK (process_generation >= 0),
    PRIMARY KEY (event_seq, worker_id),
    FOREIGN KEY (root_id, worker_id) REFERENCES delegation_workers(root_id, id)
        ON DELETE CASCADE,
    FOREIGN KEY (worker_id, run_id) REFERENCES delegation_runs(worker_id, id)
        ON DELETE CASCADE
) WITHOUT ROWID;
INSERT INTO delegation_event_links_v8 (event_seq, worker_id, root_id, run_id, category,
    invocation_id, process_generation)
SELECT event_seq, worker_id, root_id, run_id, category, invocation_id, process_generation
FROM delegation_event_links;

CREATE TABLE delegation_native_aliases_v8 (
    provider_thread_id TEXT NOT NULL REFERENCES provider_threads(id) ON DELETE CASCADE,
    process_generation INTEGER NOT NULL CHECK (process_generation >= 0),
    native_worker_id TEXT NOT NULL,
    worker_id TEXT NOT NULL REFERENCES delegation_workers(id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (provider_thread_id, process_generation, native_worker_id)
) WITHOUT ROWID;
INSERT INTO delegation_native_aliases_v8 (provider_thread_id, process_generation,
    native_worker_id, worker_id, created_at)
SELECT provider_thread_id, process_generation, native_worker_id, worker_id, created_at
FROM delegation_native_aliases;

DROP TABLE delegation_native_aliases;
DROP TABLE delegation_event_links;
DROP TABLE delegation_messages;
DROP TABLE delegation_operations;
DROP TABLE delegation_runs;
DROP TABLE delegation_workers;
DROP TABLE delegation_roots;
DROP TABLE meta;
DROP TABLE provider_threads;
DROP TABLE threads;

ALTER TABLE threads_v8 RENAME TO threads;
ALTER TABLE provider_threads_v8 RENAME TO provider_threads;
ALTER TABLE meta_v8 RENAME TO meta;
ALTER TABLE delegation_roots_v8 RENAME TO delegation_roots;
ALTER TABLE delegation_workers_v8 RENAME TO delegation_workers;
ALTER TABLE delegation_runs_v8 RENAME TO delegation_runs;
ALTER TABLE delegation_operations_v8 RENAME TO delegation_operations;
ALTER TABLE delegation_messages_v8 RENAME TO delegation_messages;
ALTER TABLE delegation_event_links_v8 RENAME TO delegation_event_links;
ALTER TABLE delegation_native_aliases_v8 RENAME TO delegation_native_aliases;

CREATE INDEX threads_updated ON threads(updated_at);
CREATE INDEX threads_active_provider ON threads(active_provider_thread);
CREATE INDEX provider_threads_thread ON provider_threads(thread_id, created_at);
CREATE INDEX provider_threads_native ON provider_threads(driver, native_id, created_at);
CREATE INDEX events_provider_thread ON events(provider_thread_id);

CREATE UNIQUE INDEX delegation_workers_root_id ON delegation_workers(root_id, id);
CREATE UNIQUE INDEX delegation_workers_one_root ON delegation_workers(root_id)
    WHERE origin = 'root';
CREATE INDEX delegation_workers_parent ON delegation_workers(root_id, parent_id);
CREATE INDEX delegation_workers_created ON delegation_workers(root_id, created_at);
CREATE INDEX delegation_workers_state ON delegation_workers(state);

CREATE UNIQUE INDEX delegation_runs_one_active ON delegation_runs(worker_id)
    WHERE state IN ('reserved', 'starting', 'running', 'stopping');
CREATE INDEX delegation_runs_worker ON delegation_runs(worker_id, created_at);
CREATE INDEX delegation_runs_state ON delegation_runs(state, worker_id);
CREATE INDEX delegation_runs_provider_thread ON delegation_runs(provider_thread_id);

CREATE INDEX delegation_operations_root ON delegation_operations(root_id, created_at);
CREATE INDEX delegation_operations_state ON delegation_operations(dispatch_state, root_id);
CREATE INDEX delegation_operations_target ON delegation_operations(target_worker_id);
CREATE INDEX delegation_operations_run ON delegation_operations(run_id);

CREATE INDEX delegation_messages_mailbox
    ON delegation_messages(recipient_worker_id, event_seq, root_id);
CREATE INDEX delegation_messages_root ON delegation_messages(root_id, queued_at);
CREATE INDEX delegation_messages_state ON delegation_messages(delivery_state, root_id);
CREATE INDEX delegation_messages_operation ON delegation_messages(operation_id);
CREATE INDEX delegation_messages_sender ON delegation_messages(sender_worker_id);
CREATE INDEX delegation_messages_reply_to ON delegation_messages(reply_to);
CREATE INDEX delegation_messages_target_run ON delegation_messages(target_run_id);
CREATE INDEX delegation_messages_recipient_run ON delegation_messages(recipient_run_id);

CREATE INDEX delegation_event_links_root ON delegation_event_links(root_id, event_seq);
CREATE INDEX delegation_event_links_worker ON delegation_event_links(worker_id, event_seq);
CREATE INDEX delegation_event_links_run ON delegation_event_links(run_id);

CREATE INDEX delegation_native_aliases_worker ON delegation_native_aliases(worker_id);

UPDATE threads SET active_provider_thread = NULL
WHERE active_provider_thread IS NOT NULL AND NOT EXISTS (SELECT 1 FROM provider_threads p
      WHERE p.id = threads.active_provider_thread AND p.thread_id = threads.id);
UPDATE events SET provider_thread_id = NULL
WHERE seq IN (SELECT e.seq FROM provider_threads p
              JOIN events e INDEXED BY events_provider_thread ON e.provider_thread_id = p.id
              WHERE e.thread_id IS NOT p.thread_id);

CREATE TRIGGER threads_active_provider_owned_insert BEFORE INSERT ON threads
WHEN NEW.active_provider_thread IS NOT NULL AND NOT EXISTS (SELECT 1 FROM provider_threads
     WHERE id = NEW.active_provider_thread AND thread_id = NEW.id)
BEGIN SELECT RAISE(ABORT, 'a thread''s active provider thread is one of its own'); END;
CREATE TRIGGER threads_active_provider_owned_update
BEFORE UPDATE OF id, active_provider_thread ON threads
WHEN NEW.active_provider_thread IS NOT NULL AND NOT EXISTS (SELECT 1 FROM provider_threads
     WHERE id = NEW.active_provider_thread AND thread_id = NEW.id)
BEGIN SELECT RAISE(ABORT, 'a thread''s active provider thread is one of its own'); END;
CREATE TRIGGER events_provider_owned_insert BEFORE INSERT ON events
WHEN NEW.provider_thread_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM provider_threads
     WHERE id = NEW.provider_thread_id AND thread_id = NEW.thread_id)
BEGIN SELECT RAISE(ABORT, 'an event''s provider thread belongs to its thread'); END;
CREATE TRIGGER events_provider_owned_update
BEFORE UPDATE OF thread_id, provider_thread_id ON events
WHEN NEW.provider_thread_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM provider_threads
     WHERE id = NEW.provider_thread_id AND thread_id = NEW.thread_id)
BEGIN SELECT RAISE(ABORT, 'an event''s provider thread belongs to its thread'); END;
CREATE TRIGGER provider_threads_owner_update BEFORE UPDATE OF thread_id ON provider_threads
WHEN NEW.thread_id IS NOT OLD.thread_id
BEGIN SELECT RAISE(ABORT, 'a provider thread stays with its thread'); END;

CREATE TRIGGER delegation_workers_depth BEFORE INSERT ON delegation_workers
WHEN NEW.parent_id IS NOT NULL
 AND NEW.depth IS NOT (SELECT depth + 1 FROM delegation_workers WHERE id = NEW.parent_id)
BEGIN SELECT RAISE(ABORT, 'a delegation worker is one level below its parent'); END;
CREATE TRIGGER delegation_workers_lineage BEFORE UPDATE ON delegation_workers
WHEN NEW.id IS NOT OLD.id OR NEW.root_id IS NOT OLD.root_id
  OR NEW.parent_id IS NOT OLD.parent_id OR NEW.origin IS NOT OLD.origin
  OR NEW.depth IS NOT OLD.depth OR NEW.created_at IS NOT OLD.created_at
  OR (OLD.backing_thread_id IS NOT NULL AND NEW.backing_thread_id IS NOT OLD.backing_thread_id)
BEGIN SELECT RAISE(ABORT, 'a delegation worker''s lineage is immutable'); END;
CREATE TRIGGER delegation_runs_frozen BEFORE UPDATE ON delegation_runs
WHEN NEW.id IS NOT OLD.id OR NEW.worker_id IS NOT OLD.worker_id
  OR NEW.process_generation IS NOT OLD.process_generation
  OR NEW.authority_epoch IS NOT OLD.authority_epoch OR NEW.created_at IS NOT OLD.created_at
  OR (OLD.state IN ('completed', 'interrupted', 'failed') AND NEW.state IS NOT OLD.state)
BEGIN SELECT RAISE(ABORT, 'a delegation run''s identity and terminal state are final'); END;
CREATE TRIGGER delegation_operations_frozen BEFORE UPDATE ON delegation_operations
WHEN NEW.id IS NOT OLD.id OR NEW.root_id IS NOT OLD.root_id
  OR NEW.principal_worker_id IS NOT OLD.principal_worker_id OR NEW.kind IS NOT OLD.kind
  OR NEW.idempotency_key IS NOT OLD.idempotency_key
  OR NEW.authority_epoch IS NOT OLD.authority_epoch
  OR NEW.canonical_request IS NOT OLD.canonical_request
  OR NEW.resolution_json IS NOT OLD.resolution_json
  OR NEW.receipt_json IS NOT OLD.receipt_json
  OR NEW.target_worker_id IS NOT OLD.target_worker_id OR NEW.created_at IS NOT OLD.created_at
  OR (OLD.run_id IS NOT NULL AND NEW.run_id IS NOT OLD.run_id)
BEGIN SELECT RAISE(ABORT, 'a delegation operation''s request, resolution and receipt are frozen');
END;
CREATE TRIGGER delegation_messages_frozen BEFORE UPDATE ON delegation_messages
WHEN NEW.id IS NOT OLD.id OR NEW.root_id IS NOT OLD.root_id
  OR NEW.operation_id IS NOT OLD.operation_id
  OR NEW.sender_worker_id IS NOT OLD.sender_worker_id
  OR NEW.recipient_worker_id IS NOT OLD.recipient_worker_id OR NEW.kind IS NOT OLD.kind
  OR NEW.reply_to IS NOT OLD.reply_to OR NEW.body IS NOT OLD.body
  OR NEW.delivery_policy IS NOT OLD.delivery_policy
  OR NEW.target_run_id IS NOT OLD.target_run_id OR NEW.queued_at IS NOT OLD.queued_at
  OR NEW.event_seq IS NOT OLD.event_seq
  OR (OLD.recipient_run_id IS NOT NULL AND NEW.recipient_run_id IS NOT OLD.recipient_run_id)
  OR (OLD.presented_at IS NOT NULL AND NEW.presented_at IS NOT OLD.presented_at)
  OR (OLD.delivery_state IN ('provider_confirmed', 'misdelivered', 'failed', 'cancelled')
      AND NEW.delivery_state IS NOT OLD.delivery_state)
BEGIN SELECT RAISE(ABORT, 'a delegation message''s accepted fields and set-once facts are final');
END;

INSERT INTO schema_version (version) VALUES (8);
";
