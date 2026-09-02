use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use crate::model::{
    Delivery, ExternalEvent, ResourceSnapshot, RuntimeTurnResponse, StateCounts, TurnInspection,
    TurnSummary,
};

#[derive(Clone, Debug)]
pub struct Database {
    path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct AcceptedTurn {
    pub turn_id: String,
    pub duplicate: bool,
    pub state: String,
}

#[derive(Clone, Debug)]
pub struct QueuedTurn {
    pub turn_id: String,
    pub interface_id: String,
    pub agent_id: String,
    pub audience_id: String,
    pub target_id: String,
    pub feedback_target_id: String,
    pub input: String,
    pub resources: ResourceSnapshot,
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
            );
            CREATE TABLE IF NOT EXISTS backups (
                path TEXT PRIMARY KEY,
                created_at INTEGER NOT NULL
            );",
        )?;
        Ok(())
    }

    pub fn accept_event(
        &self,
        interface_id: &str,
        runtime_id: &str,
        event: &ExternalEvent,
        providers: &Value,
        resources: &ResourceSnapshot,
    ) -> Result<AcceptedTurn> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction()?;
        if let Some((turn_id, state)) = transaction
            .query_row(
                "SELECT id, state FROM turns WHERE interface_id = ?1 AND event_id = ?2",
                params![interface_id, event.event_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
        {
            transaction.commit()?;
            return Ok(AcceptedTurn {
                turn_id,
                duplicate: true,
                state,
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
        Ok(AcceptedTurn {
            turn_id,
            duplicate: false,
            state: "queued".to_owned(),
        })
    }

    pub fn claim_turn(&self, runtime_id: &str) -> Result<Option<QueuedTurn>> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction()?;
        let turn = transaction
            .query_row(
                "SELECT t.id, t.interface_id, t.agent_id, t.audience_id, e.target_id,
                        e.feedback_target_id, e.input, t.resource_snapshot_json
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
                    let resources: String = row.get(7)?;
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        resources,
                    ))
                },
            )
            .optional()?;
        let Some(row) = turn else {
            transaction.commit()?;
            return Ok(None);
        };
        let changed = transaction.execute(
            "UPDATE turns SET state = 'running' WHERE id = ?1 AND state = 'queued'",
            [&row.0],
        )?;
        if changed != 1 {
            bail!("turn {} could not be claimed", row.0);
        }
        transaction.commit()?;
        Ok(Some(QueuedTurn {
            turn_id: row.0,
            interface_id: row.1,
            agent_id: row.2,
            audience_id: row.3,
            target_id: row.4,
            feedback_target_id: row.5,
            input: row.6,
            resources: serde_json::from_str(&row.7)?,
        }))
    }

    pub fn continuation(
        &self,
        interface_id: &str,
        agent_id: &str,
        audience_id: &str,
        runtime_id: &str,
    ) -> Result<Option<String>> {
        Ok(self
            .connect()?
            .query_row(
                "SELECT native_ref FROM continuations
                 WHERE interface_id = ?1 AND agent_id = ?2 AND audience_id = ?3 AND runtime_id = ?4",
                params![interface_id, agent_id, audience_id, runtime_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn complete_turn(
        &self,
        turn: &QueuedTurn,
        runtime_id: &str,
        response: &RuntimeTurnResponse,
    ) -> Result<()> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction()?;
        let now = timestamp()?;
        let changed = transaction.execute(
            "UPDATE turns SET state = 'completed', output = ?2, finished_at = ?3
             WHERE id = ?1 AND state = 'running'",
            params![turn.turn_id, response.output, now],
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
                runtime_id,
                response.continuation_ref,
                now,
            ],
        )?;
        transaction.execute(
            "INSERT INTO deliveries(turn_id, interface_id, target_id, feedback_target_id,
                 content, state, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6)",
            params![
                turn.turn_id,
                turn.interface_id,
                turn.target_id,
                turn.feedback_target_id,
                response.output,
                now,
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn mark_uncertain(&self, turn: &QueuedTurn, error: &str) -> Result<()> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction()?;
        let now = timestamp()?;
        let changed = transaction.execute(
            "UPDATE turns SET state = 'uncertain', error = ?2, finished_at = ?3
             WHERE id = ?1 AND state = 'running'",
            params![turn.turn_id, error, now],
        )?;
        if changed == 1 {
            transaction.execute(
                "INSERT OR IGNORE INTO deliveries(turn_id, interface_id, target_id,
                     feedback_target_id, content, state, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6)",
                params![
                    turn.turn_id,
                    turn.interface_id,
                    turn.target_id,
                    turn.feedback_target_id,
                    "I could not determine whether that turn completed. I will not run it again automatically.",
                    now,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn recover_running_turns(&self) -> Result<usize> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction()?;
        let now = timestamp()?;
        let turns = {
            let mut statement = transaction.prepare(
                "SELECT t.id, t.interface_id, e.target_id, e.feedback_target_id
                 FROM turns t
                 JOIN events e ON e.interface_id = t.interface_id AND e.event_id = t.event_id
                 WHERE t.state = 'running'",
            )?;
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        for (turn_id, interface_id, target_id, feedback_target_id) in &turns {
            transaction.execute(
                "UPDATE turns SET state = 'uncertain', error = ?2, finished_at = ?3 WHERE id = ?1",
                params![
                    turn_id,
                    "core restarted while the runtime was executing",
                    now
                ],
            )?;
            transaction.execute(
                "INSERT OR IGNORE INTO deliveries(turn_id, interface_id, target_id,
                     feedback_target_id, content, state, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6)",
                params![
                    turn_id,
                    interface_id,
                    target_id,
                    feedback_target_id,
                    "Carillon restarted during that turn. Its outcome is uncertain, so it was not run again.",
                    now,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(turns.len())
    }

    pub fn audience_busy(
        &self,
        interface_id: &str,
        agent_id: &str,
        audience_id: &str,
    ) -> Result<bool> {
        self.connect()?
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM turns
                 WHERE interface_id = ?1 AND agent_id = ?2 AND audience_id = ?3
                   AND state IN ('queued', 'running', 'uncertain'))",
                params![interface_id, agent_id, audience_id],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub fn resolve_uncertain_turn(&self, turn_id: &str, retry: bool) -> Result<()> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction()?;
        let changed = if retry {
            transaction.execute(
                "UPDATE turns SET state = 'queued', error = NULL, finished_at = NULL
                 WHERE id = ?1 AND state = 'uncertain'",
                [turn_id],
            )?
        } else {
            transaction.execute(
                "UPDATE turns SET state = 'skipped', finished_at = ?2
                 WHERE id = ?1 AND state = 'uncertain'",
                params![turn_id, timestamp()?],
            )?
        };
        if changed != 1 {
            bail!("turn {turn_id} is not uncertain");
        }
        if retry {
            transaction.execute("DELETE FROM deliveries WHERE turn_id = ?1", [turn_id])?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn recover_deliveries(&self) -> Result<usize> {
        Ok(self.connect()?.execute(
            "UPDATE deliveries SET state = 'uncertain', claim_owner = NULL, updated_at = ?1
             WHERE state = 'delivering'",
            [timestamp()?],
        )?)
    }

    pub fn reconcile_delivery_claims(
        &self,
        interface_id: &str,
        owner: &str,
    ) -> Result<Vec<String>> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction()?;
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

    pub fn claim_delivery(&self, interface_id: &str, owner: &str) -> Result<Option<Delivery>> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction()?;
        let delivery = transaction
            .query_row(
                "SELECT id, turn_id, target_id, feedback_target_id, content FROM deliveries
                 WHERE interface_id = ?1 AND state = 'pending' ORDER BY id LIMIT 1",
                [interface_id],
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
            transaction.execute(
                "UPDATE deliveries SET state = 'delivering', claim_owner = ?2, updated_at = ?3
                 WHERE id = ?1 AND state = 'pending'",
                params![value.id, owner, timestamp()?],
            )?;
        }
        transaction.commit()?;
        Ok(delivery)
    }

    pub fn acknowledge_delivery(
        &self,
        id: i64,
        owner: &str,
        state: &str,
        native_ref: Option<&str>,
    ) -> Result<()> {
        if state != "delivered" && state != "uncertain" {
            bail!("invalid delivery state {state}");
        }
        let changed = self.connect()?.execute(
            "UPDATE deliveries SET state = ?3, native_ref = ?4, claim_owner = NULL, updated_at = ?5
             WHERE id = ?1 AND claim_owner = ?2 AND state = 'delivering'",
            params![id, owner, state, native_ref, timestamp()?],
        )?;
        if changed != 1 {
            bail!("delivery {id} is not claimed by interface instance {owner}");
        }
        Ok(())
    }

    pub fn inspect_turn(&self, turn_id: &str) -> Result<Option<TurnInspection>> {
        let connection = self.connect()?;
        connection
            .query_row(
                "SELECT t.id, t.event_id, t.audience_id, t.user_id, e.input, t.state, t.output,
                        t.provider_snapshot_json, t.resource_snapshot_json, d.state, d.native_ref, t.error
                 FROM turns t
                 JOIN events e ON e.interface_id = t.interface_id AND e.event_id = t.event_id
                 LEFT JOIN deliveries d ON d.turn_id = t.id
                 WHERE t.id = ?1",
                [turn_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, Option<String>>(9)?,
                        row.get::<_, Option<String>>(10)?,
                        row.get::<_, Option<String>>(11)?,
                    ))
                },
            )
            .optional()?
            .map(|row| {
                Ok(TurnInspection {
                    turn_id: row.0,
                    event_id: row.1,
                    audience_id: row.2,
                    user_id: row.3,
                    input: row.4,
                    state: row.5,
                    output: row.6,
                    providers: serde_json::from_str(&row.7)?,
                    resources: serde_json::from_str(&row.8)?,
                    delivery_state: row.9,
                    delivery_native_ref: row.10,
                    error: row.11,
                })
            })
            .transpose()
    }

    pub fn state_counts(&self, table: &str) -> Result<StateCounts> {
        if table != "turns" && table != "deliveries" {
            bail!("unsupported state table {table}");
        }
        let connection = self.connect()?;
        let mut statement = connection.prepare(&format!(
            "SELECT state, COUNT(*) FROM {table} GROUP BY state"
        ))?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?))
        })?;
        let mut counts = StateCounts::default();
        for row in rows {
            let (state, count) = row?;
            let count = u64::from(count);
            match state.as_str() {
                "queued" | "pending" => counts.pending += count,
                "running" | "delivering" => counts.active += count,
                "completed" | "delivered" => counts.completed += count,
                "skipped" => counts.interrupted += count,
                "uncertain" => counts.uncertain += count,
                _ => {}
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

    pub fn record_backup(&self, path: &Path) -> Result<()> {
        self.connect()?.execute(
            "INSERT OR REPLACE INTO backups(path, created_at) VALUES (?1, ?2)",
            params![path.display().to_string(), timestamp()?],
        )?;
        Ok(())
    }

    pub fn latest_backup(&self) -> Result<Option<String>> {
        Ok(self
            .connect()?
            .query_row(
                "SELECT path FROM backups ORDER BY created_at DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?)
    }
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
    use crate::model::{ExternalEvent, ResourceSnapshot, RuntimeTurnResponse};

    fn event(id: &str) -> ExternalEvent {
        ExternalEvent {
            event_id: id.to_owned(),
            agent_id: "agent".to_owned(),
            user_id: "user".to_owned(),
            audience_id: "conversation".to_owned(),
            delivery_target_id: "channel".to_owned(),
            feedback_target_id: "message".to_owned(),
            content: "hello".to_owned(),
            native: json!({}),
        }
    }

    fn resources() -> ResourceSnapshot {
        ResourceSnapshot {
            root: "/agent".to_owned(),
            system_sha256: "a".to_owned(),
            context_sha256: "b".to_owned(),
            settings_sha256: "c".to_owned(),
            extensions_sha256: "d".to_owned(),
            skills_sha256: "e".to_owned(),
        }
    }

    #[test]
    fn event_is_idempotent_and_completion_is_atomic() -> Result<()> {
        let directory = tempdir()?;
        let database = Database::open(&directory.path().join("state.sqlite3"))?;
        let first = database.accept_event(
            "discord",
            "runtime",
            &event("one"),
            &json!([]),
            &resources(),
        )?;
        let duplicate = database.accept_event(
            "discord",
            "runtime",
            &event("one"),
            &json!([]),
            &resources(),
        )?;
        assert!(!first.duplicate);
        assert!(duplicate.duplicate);
        assert_eq!(first.turn_id, duplicate.turn_id);

        let turn = database
            .claim_turn("runtime")?
            .context("turn was not queued")?;
        database.complete_turn(
            &turn,
            "runtime",
            &RuntimeTurnResponse {
                correlation_id: turn.turn_id.clone(),
                output: "reply".to_owned(),
                continuation_ref: "native".to_owned(),
            },
        )?;
        assert_eq!(
            database
                .continuation("discord", "agent", "conversation", "runtime")?
                .as_deref(),
            Some("native")
        );
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
    fn restart_marks_running_turn_uncertain_without_requeueing_it() -> Result<()> {
        let directory = tempdir()?;
        let database = Database::open(&directory.path().join("state.sqlite3"))?;
        database.accept_event(
            "discord",
            "runtime",
            &event("one"),
            &json!([]),
            &resources(),
        )?;
        database
            .claim_turn("runtime")?
            .context("turn was not queued")?;
        assert_eq!(database.recover_running_turns()?, 1);
        assert!(database.claim_turn("runtime")?.is_none());
        assert_eq!(database.state_counts("turns")?.uncertain, 1);
        let turn_id = database.recent_turns(Some("one"), 1)?[0].turn_id.clone();
        database.resolve_uncertain_turn(&turn_id, true)?;
        let retried = database
            .claim_turn("runtime")?
            .context("resolved turn was not queued")?;
        database.mark_uncertain(&retried, "still ambiguous")?;
        database.resolve_uncertain_turn(&turn_id, false)?;
        assert!(database.claim_turn("runtime")?.is_none());
        assert_eq!(database.state_counts("turns")?.interrupted, 1);
        Ok(())
    }
}
