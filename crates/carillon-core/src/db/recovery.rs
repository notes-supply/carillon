use rusqlite::Transaction;

use super::{Database, RUNTIME_EVENT_PREFIX, Result, params, timestamp, write_transaction};

impl Database {
    pub(crate) fn queued_owners(&self) -> Result<Vec<(String, String)>> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare("SELECT DISTINCT agent_id, runtime_id FROM turns WHERE state = 'queued'")?;
        Ok(statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?)
    }

    fn interrupt(
        &self,
        transaction: &Transaction<'_>,
        turns: &[String],
        error: &str,
        notice: &str,
    ) -> Result<()> {
        let now = timestamp()?;
        for turn_id in turns {
            transaction.execute(
                "UPDATE turns SET state = 'interrupted', error = ?2, finished_at = ?3
                 WHERE id = ?1",
                params![turn_id, error, now],
            )?;
            self.record_delivery(transaction, turn_id, notice, None, now)?;
        }
        Ok(())
    }

    /// Settles only unclaimed turns of this owner, with their notices in the same transaction.
    pub(crate) fn interrupt_queued(
        &self,
        agent_id: &str,
        runtime_id: &str,
        explanation: &str,
    ) -> Result<usize> {
        let mut connection = self.connect()?;
        let transaction = write_transaction(&mut connection)?;
        let queued = transaction
            .prepare(
                "SELECT id FROM turns
                 WHERE state = 'queued' AND agent_id = ?1 AND runtime_id = ?2
                 ORDER BY acceptance_sequence",
            )?
            .query_map(params![agent_id, runtime_id], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        self.interrupt(&transaction, &queued, explanation, explanation)?;
        transaction.commit()?;
        Ok(queued.len())
    }

    /// Settles what core was doing when it stopped. Run at startup.
    ///
    /// A turn core handed one of the `configured` runtimes goes back to the
    /// queue, to be handed over again in order: the runtime takes a turn it
    /// accepted before as the same one. A turn the runtime opened itself
    /// stays running. A turn of a runtime no longer configured can no
    /// longer be settled, and is interrupted.
    pub(crate) fn recover_running_turns(&self, configured: &[&str]) -> Result<usize> {
        let mut connection = self.connect()?;
        let transaction = write_transaction(&mut connection)?;
        let gone = transaction
            .prepare(
                "SELECT id FROM turns
                 WHERE state = 'running' AND runtime_id NOT IN (SELECT value FROM json_each(?1))
                 ORDER BY acceptance_sequence",
            )?
            .query_map([serde_json::to_string(configured)?], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        self.interrupt(
            &transaction,
            &gone,
            "its runtime is no longer configured",
            "Carillon restarted during that turn. It was not run again. Send another message to continue in this conversation.",
        )?;
        transaction.execute(
            "UPDATE turns SET state = 'queued'
             WHERE state = 'running' AND substr(event_id, 1, length(?1)) <> ?1",
            params![RUNTIME_EVENT_PREFIX],
        )?;
        transaction.commit()?;
        Ok(gone.len())
    }
}
