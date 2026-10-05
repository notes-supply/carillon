//! Durable operation state. This module owns every turn and delivery state
//! transition; nothing else writes lifecycle state.
//!
//! ```text
//! turn:      queued -> running -> completed
//!                   ^         \-> interrupted
//!                   \--------- running (runtime provably never reached)
//!
//! delivery:  pending -> delivering -> delivered
//!                                 \-> uncertain (ack lost or claim abandoned)
//! ```
//!
//! Invariants:
//!
//! - One accepted event is one turn row, keyed by (interface, event); the row
//!   carries what the event said and what happened to it.
//! - Messages in one conversation (interface, agent, audience) run in
//!   acceptance order. Only queued or running predecessors block later
//!   messages. An observation (a turn whose `follow_up_json` holds a
//!   runtime's cursor) neither waits for messages nor holds them back.
//!   Session continuity belongs to the runtime.
//! - A delivery row exists exactly when its turn left `running`; completion
//!   output and the delivery intent commit in one
//!   transaction.
//! - Transitions are guarded conditional updates that succeed exactly once, so
//!   a stale caller fails instead of overwriting newer state.
//! - A turn's attachment references commit with the turn, and only after the
//!   capture they name is published and synced; a losing duplicate's capture
//!   is never referenced.

use std::{
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use rusqlite::{
    Connection, OptionalExtension, Row, Transaction, TransactionBehavior, params,
    types::{FromSql, FromSqlError, FromSqlResult, Type, ValueRef},
};
use serde::de::DeserializeOwned;
use serde_json::Value;

use carillon_protocol::model::{
    ActiveTurn, Delivery, DeliveryOutcome, DeliveryState, EventReceipt, ExternalEvent,
    RuntimeTurnResponse, StateCounts, TurnInspection, TurnState, TurnSummary,
};

use crate::{
    attachments::{Capture, CapturedFile},
    engine::Lane,
};

#[derive(Clone)]
pub struct Database {
    path: PathBuf,
}

/// A queued turn after this process moved it to `running`. Holding one is the
/// permission to execute runtime work and to record its outcome.
#[derive(Clone, Debug)]
pub struct ClaimedTurn {
    pub turn_id: String,
    pub interface_id: String,
    pub agent_id: String,
    pub audience_id: String,
    pub runtime_id: String,
    pub target_id: String,
    pub feedback_target_id: String,
    pub input: String,
    pub resource_snapshot_id: String,
    pub native: Value,
    /// The runtime's cursor, on an observation; `None` on a message.
    pub follow_up: Option<Value>,
    pub attachments: Vec<CapturedFile>,
}

/// One earlier turn's captured files.
#[derive(Clone, Debug)]
pub(crate) struct TurnFiles {
    pub(crate) turn_id: String,
    pub(crate) files: Vec<CapturedFile>,
}

/// Lifecycle states are stored by their wire names. The wire types live in
/// the protocol crate, so they are read through this wrapper rather than a
/// foreign `FromSql` implementation.
struct Stored<T>(T);

impl<T: FromStr<Err = anyhow::Error>> FromSql for Stored<T> {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        value
            .as_str()?
            .parse()
            .map(Self)
            .map_err(|error: anyhow::Error| FromSqlError::Other(error.into()))
    }
}

fn state_column<T: FromStr<Err = anyhow::Error>>(
    row: &Row<'_>,
    index: usize,
) -> rusqlite::Result<T> {
    row.get::<_, Stored<T>>(index).map(|stored| stored.0)
}

fn optional_state_column<T: FromStr<Err = anyhow::Error>>(
    row: &Row<'_>,
    index: usize,
) -> rusqlite::Result<Option<T>> {
    row.get::<_, Option<Stored<T>>>(index)
        .map(|stored| stored.map(|stored| stored.0))
}

impl Database {
    pub fn create(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)
                .with_context(|| format!("create {}", parent.display()))?;
            let metadata = std::fs::symlink_metadata(parent)?;
            if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
                bail!(
                    "database directory must be private (0700): {}",
                    parent.display()
                );
            }
        }
        let database = Self {
            path: path.to_path_buf(),
        };
        // Reserve the name atomically before SQLite opens it. Existing files,
        // including symlinks and incomplete prior creations, are never adopted.
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("create fresh database {}", path.display()))?;
        let connection =
            Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        Self::initialize_connection(&connection)?;
        Ok(database)
    }

    pub fn open(path: &Path) -> Result<Self> {
        Self::validate_backup(path)?;
        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    fn connect(&self) -> Result<Connection> {
        let connection =
            Connection::open_with_flags(&self.path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
                .with_context(|| format!("open {}", self.path.display()))?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(connection)
    }

    /// Validate without creating a missing database or repairing an old schema.
    pub fn validate_backup(path: &Path) -> Result<()> {
        let connection =
            Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let integrity: String =
            connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            bail!("SQLite integrity check failed: {integrity}");
        }
        if connection.prepare("PRAGMA foreign_key_check")?.exists([])? {
            bail!("SQLite foreign key check failed");
        }
        let current = Connection::open_in_memory()?;
        Self::initialize_connection(&current)?;
        let schema = |database: &Connection| -> Result<Vec<(String, String, String)>> {
            Ok(database.prepare(
                "SELECT type, name, sql FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name"
            )?.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        };
        if schema(&connection)? != schema(&current)? {
            bail!("database schema is not the current Carillon format");
        }
        Ok(())
    }

    /// Validate every captured resource reference before archiving or restoring.
    pub fn validate_resource_snapshots(&self, store: &Path) -> Result<()> {
        let connection =
            Connection::open_with_flags(&self.path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let mut statement =
            connection.prepare("SELECT DISTINCT resource_snapshot_id FROM turns")?;
        for id in statement.query_map([], |row| row.get::<_, String>(0))? {
            crate::resources::validate(&id?, store)
                .context("backup has missing or altered captured resources")?;
        }
        Ok(())
    }

    /// Every committed attachment reference, for validating the store before
    /// archiving or restoring.
    pub fn captured_files(&self) -> Result<Vec<CapturedFile>> {
        let connection =
            Connection::open_with_flags(&self.path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        captured_files(&connection, None)
    }

    /// The captures committed turns reference.
    pub fn captures(&self) -> Result<std::collections::BTreeSet<String>> {
        let connection = self.connect()?;
        let mut statement =
            connection.prepare("SELECT DISTINCT capture_id FROM turn_attachments")?;
        let captures = statement
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        Ok(captures)
    }

    /// Whether a committed turn references this capture.
    pub fn capture_referenced(&self, capture: &str) -> Result<bool> {
        Ok(self.connect()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM turn_attachments WHERE capture_id = ?1)",
            [capture],
            |row| row.get(0),
        )?)
    }

    /// Bytes kept by every committed capture.
    pub fn captured_bytes(&self) -> Result<u64> {
        let bytes: i64 = self.connect()?.query_row(
            "SELECT COALESCE(SUM(size), 0) FROM turn_attachments",
            [],
            |row| row.get(0),
        )?;
        Ok(u64::try_from(bytes)?)
    }

    fn initialize_connection(connection: &Connection) -> Result<()> {
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS turns (
                acceptance_sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                id TEXT NOT NULL UNIQUE,
                interface_id TEXT NOT NULL,
                event_id TEXT NOT NULL,
                agent_id TEXT NOT NULL,
                audience_id TEXT NOT NULL,
                user_id TEXT NOT NULL,
                target_id TEXT NOT NULL,
                feedback_target_id TEXT NOT NULL,
                input TEXT NOT NULL,
                native_json TEXT NOT NULL,
                follow_up_json TEXT,
                runtime_id TEXT NOT NULL,
                provider_snapshot_json TEXT NOT NULL,
                resource_snapshot_id TEXT NOT NULL,
                state TEXT NOT NULL,
                output TEXT,
                error TEXT,
                accepted_at INTEGER NOT NULL,
                finished_at INTEGER,
                UNIQUE(interface_id, event_id)
            );
            CREATE INDEX IF NOT EXISTS turns_conversation
                ON turns(interface_id, agent_id, audience_id, acceptance_sequence);
            CREATE INDEX IF NOT EXISTS turns_queued
                ON turns(runtime_id, acceptance_sequence) WHERE state = 'queued';
            CREATE INDEX IF NOT EXISTS turns_conversation_unfinished
                ON turns(interface_id, agent_id, audience_id, acceptance_sequence)
                WHERE state IN ('queued', 'running');
            CREATE INDEX IF NOT EXISTS turns_interface_unfinished
                ON turns(interface_id, acceptance_sequence)
                WHERE state IN ('queued', 'running');
            CREATE TABLE IF NOT EXISTS deliveries (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                turn_id TEXT NOT NULL UNIQUE,
                interface_id TEXT NOT NULL,
                target_id TEXT NOT NULL,
                feedback_target_id TEXT NOT NULL,
                content TEXT NOT NULL,
                state TEXT NOT NULL,
                native_ref TEXT,
                claim_owner TEXT,
                updated_at INTEGER NOT NULL,
                FOREIGN KEY(turn_id) REFERENCES turns(id)
            );
            CREATE INDEX IF NOT EXISTS deliveries_open
                ON deliveries(interface_id, id) WHERE state IN ('pending', 'delivering');
            CREATE TABLE IF NOT EXISTS turn_attachments (
                turn_id TEXT NOT NULL REFERENCES turns(id),
                ordinal INTEGER NOT NULL,
                capture_id TEXT NOT NULL,
                name TEXT NOT NULL,
                size INTEGER NOT NULL,
                content_type TEXT,
                sha256 TEXT NOT NULL,
                noted INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY(turn_id, ordinal)
            );
            CREATE INDEX IF NOT EXISTS turn_attachments_capture
                ON turn_attachments(capture_id);
            CREATE INDEX IF NOT EXISTS turn_attachments_pending
                ON turn_attachments(turn_id) WHERE noted = 0;",
        )?;
        Ok(())
    }

    /// Reads an accepted event without depending on current runtime resources.
    pub fn event_receipt(
        &self,
        interface_id: &str,
        event_id: &str,
    ) -> Result<Option<EventReceipt>> {
        event_receipt(&self.connect()?, interface_id, event_id)
    }

    /// The first accepted turn pins a conversation, even before its runtime
    /// has created a native session. Changing the default cannot move it.
    pub fn conversation_runtime(
        &self,
        interface_id: &str,
        agent_id: &str,
        audience_id: &str,
    ) -> Result<Option<String>> {
        conversation_runtime(&self.connect()?, interface_id, agent_id, audience_id)
    }

    /// Records an admitted event as a queued turn, or returns the receipt of
    /// the turn a previous submission of the same event already created.
    /// A published capture of the event's attachments is referenced only by
    /// the turn this call creates; a duplicate receipt leaves it unreferenced.
    pub fn accept_event(
        &self,
        interface_id: &str,
        runtime_id: &str,
        event: &ExternalEvent,
        providers: &Value,
        resource_snapshot_id: &str,
        capture: Option<&Capture>,
    ) -> Result<EventReceipt> {
        let mut connection = self.connect()?;
        let transaction = write_transaction(&mut connection)?;
        if let Some(receipt) = event_receipt(&transaction, interface_id, &event.event_id)? {
            transaction.commit()?;
            return Ok(receipt);
        }

        if let Some(owner) = conversation_runtime(
            &transaction,
            interface_id,
            &event.agent_id,
            &event.audience_id,
        )? && owner != runtime_id
        {
            bail!("conversation is owned by runtime {owner}, not {runtime_id}");
        }
        let turn_id = uuid::Uuid::now_v7().to_string();
        transaction.execute(
            "INSERT INTO turns(id, interface_id, event_id, agent_id, audience_id, user_id,
                 target_id, feedback_target_id, input, native_json, runtime_id,
                 provider_snapshot_json, resource_snapshot_id, state, accepted_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, 'queued', ?14)",
            params![
                turn_id,
                interface_id,
                event.event_id,
                event.agent_id,
                event.audience_id,
                event.user_id,
                event.delivery_target_id,
                event.feedback_target_id,
                event.content,
                serde_json::to_string(&event.native)?,
                runtime_id,
                serde_json::to_string(providers)?,
                resource_snapshot_id,
                timestamp()?,
            ],
        )?;
        if let Some(capture) = capture {
            let files = event
                .attachments
                .as_ref()
                .map(|staged| staged.files.as_slice())
                .unwrap_or_default();
            if files.len() != capture.digests.len() {
                bail!("captured attachments do not match the event");
            }
            for (ordinal, (file, digest)) in files.iter().zip(&capture.digests).enumerate() {
                transaction.execute(
                    "INSERT INTO turn_attachments(turn_id, ordinal, capture_id, name, size,
                         content_type, sha256)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        turn_id,
                        u32::try_from(ordinal)?,
                        capture.id,
                        file.name,
                        i64::try_from(file.size)?,
                        file.content_type,
                        digest,
                    ],
                )?;
            }
        }
        transaction.commit()?;
        Ok(EventReceipt {
            event_id: event.event_id.clone(),
            turn_id,
            state: TurnState::Queued,
            duplicate: false,
        })
    }

    /// Claims a runtime's oldest claimable turn of a lane and marks it
    /// running.
    ///
    /// A message is claimable only when its conversation has no earlier
    /// message queued or running, which serializes each conversation FIFO.
    /// Interrupted turns are terminal and never hold later messages. An
    /// observation only reads native records, so it neither waits for
    /// messages nor holds them back.
    pub fn claim(&self, runtime_id: &str, lane: Lane) -> Result<Option<ClaimedTurn>> {
        let query = match lane {
            Lane::Messages => {
                "SELECT id, interface_id, agent_id, audience_id, runtime_id, target_id,
                    feedback_target_id, input, resource_snapshot_id, native_json, follow_up_json
                 FROM turns t
                 WHERE runtime_id = ?1 AND state = 'queued' AND follow_up_json IS NULL
                   AND NOT EXISTS (
                     SELECT 1 FROM turns earlier
                     WHERE earlier.interface_id = t.interface_id
                       AND earlier.agent_id = t.agent_id
                       AND earlier.audience_id = t.audience_id
                       AND earlier.state IN ('queued', 'running')
                       AND earlier.acceptance_sequence < t.acceptance_sequence
                       AND earlier.follow_up_json IS NULL
                   )
                 ORDER BY acceptance_sequence LIMIT 1"
            }
            Lane::Observations => {
                "SELECT id, interface_id, agent_id, audience_id, runtime_id, target_id,
                    feedback_target_id, input, resource_snapshot_id, native_json, follow_up_json
                 FROM turns
                 WHERE runtime_id = ?1 AND state = 'queued' AND follow_up_json IS NOT NULL
                 ORDER BY acceptance_sequence LIMIT 1"
            }
        };
        let mut connection = self.connect()?;
        let transaction = write_transaction(&mut connection)?;
        let turn = transaction
            .query_row(query, [runtime_id], |row| {
                Ok(ClaimedTurn {
                    turn_id: row.get(0)?,
                    interface_id: row.get(1)?,
                    agent_id: row.get(2)?,
                    audience_id: row.get(3)?,
                    runtime_id: row.get(4)?,
                    target_id: row.get(5)?,
                    feedback_target_id: row.get(6)?,
                    input: row.get(7)?,
                    resource_snapshot_id: row.get(8)?,
                    native: json_column(row, 9)?,
                    follow_up: optional_json_column(row, 10)?,
                    attachments: Vec::new(),
                })
            })
            .optional()?;
        let Some(mut turn) = turn else {
            transaction.commit()?;
            return Ok(None);
        };
        let changed = transaction.execute(
            "UPDATE turns SET state = 'running' WHERE id = ?1 AND state = 'queued'",
            [&turn.turn_id],
        )?;
        if changed != 1 {
            bail!("turn {} could not be claimed", turn.turn_id);
        }
        turn.attachments = captured_files(&transaction, Some(&turn.turn_id))?;
        transaction.commit()?;
        Ok(Some(turn))
    }

    /// Files of `turn`'s conversation, accepted before it, that no completed
    /// turn has carried yet: whole turns, oldest first. The first is always
    /// taken; later ones while together they hold at most `files` files and
    /// `bytes` bytes. The rest wait for later turns.
    pub(crate) fn unnoted_files(
        &self,
        turn: &ClaimedTurn,
        files: u32,
        bytes: u64,
    ) -> Result<Vec<TurnFiles>> {
        let connection = self.connect()?;
        let mut statement = connection.prepare(
            "SELECT DISTINCT earlier.id, earlier.acceptance_sequence
             FROM turn_attachments pending
             JOIN turns earlier ON earlier.id = pending.turn_id
             JOIN turns current ON current.id = ?1
             WHERE pending.noted = 0
               AND earlier.interface_id = current.interface_id
               AND earlier.agent_id = current.agent_id
               AND earlier.audience_id = current.audience_id
               AND earlier.acceptance_sequence < current.acceptance_sequence
             ORDER BY earlier.acceptance_sequence",
        )?;
        let mut rows = statement.query([&turn.turn_id])?;
        let (mut unnoted, mut count, mut total) = (Vec::new(), 0_u64, 0_u64);
        while let Some(row) = rows.next()? {
            let turn_id: String = row.get(0)?;
            let captured = captured_files(&connection, Some(&turn_id))?;
            count += captured.len() as u64;
            total += captured.iter().map(|file| file.size).sum::<u64>();
            if !unnoted.is_empty() && (count > u64::from(files) || total > bytes) {
                break;
            }
            unnoted.push(TurnFiles {
                turn_id,
                files: captured,
            });
        }
        Ok(unnoted)
    }

    /// Record that a completed turn carried the files of `turns` to its
    /// runtime, whose conversation therefore holds them.
    pub(crate) fn mark_noted(&self, turns: &[String]) -> Result<()> {
        let mut connection = self.connect()?;
        let transaction = write_transaction(&mut connection)?;
        for turn in turns {
            transaction.execute(
                "UPDATE turn_attachments SET noted = 1 WHERE turn_id = ?1 AND noted = 0",
                [turn],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Commits a successful runtime response: completed output and the
    /// delivery intent, atomically. A response with a follow-up cursor also
    /// queues, in the same transaction, the observation that will carry the
    /// report of the work this turn left running: same conversation, target
    /// and runtime, carrying the cursor and nothing an interface sent. It
    /// does not hold back the conversation's later messages.
    pub fn complete_turn(&self, turn: &ClaimedTurn, response: &RuntimeTurnResponse) -> Result<()> {
        let mut connection = self.connect()?;
        let transaction = write_transaction(&mut connection)?;
        let now = timestamp()?;
        let changed = transaction.execute(
            "UPDATE turns SET state = 'completed', output = ?3, finished_at = ?4
             WHERE id = ?1 AND runtime_id = ?2 AND state = 'running'",
            params![turn.turn_id, turn.runtime_id, response.output, now],
        )?;
        if changed != 1 {
            bail!("turn {} was not running", turn.turn_id);
        }
        enqueue_delivery(&transaction, turn, &response.output, now)?;
        if let Some(cursor) = &response.follow_up {
            transaction.execute(
                "INSERT INTO turns(id, interface_id, event_id, agent_id, audience_id, user_id,
                     target_id, feedback_target_id, input, native_json, follow_up_json,
                     runtime_id, provider_snapshot_json, resource_snapshot_id, state,
                     accepted_at)
                 SELECT ?2, interface_id, 'follow-up:' || id, agent_id, audience_id, user_id,
                     target_id, feedback_target_id, '', '{}', ?3, runtime_id,
                     provider_snapshot_json, resource_snapshot_id, 'queued', ?4
                 FROM turns WHERE id = ?1",
                params![
                    turn.turn_id,
                    uuid::Uuid::now_v7().to_string(),
                    serde_json::to_string(cursor)?,
                    now,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Records that a running turn did not complete, and queues a delivery
    /// telling the conversation so, with the runtime's explanation when it
    /// gave one. `error` is the operator's diagnostic.
    pub fn interrupt_turn(
        &self,
        turn: &ClaimedTurn,
        error: &str,
        explanation: Option<&str>,
    ) -> Result<()> {
        const CONTINUE: &str = "Send another message to continue in this conversation. Actions already taken may have had effects.";
        let mut connection = self.connect()?;
        let transaction = write_transaction(&mut connection)?;
        let now = timestamp()?;
        let changed = transaction.execute(
            "UPDATE turns SET state = 'interrupted', error = ?3, finished_at = ?4
             WHERE id = ?1 AND runtime_id = ?2 AND state = 'running'",
            params![turn.turn_id, turn.runtime_id, error, now],
        )?;
        if changed != 1 {
            bail!("turn {} was not running", turn.turn_id);
        }
        let notice = match explanation {
            Some(explanation) => format!("That turn was interrupted: {explanation}\n{CONTINUE}"),
            None => format!("That turn was interrupted. {CONTINUE}"),
        };
        enqueue_delivery(&transaction, turn, &notice, now)?;
        transaction.commit()?;
        Ok(())
    }

    /// Returns a running turn to the queue. Only valid when the runtime
    /// provably never received the work, or for an observation, which only
    /// reads.
    pub fn requeue_turn(&self, turn: &ClaimedTurn) -> Result<()> {
        let changed = self.connect()?.execute(
            "UPDATE turns SET state = 'queued' WHERE id = ?1 AND runtime_id = ?2 AND state = 'running'",
            params![turn.turn_id, turn.runtime_id],
        )?;
        if changed != 1 {
            bail!("turn {} was not running", turn.turn_id);
        }
        Ok(())
    }

    /// Marks every message still recorded as running as interrupted, and
    /// returns running observations to the queue. Run at startup: a message
    /// that was running when core stopped may or may not have executed, and
    /// Carillon does not infer that repeating it is safe; an observation
    /// only reads native records, so it is asked again and delivers its
    /// report once, when it completes.
    pub fn recover_running_turns(&self) -> Result<usize> {
        let mut connection = self.connect()?;
        let transaction = write_transaction(&mut connection)?;
        let now = timestamp()?;
        transaction.execute(
            "INSERT INTO deliveries(turn_id, interface_id, target_id, feedback_target_id,
                 content, state, updated_at)
             SELECT id, interface_id, target_id, feedback_target_id, ?1, 'pending', ?2
             FROM turns WHERE state = 'running' AND follow_up_json IS NULL",
            params![
                "Carillon restarted during that turn. It was not run again. Send another message to continue in this conversation.",
                now,
            ],
        )?;
        let recovered = transaction.execute(
            "UPDATE turns SET state = 'interrupted', error = ?1, finished_at = ?2
             WHERE state = 'running' AND follow_up_json IS NULL",
            params!["core restarted while the runtime was executing", now],
        )?;
        transaction.execute(
            "UPDATE turns SET state = 'queued'
             WHERE state = 'running' AND follow_up_json IS NOT NULL",
            [],
        )?;
        transaction.commit()?;
        Ok(recovered)
    }

    /// Releases delivery claims held by other, presumed dead, instances of an
    /// interface as uncertain, and returns the feedback targets of every
    /// uncertain delivery so the new instance can surface them.
    pub fn reconcile_delivery_claims(
        &self,
        interface_id: &str,
        owner: &str,
    ) -> Result<Vec<String>> {
        let mut connection = self.connect()?;
        let transaction = write_transaction(&mut connection)?;
        transaction.execute(
            "UPDATE deliveries SET state = 'uncertain', claim_owner = NULL, updated_at = ?3
             WHERE interface_id = ?1 AND state = 'delivering'
               AND (claim_owner IS NULL OR claim_owner <> ?2)",
            params![interface_id, owner, timestamp()?],
        )?;
        let targets = {
            let mut statement = transaction.prepare(
                "SELECT DISTINCT feedback_target_id FROM deliveries
                 WHERE interface_id = ?1 AND state = 'uncertain' AND feedback_target_id <> ''",
            )?;
            statement
                .query_map([interface_id], |row| row.get(0))?
                .collect::<Result<Vec<String>, _>>()?
        };
        transaction.commit()?;
        Ok(targets)
    }

    /// Claims the oldest pending delivery for an interface instance.
    pub fn claim_delivery(&self, interface_id: &str, owner: &str) -> Result<Option<Delivery>> {
        let mut connection = self.connect()?;
        let transaction = write_transaction(&mut connection)?;
        let delivery = transaction
            .query_row(
                "SELECT id, turn_id, target_id, feedback_target_id, content,
                        (SELECT state FROM turns WHERE turns.id = deliveries.turn_id)
                 FROM deliveries
                 WHERE interface_id = ?1 AND state IN ('pending', 'delivering')
                   AND (state = 'pending' OR claim_owner = ?2)
                 ORDER BY CASE state WHEN 'delivering' THEN 0 ELSE 1 END, id LIMIT 1",
                params![interface_id, owner],
                |row| {
                    Ok(Delivery {
                        id: row.get(0)?,
                        turn_id: row.get(1)?,
                        target_id: row.get(2)?,
                        feedback_target_id: row.get(3)?,
                        content: row.get(4)?,
                        turn_state: state_column::<TurnState>(row, 5)?,
                    })
                },
            )
            .optional()?;
        if let Some(value) = &delivery {
            let changed = transaction.execute(
                "UPDATE deliveries SET state = 'delivering', claim_owner = ?2, updated_at = ?3
                 WHERE id = ?1 AND state = 'pending'",
                params![value.id, owner, timestamp()?],
            )?;
            if changed == 0 {
                let still_owned = transaction.query_row(
                    "SELECT EXISTS(
                        SELECT 1 FROM deliveries
                        WHERE id = ?1 AND interface_id = ?2
                          AND state = 'delivering' AND claim_owner = ?3
                    )",
                    params![value.id, interface_id, owner],
                    |row| row.get::<_, bool>(0),
                )?;
                if !still_owned {
                    bail!("delivery {} could not be claimed", value.id);
                }
            }
        }
        transaction.commit()?;
        Ok(delivery)
    }

    /// Records the reported outcome of a claimed delivery. Repeating the same
    /// acknowledgement is safe when the first commit's response was lost.
    pub fn acknowledge_delivery(
        &self,
        interface_id: &str,
        id: i64,
        owner: &str,
        outcome: DeliveryOutcome,
        native_ref: Option<&str>,
    ) -> Result<bool> {
        let mut connection = self.connect()?;
        let transaction = write_transaction(&mut connection)?;
        let desired = DeliveryState::from(outcome);
        let changed = transaction.execute(
            "UPDATE deliveries SET state = ?4, native_ref = ?5, claim_owner = NULL, updated_at = ?6
             WHERE id = ?1 AND interface_id = ?2 AND claim_owner = ?3 AND state = 'delivering'",
            params![
                id,
                interface_id,
                owner,
                desired.as_str(),
                native_ref,
                timestamp()?
            ],
        )?;
        if changed == 1 {
            transaction.commit()?;
            return Ok(true);
        }
        let already_recorded = transaction
            .query_row(
                "SELECT state, native_ref FROM deliveries WHERE id = ?1 AND interface_id = ?2",
                params![id, interface_id],
                |row| {
                    Ok((
                        state_column::<DeliveryState>(row, 0)?,
                        row.get::<_, Option<String>>(1)?,
                    ))
                },
            )
            .optional()?
            .is_some_and(|(state, recorded_ref)| {
                state == desired && recorded_ref.as_deref() == native_ref
            });
        transaction.commit()?;
        Ok(already_recorded)
    }

    pub fn inspect_turn(&self, turn_id: &str) -> Result<Option<TurnInspection>> {
        let connection = self.connect()?;
        let inspection = connection
            .query_row(
                "SELECT t.id, t.event_id, t.audience_id, t.user_id, t.input, t.state, t.output,
                        t.provider_snapshot_json, t.resource_snapshot_id, d.state, d.native_ref, t.error
                 FROM turns t LEFT JOIN deliveries d ON d.turn_id = t.id
                 WHERE t.id = ?1",
                [turn_id],
                |row| {
                    Ok(TurnInspection {
                        turn_id: row.get(0)?,
                        event_id: row.get(1)?,
                        audience_id: row.get(2)?,
                        user_id: row.get(3)?,
                        input: row.get(4)?,
                        state: state_column(row, 5)?,
                        output: row.get(6)?,
                        providers: json_column(row, 7)?,
                        resource_snapshot_id: row.get(8)?,
                        delivery_state: optional_state_column(row, 9)?,
                        delivery_native_ref: row.get(10)?,
                        error: row.get(11)?,
                        attachments: Vec::new(),
                    })
                },
            )
            .optional()?;
        let Some(mut inspection) = inspection else {
            return Ok(None);
        };
        inspection.attachments = captured_files(&connection, Some(turn_id))?
            .iter()
            .map(CapturedFile::wire)
            .collect();
        Ok(Some(inspection))
    }

    /// Queued and running turns of one interface in acceptance order, each
    /// with the runtime that owns it. `activity` is left for the caller.
    pub fn active_turns(&self, interface_id: &str) -> Result<Vec<(String, ActiveTurn)>> {
        let connection = self.connect()?;
        let mut statement = connection.prepare(
            "SELECT runtime_id, id, state, target_id, accepted_at FROM turns
             WHERE interface_id = ?1 AND state IN ('queued', 'running')
             ORDER BY acceptance_sequence",
        )?;
        let rows = statement.query_map([interface_id], |row| {
            Ok((
                row.get(0)?,
                ActiveTurn {
                    turn_id: row.get(1)?,
                    state: state_column(row, 2)?,
                    target_id: row.get(3)?,
                    accepted_at: row.get(4)?,
                    activity: None,
                },
            ))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn turn_counts(&self) -> Result<StateCounts> {
        let connection = self.connect()?;
        let mut statement =
            connection.prepare("SELECT state, COUNT(*) FROM turns GROUP BY state")?;
        let rows = statement.query_map([], |row| {
            Ok((state_column::<TurnState>(row, 0)?, row.get::<_, u32>(1)?))
        })?;
        let mut counts = StateCounts::default();
        for row in rows {
            let (state, count) = row?;
            let count = u64::from(count);
            match state {
                TurnState::Queued => counts.pending += count,
                TurnState::Running => counts.active += count,
                TurnState::Completed => counts.completed += count,
                TurnState::Interrupted => counts.interrupted += count,
            }
        }
        Ok(counts)
    }

    pub fn delivery_counts(&self) -> Result<StateCounts> {
        let connection = self.connect()?;
        let mut statement =
            connection.prepare("SELECT state, COUNT(*) FROM deliveries GROUP BY state")?;
        let rows = statement.query_map([], |row| {
            Ok((
                state_column::<DeliveryState>(row, 0)?,
                row.get::<_, u32>(1)?,
            ))
        })?;
        let mut counts = StateCounts::default();
        for row in rows {
            let (state, count) = row?;
            let count = u64::from(count);
            match state {
                DeliveryState::Pending => counts.pending += count,
                DeliveryState::Delivering => counts.active += count,
                DeliveryState::Delivered => counts.completed += count,
                DeliveryState::Uncertain => counts.uncertain += count,
            }
        }
        Ok(counts)
    }

    pub fn recent_turns(&self, event_id: Option<&str>, limit: u32) -> Result<Vec<TurnSummary>> {
        let connection = self.connect()?;
        let mut statement = connection.prepare(
            "SELECT t.id, t.event_id, t.state, t.accepted_at, d.state
             FROM turns t LEFT JOIN deliveries d ON d.turn_id = t.id
             WHERE (?1 IS NULL OR t.event_id = ?1)
             ORDER BY t.acceptance_sequence DESC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![event_id, limit.clamp(1, 100)], |row| {
            Ok(TurnSummary {
                turn_id: row.get(0)?,
                event_id: row.get(1)?,
                state: state_column(row, 2)?,
                accepted_at: row.get(3)?,
                delivery_state: optional_state_column(row, 4)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}

/// Every read-then-write sequence starts an immediate transaction so writers
/// serialize up front under the busy timeout instead of failing later on a
/// deferred read-to-write upgrade.
fn write_transaction(connection: &mut Connection) -> Result<Transaction<'_>> {
    Ok(connection.transaction_with_behavior(TransactionBehavior::Immediate)?)
}

/// Queues the delivery of a finished turn. A turn leaving `running` never has
/// a delivery row yet, so a conflict here is a real invariant violation.
fn enqueue_delivery(
    transaction: &Transaction<'_>,
    turn: &ClaimedTurn,
    content: &str,
    now: i64,
) -> Result<()> {
    transaction.execute(
        "INSERT INTO deliveries(turn_id, interface_id, target_id, feedback_target_id,
             content, state, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6)",
        params![
            turn.turn_id,
            turn.interface_id,
            turn.target_id,
            turn.feedback_target_id,
            content,
            now,
        ],
    )?;
    Ok(())
}

fn json_column<T: DeserializeOwned>(row: &Row<'_>, index: usize) -> rusqlite::Result<T> {
    parse_json(index, &row.get::<_, String>(index)?)
}

fn optional_json_column<T: DeserializeOwned>(
    row: &Row<'_>,
    index: usize,
) -> rusqlite::Result<Option<T>> {
    row.get::<_, Option<String>>(index)?
        .map(|text| parse_json(index, &text))
        .transpose()
}

fn parse_json<T: DeserializeOwned>(index: usize, text: &str) -> rusqlite::Result<T> {
    serde_json::from_str(text).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(index, Type::Text, Box::new(error))
    })
}

fn conversation_runtime(
    connection: &Connection,
    interface_id: &str,
    agent_id: &str,
    audience_id: &str,
) -> Result<Option<String>> {
    Ok(connection
        .query_row(
            "SELECT runtime_id FROM turns
         WHERE interface_id = ?1 AND agent_id = ?2 AND audience_id = ?3
         ORDER BY acceptance_sequence LIMIT 1",
            params![interface_id, agent_id, audience_id],
            |row| row.get(0),
        )
        .optional()?)
}

/// A turn's captured files in message order, or every committed reference.
fn captured_files(connection: &Connection, turn_id: Option<&str>) -> Result<Vec<CapturedFile>> {
    let mut statement = connection.prepare(
        "SELECT capture_id, ordinal, name, size, content_type, sha256 FROM turn_attachments
         WHERE ?1 IS NULL OR turn_id = ?1 ORDER BY turn_id, ordinal",
    )?;
    let files = statement.query_map([turn_id], |row| {
        Ok(CapturedFile {
            capture: row.get(0)?,
            ordinal: row.get(1)?,
            name: row.get(2)?,
            size: u64::try_from(row.get::<_, i64>(3)?).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(3, Type::Integer, Box::new(error))
            })?,
            content_type: row.get(4)?,
            sha256: row.get(5)?,
        })
    })?;
    files.collect::<Result<_, _>>().map_err(Into::into)
}

fn event_receipt(
    connection: &Connection,
    interface_id: &str,
    event_id: &str,
) -> Result<Option<EventReceipt>> {
    Ok(connection
        .query_row(
            "SELECT id, state FROM turns WHERE interface_id = ?1 AND event_id = ?2",
            params![interface_id, event_id],
            |row| {
                Ok(EventReceipt {
                    event_id: event_id.to_owned(),
                    turn_id: row.get(0)?,
                    state: state_column(row, 1)?,
                    duplicate: true,
                })
            },
        )
        .optional()?)
}

fn timestamp() -> Result<i64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?;
    i64::try_from(duration.as_secs()).context("timestamp exceeds SQLite integer range")
}

#[cfg(test)]
mod tests {
    use anyhow::{Context, Result};
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt as _;
    fn tempdir() -> Result<tempfile::TempDir> {
        let directory = tempfile::tempdir()?;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
        Ok(directory)
    }

    use super::{Database, Lane};
    use carillon_protocol::model::{
        DeliveryOutcome, DeliveryState, EventReceipt, ExternalEvent, RuntimeTurnResponse, TurnState,
    };

    fn event(id: &str, audience: &str) -> ExternalEvent {
        ExternalEvent {
            event_id: id.to_owned(),
            agent_id: "agent".to_owned(),
            user_id: "user".to_owned(),
            audience_id: audience.to_owned(),
            delivery_target_id: "channel".to_owned(),
            feedback_target_id: "message".to_owned(),
            content: "hello".to_owned(),
            native: json!({}),
            attachments: None,
        }
    }

    fn accept(database: &Database, id: &str, audience: &str) -> Result<EventReceipt> {
        database.accept_event(
            "discord",
            "runtime",
            &event(id, audience),
            &json!([]),
            "resources",
            None,
        )
    }

    #[test]
    fn interruption_notice_does_not_block_or_replay_work() -> Result<()> {
        let directory = tempdir()?;
        let database = Database::create(&directory.path().join("state.sqlite3"))?;
        accept(&database, "one", "conversation")?;
        let next = accept(&database, "two", "conversation")?;
        let turn = database
            .claim("runtime", Lane::Messages)?
            .context("missing turn")?;
        database.interrupt_turn(&turn, "runtime disconnected", None)?;
        let claim = database
            .claim_delivery("discord", "live")?
            .context("missing notice")?;
        assert_eq!(claim.turn_state, TurnState::Interrupted);
        assert_eq!(
            database
                .claim("runtime", Lane::Messages)?
                .context("next message blocked")?
                .turn_id,
            next.turn_id
        );
        assert!(database.claim("runtime", Lane::Messages)?.is_none());
        assert!(database.acknowledge_delivery(
            "discord",
            claim.id,
            "live",
            DeliveryOutcome::Delivered,
            Some("notice")
        )?);
        assert_eq!(
            database
                .inspect_turn(&turn.turn_id)?
                .context("missing turn")?
                .state,
            TurnState::Interrupted
        );
        assert!(accept(&database, "one", "conversation")?.duplicate);
        Ok(())
    }

    #[test]
    fn open_refuses_missing_empty_and_obsolete_databases_without_repair() -> Result<()> {
        let directory = tempdir()?;
        let path = directory.path().join("state.sqlite3");
        for contents in [None, Some(Vec::new()), Some(b"not SQLite".to_vec())] {
            if let Some(bytes) = &contents {
                std::fs::write(&path, bytes)?;
            }
            assert!(Database::open(&path).is_err());
            assert_eq!(std::fs::read(&path).ok(), contents);
        }
        std::fs::remove_file(&path)?;
        rusqlite::Connection::open(&path)?.execute_batch("CREATE TABLE obsolete (id INTEGER)")?;
        let before = std::fs::read(&path)?;
        assert!(Database::open(&path).is_err());
        assert_eq!(std::fs::read(&path)?, before);
        std::fs::remove_file(&path)?;
        Database::create(&path)?;
        Database::open(&path)?;
        Ok(())
    }

    #[test]
    fn established_database_handles_do_not_recreate_a_lost_file() -> Result<()> {
        let directory = tempdir()?;
        let path = directory.path().join("state.sqlite3");
        let database = Database::create(&path)?;
        accept(&database, "one", "conversation")?;
        std::fs::remove_file(&path)?;
        assert!(database.turn_counts().is_err());
        assert!(!path.exists());
        Ok(())
    }

    #[test]
    fn acceptance_pins_conversations_and_rejects_owner_changes() -> Result<()> {
        let directory = tempdir()?;
        let database = Database::create(&directory.path().join("state.sqlite3"))?;
        assert_eq!(
            database.conversation_runtime("discord", "agent", "conversation")?,
            None
        );
        accept(&database, "one", "conversation")?;
        assert_eq!(
            database
                .conversation_runtime("discord", "agent", "conversation")?
                .as_deref(),
            Some("runtime")
        );
        assert_eq!(
            database.conversation_runtime("other-interface", "agent", "conversation")?,
            None
        );
        assert_eq!(
            database.conversation_runtime("discord", "other-agent", "conversation")?,
            None
        );
        let second = event("two", "conversation");
        assert!(
            database
                .accept_event(
                    "discord",
                    "replacement",
                    &second,
                    &json!([]),
                    "resources",
                    None
                )
                .is_err()
        );
        assert!(database.event_receipt("discord", "two")?.is_none());
        assert_eq!(database.recent_turns(None, 10)?.len(), 1);
        // A stale runtime selection must not prevent recovery of an existing receipt.
        assert!(
            database
                .accept_event(
                    "discord",
                    "replacement",
                    &event("one", "conversation"),
                    &json!([]),
                    "resources",
                    None
                )?
                .duplicate
        );
        accept(&database, "two", "conversation")?;
        assert_eq!(database.recent_turns(None, 10)?.len(), 2);
        Ok(())
    }

    #[test]
    fn event_is_idempotent_and_completion_is_atomic() -> Result<()> {
        let directory = tempdir()?;
        let database = Database::create(&directory.path().join("state.sqlite3"))?;
        let first = accept(&database, "one", "conversation")?;
        let duplicate = accept(&database, "one", "conversation")?;
        assert!(!first.duplicate);
        assert!(duplicate.duplicate);
        assert_eq!(first.turn_id, duplicate.turn_id);
        assert_eq!(first.state, TurnState::Queued);

        let turn = database
            .claim("runtime", Lane::Messages)?
            .context("turn was not queued")?;
        database.complete_turn(
            &turn,
            &RuntimeTurnResponse {
                correlation_id: turn.turn_id.clone(),
                output: "reply".to_owned(),
                follow_up: None,
                unanswered_left: false,
            },
        )?;
        assert_eq!(
            database
                .claim_delivery("discord", "instance")?
                .context("delivery missing")?
                .content,
            "reply"
        );
        Ok(())
    }

    #[test]
    fn a_follow_up_cursor_queues_an_observation_beside_later_messages() -> Result<()> {
        let directory = tempdir()?;
        let database = Database::create(&directory.path().join("state.sqlite3"))?;
        let (first, cursor) = delegated(&database)?;
        // The report is an observation: executable claims never take it.
        assert!(database.claim("runtime", Lane::Messages)?.is_none());
        let report = database
            .claim("runtime", Lane::Observations)?
            .context("observation missing")?;
        assert_eq!(report.input, "");
        assert_eq!(report.follow_up, Some(cursor));
        assert_eq!(report.native, json!({}));
        assert_eq!(
            (
                report.audience_id.as_str(),
                report.target_id.as_str(),
                report.feedback_target_id.as_str()
            ),
            (
                first.audience_id.as_str(),
                first.target_id.as_str(),
                first.feedback_target_id.as_str()
            )
        );
        assert_eq!(
            database
                .event_receipt("discord", &format!("follow-up:{}", first.turn_id))?
                .context("follow-up receipt missing")?
                .turn_id,
            report.turn_id
        );
        // While it observes, the conversation's next messages run, in order.
        accept(&database, "two", "conversation")?;
        accept(&database, "three", "conversation")?;
        let second = database
            .claim("runtime", Lane::Messages)?
            .context("second turn missing")?;
        assert!(database.claim("runtime", Lane::Messages)?.is_none());
        complete(&database, &second, "second reply")?;
        assert!(database.claim("runtime", Lane::Messages)?.is_some());
        assert!(database.claim("runtime", Lane::Observations)?.is_none());
        Ok(())
    }

    #[test]
    fn a_restart_asks_an_observation_again_and_delivers_its_report_once() -> Result<()> {
        let directory = tempdir()?;
        let database = Database::create(&directory.path().join("state.sqlite3"))?;
        delegated(&database)?;
        let report = database
            .claim("runtime", Lane::Observations)?
            .context("observation missing")?;
        accept(&database, "two", "conversation")?;
        database
            .claim("runtime", Lane::Messages)?
            .context("second turn missing")?;
        // A restart returns the running observation to its queue; the
        // executable turn that was running is interrupted, as ever.
        assert_eq!(database.recover_running_turns()?, 1);
        let resumed = database
            .claim("runtime", Lane::Observations)?
            .context("observation not resumed")?;
        assert_eq!(resumed.turn_id, report.turn_id);
        complete(&database, &resumed, "the report")?;
        let mut contents = Vec::new();
        while let Some(delivery) = database.claim_delivery("discord", "instance")? {
            database.acknowledge_delivery(
                "discord",
                delivery.id,
                "instance",
                DeliveryOutcome::Delivered,
                None,
            )?;
            contents.push(delivery.content);
        }
        // The parent answer was delivered once already; now the restart's
        // notice for the executable turn only, and the report once.
        assert_eq!(contents.len(), 2);
        assert_eq!(
            contents
                .iter()
                .filter(|content| *content == "the report")
                .count(),
            1
        );
        assert!(
            contents
                .iter()
                .any(|content| content.starts_with("Carillon restarted"))
        );
        assert!(database.claim("runtime", Lane::Observations)?.is_none());
        Ok(())
    }

    /// A first turn answered with a follow-up cursor, its answer delivered.
    fn delegated(database: &Database) -> Result<(super::ClaimedTurn, serde_json::Value)> {
        accept(database, "one", "conversation")?;
        let first = database
            .claim("runtime", Lane::Messages)?
            .context("first turn missing")?;
        let cursor = json!({"message": "native-message", "after": 1});
        database.complete_turn(
            &first,
            &RuntimeTurnResponse {
                correlation_id: first.turn_id.clone(),
                output: "delegated".to_owned(),
                follow_up: Some(cursor.clone()),
                unanswered_left: false,
            },
        )?;
        let answer = database
            .claim_delivery("discord", "instance")?
            .context("delivery missing")?;
        assert_eq!(answer.content, "delegated");
        database.acknowledge_delivery(
            "discord",
            answer.id,
            "instance",
            DeliveryOutcome::Delivered,
            None,
        )?;
        Ok((first, cursor))
    }

    fn complete(database: &Database, turn: &super::ClaimedTurn, output: &str) -> Result<()> {
        database.complete_turn(
            turn,
            &RuntimeTurnResponse {
                correlation_id: turn.turn_id.clone(),
                output: output.to_owned(),
                follow_up: None,
                unanswered_left: false,
            },
        )
    }

    #[test]
    fn completion_failure_rolls_back_output_and_delivery() -> Result<()> {
        let directory = tempdir()?;
        let database = Database::create(&directory.path().join("state.sqlite3"))?;
        accept(&database, "one", "conversation")?;
        let first = database
            .claim("runtime", Lane::Messages)?
            .context("first turn missing")?;
        database.complete_turn(
            &first,
            &RuntimeTurnResponse {
                correlation_id: first.turn_id.clone(),
                output: "first reply".to_owned(),
                follow_up: None,
                unanswered_left: false,
            },
        )?;
        accept(&database, "two", "conversation")?;
        let second = database
            .claim("runtime", Lane::Messages)?
            .context("second turn missing")?;
        // Delivery insertion follows the turn update. Fail at that last write,
        // using real SQLite transaction semantics.
        database.connect()?.execute_batch(
            "CREATE TRIGGER fail_delivery BEFORE INSERT ON deliveries
             BEGIN SELECT RAISE(ABORT, 'injected delivery failure'); END;",
        )?;
        let response = RuntimeTurnResponse {
            correlation_id: second.turn_id.clone(),
            output: "second reply".to_owned(),
            follow_up: None,
            unanswered_left: false,
        };
        let failure = database
            .complete_turn(&second, &response)
            .err()
            .context("injected failure did not fire")?;
        assert!(failure.to_string().contains("injected delivery failure"));
        let inspection = database
            .inspect_turn(&second.turn_id)?
            .context("second turn disappeared")?;
        assert_eq!(inspection.state, TurnState::Running);
        assert_eq!(inspection.output, None);
        assert_eq!(
            database.connect()?.query_row(
                "SELECT COUNT(*) FROM deliveries WHERE turn_id = ?1",
                [&second.turn_id],
                |row| row.get::<_, u32>(0),
            )?,
            0
        );
        database
            .connect()?
            .execute_batch("DROP TRIGGER fail_delivery;")?;
        database.complete_turn(&second, &response)?;
        assert_eq!(
            database
                .inspect_turn(&second.turn_id)?
                .context("missing turn")?
                .state,
            TurnState::Completed
        );
        Ok(())
    }

    #[test]
    fn acceptance_order_survives_clock_rollback_and_reopening() -> Result<()> {
        // Force timestamps and opaque identifiers to disagree with insertion order.
        // No assumption about a UUID generator's in-process monotonicity is needed.
        for second_timestamp in [100, 99] {
            let directory = tempdir()?;
            let path = directory.path().join("state.sqlite3");
            let database = Database::create(&path)?;
            accept(&database, "one", "conversation")?;
            accept(&database, "two", "conversation")?;
            database.connect()?.execute(
                "UPDATE turns SET id = 'z-first', accepted_at = 100 WHERE event_id = 'one'",
                [],
            )?;
            database.connect()?.execute(
                "UPDATE turns SET id = 'a-second', accepted_at = ?1 WHERE event_id = 'two'",
                [second_timestamp],
            )?;
            drop(database);
            let database = Database::open(&path)?;
            let recent = database.recent_turns(None, 10)?;
            assert_eq!(recent[0].event_id, "two");
            assert_eq!(recent[1].event_id, "one");
            let first = database
                .claim("runtime", Lane::Messages)?
                .context("first missing")?;
            assert_eq!(first.turn_id, "z-first");
            assert!(database.claim("runtime", Lane::Messages)?.is_none());
            database.interrupt_turn(&first, "interrupted", None)?;
            let second = database
                .claim("runtime", Lane::Messages)?
                .context("second missing")?;
            assert_eq!(second.turn_id, "a-second");
        }
        Ok(())
    }

    #[test]
    fn restart_interrupts_running_turn_without_replaying_it() -> Result<()> {
        let directory = tempdir()?;
        let database = Database::create(&directory.path().join("state.sqlite3"))?;
        accept(&database, "one", "conversation")?;
        database
            .claim("runtime", Lane::Messages)?
            .context("missing turn")?;
        assert_eq!(database.recover_running_turns()?, 1);
        assert_eq!(database.recover_running_turns()?, 0);
        assert!(database.claim("runtime", Lane::Messages)?.is_none());
        assert_eq!(database.turn_counts()?.interrupted, 1);
        let next = accept(&database, "two", "conversation")?;
        assert_eq!(
            database
                .claim("runtime", Lane::Messages)?
                .context("blocked")?
                .turn_id,
            next.turn_id
        );
        Ok(())
    }

    #[test]
    fn delivery_claim_and_ack_retries_are_idempotent() -> Result<()> {
        let directory = tempdir()?;
        let database = Database::create(&directory.path().join("state.sqlite3"))?;
        accept(&database, "one", "first")?;
        accept(&database, "two", "second")?;

        for output in ["first reply", "second reply"] {
            let turn = database
                .claim("runtime", Lane::Messages)?
                .context("turn was not queued")?;
            database.complete_turn(
                &turn,
                &RuntimeTurnResponse {
                    correlation_id: turn.turn_id.clone(),
                    output: output.to_owned(),
                    follow_up: None,
                    unanswered_left: false,
                },
            )?;
        }

        let first = database
            .claim_delivery("discord", "instance")?
            .context("first delivery missing")?;
        let retried = database
            .claim_delivery("discord", "instance")?
            .context("retried delivery missing")?;
        assert_eq!(retried.id, first.id);

        assert!(database.acknowledge_delivery(
            "discord",
            first.id,
            "instance",
            DeliveryOutcome::Delivered,
            Some("message"),
        )?);
        assert!(database.acknowledge_delivery(
            "discord",
            first.id,
            "instance",
            DeliveryOutcome::Delivered,
            Some("message"),
        )?);
        assert!(!database.acknowledge_delivery(
            "discord",
            first.id,
            "instance",
            DeliveryOutcome::Uncertain,
            Some("message"),
        )?);
        assert!(!database.acknowledge_delivery(
            "other-interface",
            first.id,
            "instance",
            DeliveryOutcome::Delivered,
            Some("message"),
        )?);

        let second = database
            .claim_delivery("discord", "instance")?
            .context("second delivery missing")?;
        assert_ne!(second.id, first.id);
        Ok(())
    }

    #[test]
    fn outcome_transitions_leave_running_exactly_once() -> Result<()> {
        let directory = tempdir()?;
        let database = Database::create(&directory.path().join("state.sqlite3"))?;

        accept(&database, "uncertain", "one")?;
        let uncertain = database
            .claim("runtime", Lane::Messages)?
            .context("uncertain turn was not queued")?;
        database.interrupt_turn(&uncertain, "ambiguous", None)?;
        assert!(
            database
                .interrupt_turn(&uncertain, "ambiguous", None)
                .is_err()
        );
        assert!(database.requeue_turn(&uncertain).is_err());

        accept(&database, "requeued", "two")?;
        let requeued = database
            .claim("runtime", Lane::Messages)?
            .context("requeued turn was not queued")?;
        database.requeue_turn(&requeued)?;
        assert!(database.requeue_turn(&requeued).is_err());
        assert!(
            database
                .interrupt_turn(&requeued, "ambiguous", None)
                .is_err()
        );

        let requeued = database
            .claim("runtime", Lane::Messages)?
            .context("requeued turn could not be claimed again")?;
        let response = RuntimeTurnResponse {
            correlation_id: requeued.turn_id.clone(),
            output: "done".to_owned(),
            follow_up: None,
            unanswered_left: false,
        };
        database.complete_turn(&requeued, &response)?;
        assert!(database.complete_turn(&requeued, &response).is_err());
        assert!(database.requeue_turn(&requeued).is_err());
        assert!(
            database
                .interrupt_turn(&requeued, "ambiguous", None)
                .is_err()
        );
        let inspection = database
            .inspect_turn(&requeued.turn_id)?
            .context("completed turn disappeared")?;
        assert_eq!(inspection.state, TurnState::Completed);
        assert_eq!(inspection.delivery_state, Some(DeliveryState::Pending));
        assert_eq!(database.turn_counts()?.interrupted, 1);
        assert_eq!(database.delivery_counts()?.pending, 2);
        Ok(())
    }
}
