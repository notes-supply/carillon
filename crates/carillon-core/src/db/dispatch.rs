use rusqlite::Row;
use serde_json::Value;

use super::{CapturedFile, Database, OptionalExtension, Result, captured_files, json_column};

/// A turn as core hands it to its runtime: queued, as the dispatcher hands
/// it over, leaving it queued until the runtime takes it, or running, as the
/// runtime reads it back.
#[derive(Clone, Debug)]
pub(crate) struct QueuedTurn {
    pub turn_id: String,
    pub interface_id: String,
    pub agent_id: String,
    pub audience_id: String,
    pub runtime_id: String,
    pub input: String,
    pub resource_snapshot_id: String,
    pub native: Value,
    pub attachments: Vec<CapturedFile>,
}

/// The columns [`queued_turn`] reads, in its order.
const TURN: &str =
    "id, interface_id, agent_id, audience_id, runtime_id, input, resource_snapshot_id, native_json";

/// A turn as [`TURN`] selects it, without its files.
fn queued_turn(row: &Row<'_>) -> rusqlite::Result<QueuedTurn> {
    Ok(QueuedTurn {
        turn_id: row.get(0)?,
        interface_id: row.get(1)?,
        agent_id: row.get(2)?,
        audience_id: row.get(3)?,
        runtime_id: row.get(4)?,
        input: row.get(5)?,
        resource_snapshot_id: row.get(6)?,
        native: json_column(row, 7)?,
        attachments: Vec::new(),
    })
}

impl Database {
    /// A runtime's oldest queued turn, left queued: the runtime orders its
    /// conversations itself, so no earlier turn holds it back. One
    /// dispatcher per runtime hands its turns over in acceptance order.
    pub(crate) fn next_dispatch(&self, runtime_id: &str) -> Result<Option<QueuedTurn>> {
        let connection = self.connect()?;
        let turn = connection
            .query_row(
                &format!(
                    "SELECT {TURN} FROM turns WHERE runtime_id = ?1 AND state = 'queued'
                     ORDER BY acceptance_sequence LIMIT 1"
                ),
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

    /// The turns core records running on `runtime_id`, oldest first, each
    /// with its event ID, read in one snapshot.
    pub(crate) fn running_turns(&self, runtime_id: &str) -> Result<Vec<(QueuedTurn, String)>> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction()?;
        let running = transaction
            .prepare(&format!(
                "SELECT {TURN}, event_id FROM turns WHERE runtime_id = ?1 AND state = 'running'
                 ORDER BY acceptance_sequence"
            ))?
            .query_map([runtime_id], |row| Ok((queued_turn(row)?, row.get(8)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        running
            .into_iter()
            .map(|(mut turn, event_id)| {
                turn.attachments = captured_files(&transaction, Some(&turn.turn_id))?;
                Ok((turn, event_id))
            })
            .collect()
    }

    /// Records that the runtime holds `turn`: it runs there. A turn the
    /// runtime settled before this was recorded keeps its outcome.
    pub(crate) fn dispatched(&self, turn: &QueuedTurn) -> Result<()> {
        self.connect()?.execute(
            "UPDATE turns SET state = 'running'
             WHERE id = ?1 AND runtime_id = ?2 AND state = 'queued'",
            [&turn.turn_id, &turn.runtime_id],
        )?;
        Ok(())
    }
}
