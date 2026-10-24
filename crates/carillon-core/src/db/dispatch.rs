use rusqlite::Row;
use serde_json::Value;

use super::{
    CapturedFile, Database, OptionalExtension, Result, captured_files, json_column,
    optional_json_column, write_transaction,
};

/// A queued turn as core hands it to a runtime: as a claim takes it, or as
/// a durable runtime's dispatcher hands it over, leaving it queued until the
/// runtime takes it.
#[derive(Clone, Debug)]
pub(crate) struct QueuedTurn {
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

/// A queued turn as a claim selects it, without its files.
pub(super) fn queued_turn(row: &Row<'_>) -> rusqlite::Result<QueuedTurn> {
    Ok(QueuedTurn {
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
}

impl Database {
    /// A durable runtime's oldest queued message, left queued: the runtime
    /// orders its conversations itself, so no earlier turn holds it back.
    /// One dispatcher per runtime hands its turns over in acceptance order.
    pub(crate) fn next_dispatch(&self, runtime_id: &str) -> Result<Option<QueuedTurn>> {
        let connection = self.connect()?;
        let turn = connection
            .query_row(
                "SELECT id, interface_id, agent_id, audience_id, runtime_id, target_id,
                    feedback_target_id, input, resource_snapshot_id, native_json, follow_up_json
                 FROM turns
                 WHERE runtime_id = ?1 AND state = 'queued' AND follow_up_json IS NULL
                 ORDER BY acceptance_sequence LIMIT 1",
                [runtime_id],
                queued_turn,
            )
            .optional()?;
        let Some(mut turn) = turn else {
            return Ok(None);
        };
        turn.attachments = captured_files(&connection, Some(&turn.turn_id))?;
        Ok(Some(turn))
    }

    /// Records that the durable runtime holds `turn`: it runs there, and its
    /// files are in the runtime's conversation. A turn the runtime settled
    /// before this was recorded keeps its outcome.
    pub(crate) fn dispatched(&self, turn: &QueuedTurn) -> Result<()> {
        let mut connection = self.connect()?;
        let transaction = write_transaction(&mut connection)?;
        transaction.execute(
            "UPDATE turns SET state = 'running'
             WHERE id = ?1 AND runtime_id = ?2 AND state = 'queued'",
            [&turn.turn_id, &turn.runtime_id],
        )?;
        transaction.execute(
            "UPDATE turn_attachments SET noted = 1 WHERE turn_id = ?1 AND noted = 0",
            [&turn.turn_id],
        )?;
        transaction.commit()?;
        Ok(())
    }
}
