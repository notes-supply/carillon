//! Durable operation state. This module owns every turn and delivery state
//! transition; nothing else writes lifecycle state.
//!
//! ```text
//! turn:      queued -> running -> completed
//!                   ^         \-> uncertain -> queued  (operator retry)
//!                   |                      \-> skipped (operator skip)
//!                   \--------- running (runtime provably never reached)
//!
//! delivery:  pending -> delivering -> delivered
//!                                 \-> uncertain (ack lost or claim abandoned)
//! ```
//!
//! Invariants:
//!
//! - Turns in one conversation (interface, agent, audience) run in acceptance
//!   order, and an unfinished earlier turn — queued, running, or uncertain —
//!   blocks later ones. Unrelated conversations proceed independently.
//! - A delivery row exists exactly when its turn left `running`; completion
//!   output, the continuation reference, and the delivery intent commit in one
//!   transaction.
//! - Transitions are guarded conditional updates, so a stale caller fails
//!   instead of overwriting newer state.

use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use rusqlite::{
    Connection, OptionalExtension, Row, Transaction, TransactionBehavior, params,
    types::{FromSql, FromSqlError, FromSqlResult, Type, ValueRef},
};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::model::{
    Delivery, DeliveryOutcome, DeliveryState, EventReceipt, ExternalEvent, ResourceSnapshot,
    RuntimeTurnResponse, StateCounts, TurnInspection, TurnState, TurnSummary,
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
    pub resources: ResourceSnapshot,
}

impl FromSql for TurnState {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        value
            .as_str()?
            .parse()
            .map_err(|error: anyhow::Error| FromSqlError::Other(error.into()))
    }
}

impl FromSql for DeliveryState {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        value
            .as_str()?
            .parse()
            .map_err(|error: anyhow::Error| FromSqlError::Other(error.into()))
    }
}

impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let database = Self {
            path: path.to_path_buf(),
        };
        database.initialize()?;
        Ok(database)
    }

    fn connect(&self) -> Result<Connection> {
        let connection = Connection::open(&self.path)
            .with_context(|| format!("open {}", self.path.display()))?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(connection)
    }

    fn initialize(&self) -> Result<()> {
        self.connect()?.execute_batch(
            "CREATE TABLE IF NOT EXISTS events (
                interface_id TEXT NOT NULL,
                event_id TEXT NOT NULL,
                agent_id TEXT NOT NULL,
                audience_id TEXT NOT NULL,
                user_id TEXT NOT NULL,
                target_id TEXT NOT NULL,
                feedback_target_id TEXT NOT NULL,
                input TEXT NOT NULL,
                native_json TEXT NOT NULL,
                accepted_at INTEGER NOT NULL,
                PRIMARY KEY(interface_id, event_id)
            );
            CREATE TABLE IF NOT EXISTS turns (
                id TEXT PRIMARY KEY,
                interface_id TEXT NOT NULL,
                event_id TEXT NOT NULL,
                agent_id TEXT NOT NULL,
                audience_id TEXT NOT NULL,
                user_id TEXT NOT NULL,
                runtime_id TEXT NOT NULL,
                state TEXT NOT NULL,
                provider_snapshot_json TEXT NOT NULL,
                resource_snapshot_json TEXT NOT NULL,
                output TEXT,
                error TEXT,
                started_at INTEGER NOT NULL,
                finished_at INTEGER,
                UNIQUE(interface_id, event_id),
                FOREIGN KEY(interface_id, event_id) REFERENCES events(interface_id, event_id)
            );
            CREATE TABLE IF NOT EXISTS continuations (
                interface_id TEXT NOT NULL,
                agent_id TEXT NOT NULL,
                audience_id TEXT NOT NULL,
                runtime_id TEXT NOT NULL,
                native_ref TEXT NOT NULL,
                updated_at INTEGER NOT NULL,
                PRIMARY KEY(interface_id, agent_id, audience_id, runtime_id)
            );
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
            );",
        )?;
        Ok(())
    }

    /// Records an admitted event and its queued turn, or returns the receipt
    /// of the turn a previous submission of the same event already created.
    pub fn accept_event(
        &self,
        interface_id: &str,
        runtime_id: &str,
        event: &ExternalEvent,
        providers: &Value,
        resources: &ResourceSnapshot,
    ) -> Result<EventReceipt> {
        let mut connection = self.connect()?;
        let transaction = write_transaction(&mut connection)?;
        if let Some((turn_id, state)) = transaction
            .query_row(
                "SELECT id, state FROM turns WHERE interface_id = ?1 AND event_id = ?2",
                params![interface_id, event.event_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, TurnState>(1)?)),
            )
            .optional()?
        {
            transaction.commit()?;
            return Ok(EventReceipt {
                event_id: event.event_id.clone(),
                turn_id,
                state,
                duplicate: true,
            });
        }

        let turn_id = uuid::Uuid::now_v7().to_string();
        let now = timestamp()?;
        transaction.execute(
            "INSERT INTO events(interface_id, event_id, agent_id, audience_id, user_id, target_id,
                 feedback_target_id, input, native_json, accepted_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                interface_id,
                event.event_id,
                event.agent_id,
                event.audience_id,
                event.user_id,
                event.delivery_target_id,
                event.feedback_target_id,
                event.content,
                serde_json::to_string(&event.native)?,
                now,
            ],
        )?;
        transaction.execute(
            "INSERT INTO turns(id, interface_id, event_id, agent_id, audience_id, user_id,
                 runtime_id, state, provider_snapshot_json, resource_snapshot_json, started_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'queued', ?8, ?9, ?10)",
            params![
                turn_id,
                interface_id,
                event.event_id,
                event.agent_id,
                event.audience_id,
                event.user_id,
                runtime_id,
                serde_json::to_string(providers)?,
                serde_json::to_string(resources)?,
                now,
            ],
        )?;
        transaction.commit()?;
        Ok(EventReceipt {
            event_id: event.event_id.clone(),
            turn_id,
            state: TurnState::Queued,
            duplicate: false,
        })
    }

    /// Claims the oldest eligible queued turn for a runtime and marks it
    /// running. A turn is eligible only when its conversation has no earlier
    /// unfinished turn, which serializes each conversation FIFO and lets an
    /// uncertain turn hold the conversation until an operator resolves it.
    pub fn claim_turn(&self, runtime_id: &str) -> Result<Option<ClaimedTurn>> {
        let mut connection = self.connect()?;
        let transaction = write_transaction(&mut connection)?;
        let turn = transaction
            .query_row(
                "SELECT t.id, t.interface_id, t.agent_id, t.audience_id, t.runtime_id,
                        e.target_id, e.feedback_target_id, e.input, t.resource_snapshot_json
                 FROM turns t
                 JOIN events e ON e.interface_id = t.interface_id AND e.event_id = t.event_id
                 WHERE t.runtime_id = ?1 AND t.state = 'queued'
                   AND NOT EXISTS (
                     SELECT 1 FROM turns earlier
                     WHERE earlier.interface_id = t.interface_id
                       AND earlier.agent_id = t.agent_id
                       AND earlier.audience_id = t.audience_id
                       AND earlier.state IN ('queued', 'running', 'uncertain')
                       AND (earlier.started_at < t.started_at
                         OR (earlier.started_at = t.started_at AND earlier.id < t.id))
                   )
                 ORDER BY t.started_at, t.id LIMIT 1",
                [runtime_id],
                |row| {
                    Ok(ClaimedTurn {
                        turn_id: row.get(0)?,
                        interface_id: row.get(1)?,
                        agent_id: row.get(2)?,
                        audience_id: row.get(3)?,
                        runtime_id: row.get(4)?,
                        target_id: row.get(5)?,
                        feedback_target_id: row.get(6)?,
                        input: row.get(7)?,
                        resources: json_column(row, 8)?,
                    })
                },
            )
            .optional()?;
        let Some(turn) = turn else {
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
        transaction.commit()?;
        Ok(Some(turn))
    }

    /// The continuation reference the last completed turn of this conversation
    /// left for its runtime, if any.
    pub fn continuation(&self, turn: &ClaimedTurn) -> Result<Option<String>> {
        Ok(self
            .connect()?
            .query_row(
                "SELECT native_ref FROM continuations
                 WHERE interface_id = ?1 AND agent_id = ?2 AND audience_id = ?3 AND runtime_id = ?4",
                params![
                    turn.interface_id,
                    turn.agent_id,
                    turn.audience_id,
                    turn.runtime_id
                ],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Commits a successful runtime response: completed output, the new
    /// continuation reference, and the delivery intent, atomically.
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
        transaction.execute(
            "INSERT INTO continuations(interface_id, agent_id, audience_id, runtime_id, native_ref, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(interface_id, agent_id, audience_id, runtime_id)
             DO UPDATE SET native_ref = excluded.native_ref, updated_at = excluded.updated_at",
            params![
                turn.interface_id,
                turn.agent_id,
                turn.audience_id,
                turn.runtime_id,
                response.continuation_ref,
                now,
            ],
        )?;
        enqueue_delivery(&transaction, turn, &response.output, now)?;
        transaction.commit()?;
        Ok(())
    }

    /// Records that the outcome of a running turn cannot be proven, and queues
    /// a delivery telling the conversation so.
    ///
    /// Idempotent for the worker's persist-until-recorded retry loop. A retry
    /// also succeeds when the preceding completion write actually committed;
    /// any other state remains an invariant violation.
    pub fn mark_uncertain(&self, turn: &ClaimedTurn, error: &str) -> Result<()> {
        let mut connection = self.connect()?;
        let transaction = write_transaction(&mut connection)?;
        let now = timestamp()?;
        let changed = transaction.execute(
            "UPDATE turns SET state = 'uncertain', error = ?3, finished_at = ?4
             WHERE id = ?1 AND runtime_id = ?2 AND state = 'running'",
            params![turn.turn_id, turn.runtime_id, error, now],
        )?;
        if changed == 1 {
            enqueue_delivery(
                &transaction,
                turn,
                "I could not determine whether that turn completed. I will not run it again automatically.",
                now,
            )?;
        } else if !matches!(
            turn_state(&transaction, &turn.turn_id, &turn.runtime_id)?,
            Some(TurnState::Uncertain | TurnState::Completed)
        ) {
            bail!(
                "turn {} was not running, uncertain, or already completed",
                turn.turn_id
            );
        }
        transaction.commit()?;
        Ok(())
    }

    /// Returns a running turn to the queue. Only valid when the runtime
    /// provably never received the work. A retry succeeds only when the same
    /// transition already committed.
    pub fn requeue_turn(&self, turn: &ClaimedTurn) -> Result<()> {
        let mut connection = self.connect()?;
        let transaction = write_transaction(&mut connection)?;
        let changed = transaction.execute(
            "UPDATE turns SET state = 'queued', error = NULL, finished_at = NULL
             WHERE id = ?1 AND runtime_id = ?2 AND state = 'running'",
            params![turn.turn_id, turn.runtime_id],
        )?;
        if changed == 0
            && turn_state(&transaction, &turn.turn_id, &turn.runtime_id)? != Some(TurnState::Queued)
        {
            bail!("turn {} was not running or already queued", turn.turn_id);
        }
        transaction.commit()?;
        Ok(())
    }

    /// Marks every turn still recorded as running as uncertain. Run at
    /// startup: a turn that was running when core stopped may or may not have
    /// executed, and Carillon does not infer that repeating it is safe.
    pub fn recover_running_turns(&self) -> Result<usize> {
        let mut connection = self.connect()?;
        let transaction = write_transaction(&mut connection)?;
        let now = timestamp()?;
        transaction.execute(
            "INSERT INTO deliveries(turn_id, interface_id, target_id, feedback_target_id,
                 content, state, updated_at)
             SELECT t.id, t.interface_id, e.target_id, e.feedback_target_id, ?1, 'pending', ?2
             FROM turns t
             JOIN events e ON e.interface_id = t.interface_id AND e.event_id = t.event_id
             WHERE t.state = 'running'",
            params![
                "Carillon restarted during that turn. Its outcome is uncertain, so it was not run again.",
                now,
            ],
        )?;
        let recovered = transaction.execute(
            "UPDATE turns SET state = 'uncertain', error = ?1, finished_at = ?2
             WHERE state = 'running'",
            params!["core restarted while the runtime was executing", now],
        )?;
        transaction.commit()?;
        Ok(recovered)
    }

    /// Operator decision: run an uncertain turn again. The stale uncertainty
    /// delivery is withdrawn because the retried run will produce its own.
    pub fn retry_turn(&self, turn_id: &str) -> Result<()> {
        let mut connection = self.connect()?;
        let transaction = write_transaction(&mut connection)?;
        let changed = transaction.execute(
            "UPDATE turns SET state = 'queued', error = NULL, finished_at = NULL
             WHERE id = ?1 AND state = 'uncertain'",
            [turn_id],
        )?;
        if changed != 1 {
            bail!("turn {turn_id} is not uncertain");
        }
        transaction.execute("DELETE FROM deliveries WHERE turn_id = ?1", [turn_id])?;
        transaction.commit()?;
        Ok(())
    }

    /// Operator decision: abandon an uncertain turn without running it again,
    /// unblocking later turns in its conversation.
    pub fn skip_turn(&self, turn_id: &str) -> Result<()> {
        let changed = self.connect()?.execute(
            "UPDATE turns SET state = 'skipped', finished_at = ?2
             WHERE id = ?1 AND state = 'uncertain'",
            params![turn_id, timestamp()?],
        )?;
        if changed != 1 {
            bail!("turn {turn_id} is not uncertain");
        }
        Ok(())
    }

    /// Marks every claimed delivery as uncertain. Run at core startup: the
    /// interface holding the claim may already have sent the content.
    pub fn recover_deliveries(&self) -> Result<usize> {
        Ok(self.connect()?.execute(
            "UPDATE deliveries SET state = 'uncertain', claim_owner = NULL, updated_at = ?1
             WHERE state = 'delivering'",
            [timestamp()?],
        )?)
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
                "SELECT id, turn_id, target_id, feedback_target_id, content FROM deliveries
                 WHERE interface_id = ?1
                   AND (state = 'pending' OR (state = 'delivering' AND claim_owner = ?2))
                 ORDER BY CASE state WHEN 'delivering' THEN 0 ELSE 1 END, id LIMIT 1",
                params![interface_id, owner],
                |row| {
                    Ok(Delivery {
                        id: row.get(0)?,
                        turn_id: row.get(1)?,
                        target_id: row.get(2)?,
                        feedback_target_id: row.get(3)?,
                        content: row.get(4)?,
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
                        row.get::<_, DeliveryState>(0)?,
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
        Ok(self
            .connect()?
            .query_row(
                "SELECT t.id, t.event_id, t.audience_id, t.user_id, e.input, t.state, t.output,
                        t.provider_snapshot_json, t.resource_snapshot_json, d.state, d.native_ref, t.error
                 FROM turns t
                 JOIN events e ON e.interface_id = t.interface_id AND e.event_id = t.event_id
                 LEFT JOIN deliveries d ON d.turn_id = t.id
                 WHERE t.id = ?1",
                [turn_id],
                |row| {
                    Ok(TurnInspection {
                        turn_id: row.get(0)?,
                        event_id: row.get(1)?,
                        audience_id: row.get(2)?,
                        user_id: row.get(3)?,
                        input: row.get(4)?,
                        state: row.get(5)?,
                        output: row.get(6)?,
                        providers: json_column(row, 7)?,
                        resources: json_column(row, 8)?,
                        delivery_state: row.get(9)?,
                        delivery_native_ref: row.get(10)?,
                        error: row.get(11)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn turn_counts(&self) -> Result<StateCounts> {
        let connection = self.connect()?;
        let mut statement =
            connection.prepare("SELECT state, COUNT(*) FROM turns GROUP BY state")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, TurnState>(0)?, row.get::<_, u32>(1)?))
        })?;
        let mut counts = StateCounts::default();
        for row in rows {
            let (state, count) = row?;
            let count = u64::from(count);
            match state {
                TurnState::Queued => counts.pending += count,
                TurnState::Running => counts.active += count,
                TurnState::Completed => counts.completed += count,
                TurnState::Skipped => counts.interrupted += count,
                TurnState::Uncertain => counts.uncertain += count,
            }
        }
        Ok(counts)
    }

    pub fn delivery_counts(&self) -> Result<StateCounts> {
        let connection = self.connect()?;
        let mut statement =
            connection.prepare("SELECT state, COUNT(*) FROM deliveries GROUP BY state")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, DeliveryState>(0)?, row.get::<_, u32>(1)?))
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
            "SELECT t.id, t.event_id, t.state, t.started_at, d.state
             FROM turns t LEFT JOIN deliveries d ON d.turn_id = t.id
             WHERE (?1 IS NULL OR t.event_id = ?1)
             ORDER BY t.started_at DESC, t.id DESC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![event_id, limit.clamp(1, 100)], |row| {
            Ok(TurnSummary {
                turn_id: row.get(0)?,
                event_id: row.get(1)?,
                state: row.get(2)?,
                started_at: row.get(3)?,
                delivery_state: row.get(4)?,
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

fn turn_state(
    transaction: &Transaction<'_>,
    turn_id: &str,
    runtime_id: &str,
) -> Result<Option<TurnState>> {
    Ok(transaction
        .query_row(
            "SELECT state FROM turns WHERE id = ?1 AND runtime_id = ?2",
            params![turn_id, runtime_id],
            |row| row.get(0),
        )
        .optional()?)
}

fn json_column<T: DeserializeOwned>(row: &Row<'_>, index: usize) -> rusqlite::Result<T> {
    let text: String = row.get(index)?;
    serde_json::from_str(&text).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(index, Type::Text, Box::new(error))
    })
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
    use tempfile::tempdir;

    use super::Database;
    use crate::model::{
        DeliveryOutcome, ExternalEvent, ResourceSnapshot, RuntimeTurnResponse, TurnState,
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
        }
    }

    fn resources() -> ResourceSnapshot {
        ResourceSnapshot {
            id: "resources".to_owned(),
            system_sha256: "a".to_owned(),
            context_sha256: "b".to_owned(),
            settings_sha256: "c".to_owned(),
            extensions_sha256: "d".to_owned(),
            skills_sha256: "e".to_owned(),
        }
    }

    fn accept(database: &Database, id: &str, audience: &str) -> Result<crate::model::EventReceipt> {
        database.accept_event(
            "discord",
            "runtime",
            &event(id, audience),
            &json!([]),
            &resources(),
        )
    }

    #[test]
    fn event_is_idempotent_and_completion_is_atomic() -> Result<()> {
        let directory = tempdir()?;
        let database = Database::open(&directory.path().join("state.sqlite3"))?;
        let first = accept(&database, "one", "conversation")?;
        let duplicate = accept(&database, "one", "conversation")?;
        assert!(!first.duplicate);
        assert!(duplicate.duplicate);
        assert_eq!(first.turn_id, duplicate.turn_id);
        assert_eq!(first.state, TurnState::Queued);

        let turn = database
            .claim_turn("runtime")?
            .context("turn was not queued")?;
        database.complete_turn(
            &turn,
            &RuntimeTurnResponse {
                correlation_id: turn.turn_id.clone(),
                output: "reply".to_owned(),
                continuation_ref: "native".to_owned(),
            },
        )?;
        assert_eq!(database.continuation(&turn)?.as_deref(), Some("native"));
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
    fn uncertain_turn_blocks_its_conversation_but_not_others() -> Result<()> {
        let directory = tempdir()?;
        let database = Database::open(&directory.path().join("state.sqlite3"))?;
        accept(&database, "one", "blocked")?;
        accept(&database, "two", "blocked")?;
        let first = database
            .claim_turn("runtime")?
            .context("turn was not queued")?;
        database.mark_uncertain(&first, "ambiguous")?;
        assert!(database.claim_turn("runtime")?.is_none());

        accept(&database, "three", "unrelated")?;
        let unrelated = database
            .claim_turn("runtime")?
            .context("unrelated conversation was blocked")?;
        assert_eq!(unrelated.audience_id, "unrelated");
        Ok(())
    }

    #[test]
    fn restart_marks_running_turn_uncertain_without_requeueing_it() -> Result<()> {
        let directory = tempdir()?;
        let database = Database::open(&directory.path().join("state.sqlite3"))?;
        accept(&database, "one", "conversation")?;
        database
            .claim_turn("runtime")?
            .context("turn was not queued")?;
        assert_eq!(database.recover_running_turns()?, 1);
        assert!(database.claim_turn("runtime")?.is_none());
        assert_eq!(database.turn_counts()?.uncertain, 1);
        let turn_id = database.recent_turns(Some("one"), 1)?[0].turn_id.clone();
        database.retry_turn(&turn_id)?;
        let retried = database
            .claim_turn("runtime")?
            .context("retried turn was not queued")?;
        database.mark_uncertain(&retried, "still ambiguous")?;
        database.skip_turn(&turn_id)?;
        assert!(database.claim_turn("runtime")?.is_none());
        assert_eq!(database.turn_counts()?.interrupted, 1);
        Ok(())
    }

    #[test]
    fn delivery_claim_and_ack_retries_are_idempotent() -> Result<()> {
        let directory = tempdir()?;
        let database = Database::open(&directory.path().join("state.sqlite3"))?;
        accept(&database, "one", "first")?;
        accept(&database, "two", "second")?;

        for output in ["first reply", "second reply"] {
            let turn = database
                .claim_turn("runtime")?
                .context("turn was not queued")?;
            database.complete_turn(
                &turn,
                &RuntimeTurnResponse {
                    correlation_id: turn.turn_id.clone(),
                    output: output.to_owned(),
                    continuation_ref: format!("{output} continuation"),
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
    fn worker_transitions_only_retry_their_committed_target_state() -> Result<()> {
        let directory = tempdir()?;
        let database = Database::open(&directory.path().join("state.sqlite3"))?;

        accept(&database, "uncertain", "one")?;
        let uncertain = database
            .claim_turn("runtime")?
            .context("uncertain turn was not queued")?;
        database.mark_uncertain(&uncertain, "ambiguous")?;
        database.mark_uncertain(&uncertain, "ambiguous")?;
        assert!(database.requeue_turn(&uncertain).is_err());

        accept(&database, "requeued", "two")?;
        let requeued = database
            .claim_turn("runtime")?
            .context("requeued turn was not queued")?;
        database.requeue_turn(&requeued)?;
        database.requeue_turn(&requeued)?;
        assert!(database.mark_uncertain(&requeued, "ambiguous").is_err());

        let requeued = database
            .claim_turn("runtime")?
            .context("requeued turn could not be claimed again")?;
        database.complete_turn(
            &requeued,
            &RuntimeTurnResponse {
                correlation_id: requeued.turn_id.clone(),
                output: "done".to_owned(),
                continuation_ref: "native".to_owned(),
            },
        )?;
        assert!(database.requeue_turn(&requeued).is_err());
        database.mark_uncertain(&requeued, "ambiguous")?;
        assert_eq!(
            database
                .inspect_turn(&requeued.turn_id)?
                .context("completed turn disappeared")?
                .state,
            TurnState::Completed
        );
        Ok(())
    }
}
