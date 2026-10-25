//! Durable operation state. This module owns every turn and delivery state
//! transition; nothing else writes lifecycle state.
//!
//! ```text
//! turn:      queued -> running -> completed
//!              |         \-> interrupted
//!              \--------------> completed | interrupted (settled before its hand-over was recorded)
//!                   running -> queued (handed over again after a restart)
//!
//! delivery:  pending -> delivering -> delivered
//!               ^                 \-> uncertain (ack lost or claim abandoned)
//!               \--------------------- claim of a silence abandoned
//! ```
//!
//! Invariants:
//!
//! - One accepted event is one turn row, keyed by (interface, event); the row
//!   carries what the event said and what happened to it.
//! - A runtime is handed its queued turns in acceptance order; it orders
//!   each conversation's turns itself. Session continuity belongs to the
//!   runtime.
//! - A turn is a job of its own: interfaces read jobs by the turn's ID.
//! - A turn's final report is stored once, with the outcome it describes.
//! - Each terminal turn has one delivery row; its outcome and delivery intent
//!   commit in one transaction, including a turn interrupted during admission.
//! - Transitions are guarded conditional updates that succeed exactly once, so
//!   a stale caller fails instead of overwriting newer state.
//! - A turn's attachment references commit with the turn, and only after the
//!   capture they name is published and synced; a losing duplicate's capture
//!   is never referenced.

mod admission;
mod dispatch;
mod jobs;
mod opening;
mod recovery;
mod settle;

pub(crate) use admission::Admission;
pub(crate) use dispatch::QueuedTurn;
pub(crate) use jobs::JobPage;
pub(crate) use opening::{Opened, RUNTIME_EVENT_PREFIX};
pub use settle::Settled;
pub(crate) use settle::Settlement;

use std::{
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use carillon_protocol::model::{
    Delivery, DeliveryOutcome, DeliveryState, EventReceipt, ExternalEvent, Origin, StateCounts,
    TurnActivity, TurnInspection, TurnState, TurnSummary,
};
use rusqlite::{
    Connection, OptionalExtension, Row, Transaction, TransactionBehavior, params,
    types::{FromSql, FromSqlError, FromSqlResult, Type, ValueRef},
};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::attachments::{Capture, CapturedFile};

#[derive(Clone)]
pub(crate) struct Database {
    path: PathBuf,
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
    pub(crate) fn create(path: &Path) -> Result<Self> {
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

    pub(crate) fn open(path: &Path) -> Result<Self> {
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
    pub(crate) fn validate_backup(path: &Path) -> Result<()> {
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
    pub(crate) fn validate_resource_snapshots(&self, store: &Path) -> Result<()> {
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
    pub(crate) fn captured_files(&self) -> Result<Vec<CapturedFile>> {
        let connection =
            Connection::open_with_flags(&self.path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        captured_files(&connection, None)
    }

    /// The captures committed turns reference.
    pub(crate) fn captures(&self) -> Result<std::collections::BTreeSet<String>> {
        let connection = self.connect()?;
        let mut statement =
            connection.prepare("SELECT DISTINCT capture_id FROM turn_attachments")?;
        let captures = statement
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        Ok(captures)
    }

    pub(crate) fn capture_referenced(&self, capture: &str) -> Result<bool> {
        Ok(self.connect()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM turn_attachments WHERE capture_id = ?1)",
            [capture],
            |row| row.get(0),
        )?)
    }

    /// Bytes kept by every committed capture.
    pub(crate) fn captured_bytes(&self) -> Result<u64> {
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
                runtime_id TEXT NOT NULL,
                provider_snapshot_json TEXT NOT NULL,
                resource_snapshot_id TEXT NOT NULL,
                state TEXT NOT NULL,
                output TEXT,
                error TEXT,
                activity_json TEXT,
                accepted_at INTEGER NOT NULL,
                finished_at INTEGER,
                UNIQUE(interface_id, event_id)
            );
            CREATE INDEX IF NOT EXISTS turns_conversation
                ON turns(interface_id, agent_id, audience_id, acceptance_sequence);
            CREATE INDEX IF NOT EXISTS turns_queued
                ON turns(runtime_id, acceptance_sequence) WHERE state = 'queued';
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
                silence TEXT,
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
                PRIMARY KEY(turn_id, ordinal)
            );
            CREATE INDEX IF NOT EXISTS turn_attachments_capture
                ON turn_attachments(capture_id);",
        )?;
        Ok(())
    }

    /// Reads an accepted event without depending on current runtime resources.
    pub(crate) fn event_receipt(
        &self,
        interface_id: &str,
        event_id: &str,
    ) -> Result<Option<EventReceipt>> {
        event_receipt(&self.connect()?, interface_id, event_id)
    }

    /// The runtime that took the conversation's latest turn owns it, before any native
    /// session exists; only a stranded conversation's notice moves it to the agent's runtime.
    pub(crate) fn conversation_runtime(
        &self,
        interface_id: &str,
        agent_id: &str,
        audience_id: &str,
    ) -> Result<Option<String>> {
        conversation_runtime(&self.connect()?, interface_id, agent_id, audience_id)
    }

    /// Releases delivery claims held by other, presumed dead, instances of an
    /// interface as uncertain, or as pending for a silence, which sends no
    /// message, and returns the feedback targets of every uncertain delivery.
    pub(crate) fn reconcile_delivery_claims(
        &self,
        interface_id: &str,
        owner: &str,
    ) -> Result<Vec<String>> {
        let mut connection = self.connect()?;
        let transaction = write_transaction(&mut connection)?;
        transaction.execute(
            "UPDATE deliveries SET claim_owner = NULL, updated_at = ?3,
                 state = CASE WHEN silence IS NULL THEN 'uncertain' ELSE 'pending' END
             WHERE interface_id = ?1 AND state = 'delivering'
               AND (claim_owner IS NULL OR claim_owner <> ?2)",
            params![interface_id, owner, timestamp()?],
        )?;
        let targets = transaction
            .prepare(
                "SELECT DISTINCT feedback_target_id FROM deliveries
                 WHERE interface_id = ?1 AND state = 'uncertain' AND feedback_target_id <> ''",
            )?
            .query_map([interface_id], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        transaction.commit()?;
        Ok(targets)
    }

    /// Claims the oldest pending delivery for an interface instance, with its
    /// turn's outcome and origin.
    pub(crate) fn claim_delivery(
        &self,
        interface_id: &str,
        owner: &str,
    ) -> Result<Option<Delivery>> {
        self.claim_next_delivery(interface_id, owner, None)
    }

    /// Claims the pending delivery of one turn for an interface instance.
    pub(crate) fn claim_turn_delivery(
        &self,
        interface_id: &str,
        owner: &str,
        turn_id: &str,
    ) -> Result<Option<Delivery>> {
        self.claim_next_delivery(interface_id, owner, Some(turn_id))
    }

    /// A delivery the instance already holds comes first, then the oldest
    /// pending one, of `turn_id` when given.
    fn claim_next_delivery(
        &self,
        interface_id: &str,
        owner: &str,
        turn_id: Option<&str>,
    ) -> Result<Option<Delivery>> {
        let mut connection = self.connect()?;
        let transaction = write_transaction(&mut connection)?;
        let delivery = transaction
            .query_row(
                "SELECT d.id, d.turn_id, d.target_id, d.feedback_target_id, d.content, t.state,
                        t.id, t.accepted_at, d.silence
                 FROM deliveries d
                 JOIN turns t ON t.id = d.turn_id
                 WHERE d.interface_id = ?1 AND d.state IN ('pending', 'delivering')
                   AND (d.state = 'pending' OR d.claim_owner = ?2)
                   AND (?3 IS NULL OR d.turn_id = ?3)
                 ORDER BY CASE d.state WHEN 'delivering' THEN 0 ELSE 1 END, d.id LIMIT 1",
                params![interface_id, owner, turn_id],
                |row| {
                    Ok(Delivery {
                        id: row.get(0)?,
                        turn_id: row.get(1)?,
                        target_id: row.get(2)?,
                        feedback_target_id: row.get(3)?,
                        content: row.get(4)?,
                        turn_state: state_column::<TurnState>(row, 5)?,
                        origin: Origin {
                            turn_id: row.get(6)?,
                            accepted_at: row.get(7)?,
                        },
                        silence: optional_state_column(row, 8)?,
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
    pub(crate) fn acknowledge_delivery(
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

    pub(crate) fn inspect_turn(&self, turn_id: &str) -> Result<Option<TurnInspection>> {
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

    pub(crate) fn turn_counts(&self) -> Result<StateCounts> {
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

    pub(crate) fn delivery_counts(&self) -> Result<StateCounts> {
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

    /// Deliveries of `interfaces` their interface has not yet settled:
    /// waiting to be claimed, or claimed and not acknowledged.
    pub(crate) fn undelivered(&self, interfaces: &[&str]) -> Result<u64> {
        let connection = self.connect()?;
        let mut statement = connection.prepare(
            "SELECT interface_id, COUNT(*) FROM deliveries
             WHERE state IN ('pending', 'delivering') GROUP BY interface_id",
        )?;
        let mut undelivered = 0;
        for row in statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?))
        })? {
            let (interface, count) = row?;
            if interfaces.contains(&interface.as_str()) {
                undelivered += u64::from(count);
            }
        }
        Ok(undelivered)
    }

    pub(crate) fn recent_turns(
        &self,
        event_id: Option<&str>,
        limit: u32,
    ) -> Result<Vec<TurnSummary>> {
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

/// A turn's final report as stored. The engine keeps it within
/// [`carillon_protocol::model::ACTIVITY_REPORT_LIMIT`].
fn report_json(activity: Option<&TurnActivity>) -> Result<Option<String>> {
    Ok(activity.map(serde_json::to_string).transpose()?)
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
         ORDER BY acceptance_sequence DESC LIMIT 1",
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
    use anyhow::{Context, Result, bail};
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt as _;
    fn tempdir() -> Result<tempfile::TempDir> {
        let directory = tempfile::tempdir()?;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
        Ok(directory)
    }

    use super::{Admission, Database, QueuedTurn, Settled, Settlement};
    use carillon_protocol::model::{
        DeliveryOutcome, DeliveryState, EventReceipt, ExternalEvent, JOB_READ_LIMIT, Job,
        TurnActivity, TurnState,
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
            Admission::Queued {
                providers: &json!([]),
                resource_snapshot_id: "resources",
            },
            None,
        )
    }

    /// `runtime`'s oldest queued turn, which it now holds.
    fn hold(database: &Database, runtime: &str) -> Result<QueuedTurn> {
        let turn = database.next_dispatch(runtime)?.context("nothing queued")?;
        database.dispatched(&turn)?;
        Ok(turn)
    }

    fn answer(database: &Database, turn_id: &str, output: &str) -> Result<()> {
        let settled = database.settle_held(
            "runtime",
            turn_id,
            Settlement::Answered {
                output,
                activity: None,
            },
        )?;
        assert_eq!(settled, Settled::Recorded);
        Ok(())
    }

    #[test]
    fn open_refuses_missing_empty_and_obsolete_databases_without_repair() -> Result<()> {
        let directory = tempdir()?;
        let path = directory.path().join("state.sqlite3");
        for (contents, reason) in [
            (None, "unable to open database file"),
            (Some(Vec::new()), "not the current Carillon format"),
            (Some(b"not SQLite".to_vec()), "file is not a database"),
        ] {
            if let Some(bytes) = &contents {
                std::fs::write(&path, bytes)?;
            }
            crate::assert_refused(Database::open(&path), reason)?;
            assert_eq!(std::fs::read(&path).ok(), contents);
        }
        std::fs::remove_file(&path)?;
        rusqlite::Connection::open(&path)?.execute_batch("CREATE TABLE obsolete (id INTEGER)")?;
        let before = std::fs::read(&path)?;
        crate::assert_refused(Database::open(&path), "not the current Carillon format")?;
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
        crate::assert_refused(database.turn_counts(), "unable to open database file")?;
        assert!(!path.exists());
        Ok(())
    }

    mod ownership;

    #[test]
    fn a_repeated_event_returns_its_first_queued_turn() -> Result<()> {
        let directory = tempdir()?;
        let database = Database::create(&directory.path().join("state.sqlite3"))?;
        let first = accept(&database, "one", "conversation")?;
        let duplicate = accept(&database, "one", "conversation")?;
        assert!(!first.duplicate);
        assert!(duplicate.duplicate);
        assert_eq!(first.turn_id, duplicate.turn_id);
        assert_eq!(first.state, TurnState::Queued);
        Ok(())
    }

    #[test]
    fn a_restart_hands_held_turns_over_again_keeps_opened_ones_and_interrupts_the_unconfigured()
    -> Result<()> {
        let directory = tempdir()?;
        let database = Database::create(&directory.path().join("state.sqlite3"))?;
        accept(&database, "held", "conversation")?;
        let held = hold(&database, "runtime")?;
        let queued = accept(&database, "queued", "conversation")?;
        database.accept_event(
            "discord",
            "removed",
            &event("gone", "elsewhere"),
            Admission::Queued {
                providers: &json!([]),
                resource_snapshot_id: "resources",
            },
            None,
        )?;
        let gone = hold(&database, "removed")?;
        let opening = carillon_protocol::turns::TurnOpening {
            interface_id: "discord".to_owned(),
            agent_id: "agent".to_owned(),
            audience_id: "conversation".to_owned(),
            key: "nightly".to_owned(),
            label: None,
        };
        let super::Opened::Receipt(opened) =
            database.open_turn("runtime", Some("runtime"), &opening)?
        else {
            bail!("no turn opened");
        };
        assert_eq!(database.recover_running_turns(&["runtime"])?, 1);
        let state = |turn: &str| -> Result<TurnState> {
            Ok(database.inspect_turn(turn)?.context("missing turn")?.state)
        };
        // Handed over again, in order: the runtime takes it as the same turn.
        assert_eq!(state(&held.turn_id)?, TurnState::Queued);
        assert_eq!(
            database.next_dispatch("runtime")?.map(|turn| turn.turn_id),
            Some(held.turn_id)
        );
        assert_eq!(state(&queued.turn_id)?, TurnState::Queued);
        assert_eq!(state(&opened.turn_id)?, TurnState::Running);
        // A runtime no longer configured can settle nothing: its conversation is told.
        assert_eq!(state(&gone.turn_id)?, TurnState::Interrupted);
        let notice = database
            .claim_delivery("discord", "instance")?
            .context("notice missing")?;
        assert_eq!(notice.turn_id, gone.turn_id);
        assert!(
            notice
                .content
                .starts_with("Carillon restarted during that turn.")
        );
        Ok(())
    }

    fn report(calls: u32) -> Result<TurnActivity> {
        Ok(serde_json::from_value(json!({
            "phase": "model",
            "running": [],
            "recent": [{"tool": "bash", "detail": "make", "ok": true, "duration_ms": 1200}],
            "tool_calls": calls,
        }))?)
    }

    /// Every job at work, one page after another, as `(runtime, job)`.
    fn at_work(database: &Database, interface: &str) -> Result<Vec<(String, Job)>> {
        let super::JobPage { mut jobs, mut next } = database
            .jobs_at_work(interface, None)?
            .context("first page")?;
        while let Some(after) = next {
            let page = database
                .jobs_at_work(interface, Some(&after))?
                .context("next page")?;
            assert!(page.jobs.len() <= JOB_READ_LIMIT);
            jobs.extend(page.jobs);
            next = page.next;
        }
        Ok(jobs)
    }

    #[test]
    fn an_interrupted_jobs_report_is_kept_with_its_notice() -> Result<()> {
        let directory = tempdir()?;
        let database = Database::create(&directory.path().join("state.sqlite3"))?;
        accept(&database, "two", "conversation")?;
        let failed = hold(&database, "runtime")?;
        let report = report(1)?;
        database.settle_held(
            "runtime",
            &failed.turn_id,
            Settlement::Failed {
                error: "runtime failed",
                notice: "That turn was interrupted: It broke.",
                activity: Some(&report),
            },
        )?;
        let notice = database
            .claim_delivery("discord", "instance")?
            .context("notice missing")?;
        assert_eq!(notice.turn_state, TurnState::Interrupted);
        assert_eq!(notice.origin.turn_id, failed.turn_id);
        let named = database.named_jobs("discord", std::slice::from_ref(&failed.turn_id))?;
        let [(runtime, job)] = named.as_slice() else {
            bail!("expected one job: {named:?}");
        };
        assert_eq!(runtime, "runtime");
        assert_eq!(job.origin.turn_id, failed.turn_id);
        let [turn] = job.turns.as_slice() else {
            bail!("a job is one turn: {job:?}");
        };
        assert_eq!(
            (
                turn.state,
                turn.delivery,
                turn.activity.as_ref().map(|report| report.tool_calls)
            ),
            (
                TurnState::Interrupted,
                Some(DeliveryState::Delivering),
                Some(1)
            )
        );
        Ok(())
    }

    #[test]
    fn job_reads_name_only_the_interfaces_own_jobs_and_page_through_all_at_work() -> Result<()> {
        let directory = tempdir()?;
        let database = Database::create(&directory.path().join("state.sqlite3"))?;
        // Messages of one conversation are jobs of their own.
        let one = accept(&database, "one", "conversation")?;
        let two = accept(&database, "two", "conversation")?;
        let listed = at_work(&database, "discord")?;
        assert_eq!(
            listed
                .iter()
                .map(|(_, job)| job.origin.turn_id.as_str())
                .collect::<Vec<_>>(),
            [one.turn_id.as_str(), two.turn_id.as_str()]
        );
        // Neither another interface's job nor an unknown turn names a job.
        let other = database.accept_event(
            "other",
            "runtime",
            &event("foreign", "elsewhere"),
            Admission::Queued {
                providers: &json!({}),
                resource_snapshot_id: "snapshot",
            },
            None,
        )?;
        let named = [
            other.turn_id.clone(),
            "unknown".to_owned(),
            two.turn_id.clone(),
        ];
        let listed = database.named_jobs("discord", &named)?;
        assert_eq!(
            listed
                .iter()
                .map(|(_, job)| job.origin.turn_id.as_str())
                .collect::<Vec<_>>(),
            [two.turn_id.as_str()]
        );
        assert_eq!(at_work(&database, "other")?.len(), 1);
        assert!(
            database
                .named_jobs("other", std::slice::from_ref(&one.turn_id))?
                .is_empty()
        );
        // A page cursor is one of the interface's own turns.
        assert!(
            database
                .jobs_at_work("discord", Some(&other.turn_id))?
                .is_none()
        );
        // A job that ended is no longer at work.
        let first = hold(&database, "runtime")?;
        assert_eq!(first.turn_id, one.turn_id);
        answer(&database, &one.turn_id, "done")?;
        // Every job at work is read, a bounded page at a time, oldest first.
        let mut accepted = vec![two.turn_id];
        for index in 0..2 * JOB_READ_LIMIT {
            accepted.push(
                accept(
                    &database,
                    &format!("many-{index}"),
                    &format!("many-{index}"),
                )?
                .turn_id,
            );
        }
        let page = database
            .jobs_at_work("discord", None)?
            .context("first page")?;
        assert_eq!(page.jobs.len(), JOB_READ_LIMIT);
        assert!(page.next.is_some());
        let listed = at_work(&database, "discord")?
            .into_iter()
            .map(|(_, job)| job.origin.turn_id)
            .collect::<Vec<_>>();
        assert_eq!(listed, accepted);
        Ok(())
    }

    #[test]
    fn a_database_missing_a_current_index_is_refused_without_repair() -> Result<()> {
        let directory = tempdir()?;
        let path = directory.path().join("state.sqlite3");
        let database = Database::create(&path)?;
        accept(&database, "one", "conversation")?;
        drop(database);
        // An index is part of the format: a database an earlier release wrote
        // without it is refused, and nothing adds it.
        rusqlite::Connection::open(&path)?.execute_batch("DROP INDEX turns_queued")?;
        crate::assert_refused(Database::open(&path), "not the current Carillon format")?;
        let indexes: u32 = rusqlite::Connection::open(&path)?.query_row(
            "SELECT COUNT(*) FROM sqlite_schema WHERE name = 'turns_queued'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(indexes, 0, "a refused database was repaired");
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
            assert_eq!(hold(&database, "runtime")?.turn_id, "z-first");
            assert_eq!(hold(&database, "runtime")?.turn_id, "a-second");
        }
        Ok(())
    }

    #[test]
    fn delivery_claim_and_ack_retries_are_idempotent() -> Result<()> {
        let directory = tempdir()?;
        let database = Database::create(&directory.path().join("state.sqlite3"))?;
        accept(&database, "one", "first")?;
        accept(&database, "two", "second")?;
        for output in ["first reply", "second reply"] {
            let turn = hold(&database, "runtime")?;
            answer(&database, &turn.turn_id, output)?;
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
    fn a_turns_delivery_is_claimed_without_touching_older_ones() -> Result<()> {
        let directory = tempdir()?;
        let database = Database::create(&directory.path().join("state.sqlite3"))?;
        let mut turns = Vec::new();
        for (event, output) in [("older", "older reply"), ("wanted", "wanted reply")] {
            accept(&database, event, output)?;
            let turn = hold(&database, "runtime")?;
            answer(&database, &turn.turn_id, output)?;
            turns.push(turn.turn_id);
        }
        let wanted = database
            .claim_turn_delivery("discord", "instance", &turns[1])?
            .context("wanted delivery missing")?;
        assert_eq!(wanted.turn_id, turns[1]);
        // Held by this instance, it is claimed again until acknowledged.
        let again = database
            .claim_turn_delivery("discord", "instance", &turns[1])?
            .context("held delivery missing")?;
        assert_eq!(again.id, wanted.id);
        database.acknowledge_delivery(
            "discord",
            wanted.id,
            "instance",
            DeliveryOutcome::Delivered,
            Some("message"),
        )?;
        assert!(
            database
                .claim_turn_delivery("discord", "instance", &turns[1])?
                .is_none()
        );
        // The older turn's delivery was never claimed: it is still the next.
        let older = database
            .claim_delivery("discord", "other-instance")?
            .context("older delivery missing")?;
        assert_eq!(older.turn_id, turns[0]);
        Ok(())
    }
}
