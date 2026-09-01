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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Continuation {
    pub runtime: String,
    pub native_ref: String,
}

#[derive(Clone, Debug)]
pub struct PendingTurn {
    pub turn: String,
    pub audience: String,
    pub runtime: String,
    pub interface: String,
    pub target: String,
    pub feedback_target: String,
    pub agent: String,
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
        let mut connection = Connection::open(&self.path)
            .with_context(|| format!("open {}", self.path.display()))?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        let transaction = connection.transaction()?;
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS events (
                interface_id TEXT NOT NULL,
                event_id TEXT NOT NULL,
                audience_id TEXT NOT NULL,
                user_id TEXT NOT NULL,
                delivery_target_id TEXT NOT NULL,
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
                audience_id TEXT NOT NULL,
                user_id TEXT NOT NULL,
                agent_id TEXT NOT NULL,
                runtime_id TEXT NOT NULL,
                state TEXT NOT NULL,
                provider_snapshot_json TEXT NOT NULL,
                resource_snapshot_json TEXT NOT NULL,
                output TEXT,
                native_evidence_json TEXT,
                capability_activity_json TEXT NOT NULL DEFAULT '[]',
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
                PRIMARY KEY(interface_id, agent_id, audience_id)
            );
            CREATE TABLE IF NOT EXISTS deliveries (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                turn_id TEXT NOT NULL UNIQUE,
                interface_id TEXT NOT NULL,
                target_id TEXT NOT NULL,
                feedback_target_id TEXT NOT NULL,
                content TEXT,
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
        transaction.commit()?;
        Ok(())
    }

    pub fn accept_event(
        &self,
        interface_id: &str,
        agent_id: &str,
        runtime_id: &str,
        event: &ExternalEvent,
        providers: &Value,
        resources: &ResourceSnapshot,
    ) -> Result<AcceptedTurn> {
        let audience_id = &event.audience_id;
        let mut connection = self.connect()?;
        let transaction = connection.transaction()?;
        let existing = transaction
            .query_row(
                "SELECT id, state FROM turns WHERE interface_id = ?1 AND event_id = ?2",
                params![interface_id, event.event_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        if let Some((turn_id, state)) = existing {
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
            "INSERT INTO events(interface_id, event_id, audience_id, user_id, delivery_target_id,
                 feedback_target_id, input, native_json, accepted_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                interface_id,
                event.event_id,
                audience_id,
                event.user_id,
                event.delivery_target_id,
                event.feedback_target_id,
                event.content,
                serde_json::to_string(&event.native)?,
                now,
            ],
        )?;
        transaction.execute(
            "INSERT INTO turns(id, interface_id, event_id, audience_id, user_id, agent_id, runtime_id, state,
                 provider_snapshot_json, resource_snapshot_json, started_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'accepted', ?8, ?9, ?10)",
            params![
                turn_id,
                interface_id,
                event.event_id,
                audience_id,
                event.user_id,
                agent_id,
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
            state: "accepted".to_owned(),
        })
    }

    pub fn mark_processing(&self, turn_id: &str) -> Result<()> {
        self.connect()?.execute(
            "UPDATE turns SET state = 'processing' WHERE id = ?1 AND state = 'accepted'",
            [turn_id],
        )?;
        Ok(())
    }

    pub fn complete_turn(
        &self,
        turn_id: &str,
        interface_id: &str,
        target_id: &str,
        feedback_target_id: &str,
        response: &RuntimeTurnResponse,
    ) -> Result<()> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction()?;
        let now = timestamp()?;
        let changed = transaction.execute(
            "UPDATE turns SET state = 'completed', output = ?2, native_evidence_json = ?3,
                 capability_activity_json = ?4, finished_at = ?5
             WHERE id = ?1 AND state IN ('accepted', 'processing')",
            params![
                turn_id,
                response.output,
                serde_json::to_string(&response.native)?,
                serde_json::to_string(&response.capability_activity)?,
                now,
            ],
        )?;
        if changed == 0 {
            bail!("turn {turn_id} was not active");
        }
        transaction.execute(
            "INSERT INTO deliveries(
                turn_id, interface_id, target_id, feedback_target_id, content, state, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6)",
            params![
                turn_id,
                interface_id,
                target_id,
                feedback_target_id,
                response.output,
                now
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn fail_turn(&self, turn_id: &str, state: &str, error: &str) -> Result<()> {
        if state != "failed" && state != "interrupted" {
            bail!("invalid terminal turn state {state}");
        }
        let mut connection = self.connect()?;
        let transaction = connection.transaction()?;
        let now = timestamp()?;
        let changed = transaction.execute(
            "UPDATE turns SET state = ?2, error = ?3, finished_at = ?4
             WHERE id = ?1 AND state IN ('accepted', 'processing')",
            params![turn_id, state, error, now],
        )?;
        if changed == 1 {
            let inserted = transaction.execute(
                "INSERT INTO deliveries(
                    turn_id, interface_id, target_id, feedback_target_id, content, state, updated_at
                 ) SELECT t.id, t.interface_id, e.delivery_target_id, e.feedback_target_id,
                          NULL, 'pending', ?2
                 FROM turns t
                 JOIN events e ON e.interface_id = t.interface_id AND e.event_id = t.event_id
                 WHERE t.id = ?1",
                params![turn_id, now],
            )?;
            if inserted != 1 {
                bail!("terminal turn {turn_id} has no event for interface feedback");
            }
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn continuation(
        &self,
        interface_id: &str,
        agent_id: &str,
        audience_id: &str,
    ) -> Result<Option<Continuation>> {
        Ok(self
            .connect()?
            .query_row(
                "SELECT runtime_id, native_ref FROM continuations
                 WHERE interface_id = ?1 AND agent_id = ?2 AND audience_id = ?3",
                params![interface_id, agent_id, audience_id],
                |row| {
                    Ok(Continuation {
                        runtime: row.get(0)?,
                        native_ref: row.get(1)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn save_continuation(
        &self,
        interface_id: &str,
        agent_id: &str,
        audience_id: &str,
        runtime_id: &str,
        native_ref: &str,
    ) -> Result<()> {
        self.connect()?.execute(
            "INSERT INTO continuations(
                interface_id, agent_id, audience_id, runtime_id, native_ref, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(interface_id, agent_id, audience_id) DO UPDATE SET
                native_ref = excluded.native_ref, updated_at = excluded.updated_at",
            params![
                interface_id,
                agent_id,
                audience_id,
                runtime_id,
                native_ref,
                timestamp()?
            ],
        )?;
        Ok(())
    }

    pub fn has_pending_predecessor(
        &self,
        interface_id: &str,
        agent_id: &str,
        audience_id: &str,
        event_id: &str,
    ) -> Result<bool> {
        let pending = self.connect()?.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM turns
                WHERE interface_id = ?1 AND agent_id = ?2 AND audience_id = ?3
                  AND event_id <> ?4 AND state IN ('accepted', 'processing')
            )",
            params![interface_id, agent_id, audience_id, event_id],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(pending != 0)
    }

    pub fn pending_turns(&self) -> Result<Vec<PendingTurn>> {
        let connection = self.connect()?;
        let mut statement = connection.prepare(
            "SELECT t.id, t.audience_id, t.runtime_id, t.interface_id, e.delivery_target_id,
                    e.feedback_target_id, t.agent_id
             FROM turns t
             JOIN events e ON e.interface_id = t.interface_id AND e.event_id = t.event_id
             WHERE t.state IN ('accepted', 'processing') ORDER BY t.started_at",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(PendingTurn {
                turn: row.get(0)?,
                audience: row.get(1)?,
                runtime: row.get(2)?,
                interface: row.get(3)?,
                target: row.get(4)?,
                feedback_target: row.get(5)?,
                agent: row.get(6)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
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
        let feedback_target_ids = {
            let mut statement = transaction.prepare(
                "SELECT DISTINCT feedback_target_id FROM deliveries
                 WHERE interface_id = ?1 AND state = 'uncertain' AND feedback_target_id <> ''",
            )?;
            statement
                .query_map([interface_id], |row| row.get(0))?
                .collect::<Result<Vec<String>, _>>()?
        };
        transaction.commit()?;
        Ok(feedback_target_ids)
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
        interface_id: &str,
        owner: &str,
        state: &str,
        native_ref: Option<&str>,
    ) -> Result<()> {
        if state != "delivered" && state != "uncertain" {
            bail!("invalid delivery state {state}");
        }
        let connection = self.connect()?;
        let changed = connection.execute(
            "UPDATE deliveries SET state = ?4, native_ref = ?5, claim_owner = NULL, updated_at = ?6
             WHERE id = ?1 AND interface_id = ?2 AND claim_owner = ?3 AND state = 'delivering'",
            params![id, interface_id, owner, state, native_ref, timestamp()?],
        )?;
        if changed == 1 {
            return Ok(());
        }
        let recorded = connection
            .query_row(
                "SELECT state, native_ref FROM deliveries WHERE id = ?1 AND interface_id = ?2",
                params![id, interface_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()?;
        if recorded
            .as_ref()
            .is_some_and(|(recorded_state, recorded_ref)| {
                recorded_state == state && recorded_ref.as_deref() == native_ref
            })
        {
            return Ok(());
        }
        bail!("delivery {id} is not claimed by interface instance {owner}")
    }

    pub fn inspect_turn(&self, turn_id: &str) -> Result<Option<TurnInspection>> {
        let connection = self.connect()?;
        connection
            .query_row(
                "SELECT t.id, t.event_id, t.audience_id, t.user_id, e.input, t.state, t.output,
                    t.provider_snapshot_json, t.resource_snapshot_json, t.native_evidence_json,
                    t.capability_activity_json, d.state, d.native_ref, t.error
             FROM turns t
             JOIN events e ON e.interface_id = t.interface_id AND e.event_id = t.event_id
             LEFT JOIN deliveries d ON d.turn_id = t.id
             WHERE t.id = ?1",
                [turn_id],
                |row| {
                    let components: String = row.get(7)?;
                    let resources: String = row.get(8)?;
                    let evidence: Option<String> = row.get(9)?;
                    let activity: String = row.get(10)?;
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        components,
                        resources,
                        evidence,
                        activity,
                        row.get::<_, Option<String>>(11)?,
                        row.get::<_, Option<String>>(12)?,
                        row.get::<_, Option<String>>(13)?,
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
                    native_evidence: row
                        .9
                        .map(|value| serde_json::from_str(&value))
                        .transpose()?,
                    capability_activity: serde_json::from_str(&row.10)?,
                    delivery_state: row.11,
                    delivery_native_ref: row.12,
                    error: row.13,
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
                "accepted" | "pending" => counts.pending += count,
                "processing" | "delivering" => counts.active += count,
                "completed" | "delivered" => counts.completed += count,
                "failed" | "interrupted" => counts.interrupted += count,
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
             FROM turns t
             LEFT JOIN deliveries d ON d.turn_id = t.id
             WHERE (?1 IS NULL OR t.event_id = ?1)
             ORDER BY t.started_at DESC, t.id DESC
             LIMIT ?2",
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

    #[test]
    fn duplicate_event_has_one_turn_and_one_delivery() -> Result<()> {
        let directory = tempdir()?;
        let database = Database::open(&directory.path().join("state.sqlite3"))?;
        let event = ExternalEvent {
            event_id: "external-1".to_owned(),
            agent_id: "agent".to_owned(),
            user_id: "user-1".to_owned(),
            audience_id: "conversation-1".to_owned(),
            delivery_target_id: "channel-1".to_owned(),
            feedback_target_id: "channel-1:message-1".to_owned(),
            content: "hello".to_owned(),
            native: json!({}),
        };
        let resources = ResourceSnapshot {
            root: "/agent".to_owned(),
            system_sha256: "a".to_owned(),
            context_sha256: "b".to_owned(),
            settings_sha256: "c".to_owned(),
            extensions_sha256: "d".to_owned(),
            skills_sha256: "e".to_owned(),
        };
        let first = database.accept_event(
            "discord",
            "agent",
            "runtime",
            &event,
            &json!([]),
            &resources,
        )?;
        let duplicate = database.accept_event(
            "discord",
            "agent",
            "runtime",
            &event,
            &json!([]),
            &resources,
        )?;
        assert!(!first.duplicate);
        assert!(duplicate.duplicate);
        assert_eq!(first.turn_id, duplicate.turn_id);

        database.mark_processing(&first.turn_id)?;
        database.complete_turn(
            &first.turn_id,
            "discord",
            "channel-1",
            "channel-1:message-1",
            &RuntimeTurnResponse {
                correlation_id: first.turn_id.clone(),
                output: "reply".to_owned(),
                continuation_ref: "native-1".to_owned(),
                native: json!({ "complete": true }),
                capability_activity: Vec::new(),
            },
        )?;
        let delivery = database
            .claim_delivery("discord", "instance-a")?
            .context("completed turn did not create a delivery")?;
        assert_eq!(delivery.content.as_deref(), Some("reply"));
        assert_eq!(
            database.reconcile_delivery_claims("discord", "instance-b")?,
            vec!["channel-1:message-1"]
        );
        let inspection = database
            .inspect_turn(&first.turn_id)?
            .context("turn inspection missing")?;
        assert_eq!(inspection.delivery_state.as_deref(), Some("uncertain"));
        database.acknowledge_delivery(delivery.id, "discord", "instance-a", "uncertain", None)?;
        assert!(
            database
                .acknowledge_delivery(delivery.id, "discord", "instance-a", "delivered", None,)
                .is_err()
        );
        Ok(())
    }
}
