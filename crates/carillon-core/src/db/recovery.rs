use super::{Database, RUNTIME_EVENT_PREFIX, Result, params, timestamp, write_transaction};

/// Running turns of a runtime not among the JSON array `?3`.
const GONE: &str = "state = 'running' AND runtime_id NOT IN (SELECT value FROM json_each(?3))";

impl Database {
    pub(crate) fn queued_owners(&self) -> Result<Vec<(String, String)>> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare("SELECT DISTINCT agent_id, runtime_id FROM turns WHERE state = 'queued'")?;
        Ok(statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?)
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
        let now = timestamp()?;
        transaction.execute(
            "INSERT INTO deliveries(turn_id, interface_id, target_id, feedback_target_id,
                 content, state, updated_at)
             SELECT id, interface_id, target_id, feedback_target_id, ?3, 'pending', ?4
             FROM turns WHERE state = 'queued' AND agent_id = ?1 AND runtime_id = ?2
             ORDER BY acceptance_sequence",
            params![agent_id, runtime_id, explanation, now],
        )?;
        let interrupted = transaction.execute(
            "UPDATE turns SET state = 'interrupted', error = ?3, finished_at = ?4
             WHERE state = 'queued' AND agent_id = ?1 AND runtime_id = ?2",
            params![agent_id, runtime_id, explanation, now],
        )?;
        transaction.commit()?;
        Ok(interrupted)
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
        let now = timestamp()?;
        let configured = serde_json::to_string(configured)?;
        transaction.execute(
            &format!(
                "INSERT INTO deliveries(turn_id, interface_id, target_id, feedback_target_id,
                     content, state, updated_at)
                 SELECT id, interface_id, target_id, feedback_target_id, ?1, 'pending', ?2
                 FROM turns WHERE {GONE} ORDER BY acceptance_sequence"
            ),
            params![
                "Carillon restarted during that turn. It was not run again. Send another message to continue in this conversation.",
                now,
                configured
            ],
        )?;
        let recovered = transaction.execute(
            &format!(
                "UPDATE turns SET state = 'interrupted', error = ?1, finished_at = ?2 WHERE {GONE}"
            ),
            params!["its runtime is no longer configured", now, configured],
        )?;
        transaction.execute(
            "UPDATE turns SET state = 'queued'
             WHERE state = 'running' AND substr(event_id, 1, length(?1)) <> ?1",
            params![RUNTIME_EVENT_PREFIX],
        )?;
        transaction.commit()?;
        Ok(recovered)
    }
}
