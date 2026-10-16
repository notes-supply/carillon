use super::{Database, Result, params, timestamp, write_transaction};

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
        observation_notice: &str,
    ) -> Result<usize> {
        let mut connection = self.connect()?;
        let transaction = write_transaction(&mut connection)?;
        let now = timestamp()?;
        transaction.execute(
            "INSERT INTO deliveries(turn_id, interface_id, target_id, feedback_target_id,
                 content, state, updated_at)
             SELECT id, interface_id, target_id, feedback_target_id,
                 CASE WHEN follow_up_json IS NULL THEN ?3 ELSE ?4 END, 'pending', ?5
             FROM turns WHERE state = 'queued' AND agent_id = ?1 AND runtime_id = ?2
             ORDER BY acceptance_sequence",
            params![agent_id, runtime_id, explanation, observation_notice, now],
        )?;
        let interrupted = transaction.execute(
            "UPDATE turns SET state = 'interrupted', error = ?3, finished_at = ?4
             WHERE state = 'queued' AND agent_id = ?1 AND runtime_id = ?2",
            params![agent_id, runtime_id, explanation, now],
        )?;
        transaction.commit()?;
        Ok(interrupted)
    }

    /// Marks every message still recorded as running as interrupted, and
    /// returns running observations to the queue. Run at startup: a message
    /// that was running when core stopped may or may not have executed, and
    /// Carillon does not infer that repeating it is safe; an observation
    /// only reads native records, so it is asked again and delivers its
    /// report once, when it completes.
    pub(crate) fn recover_running_turns(&self) -> Result<usize> {
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
}
