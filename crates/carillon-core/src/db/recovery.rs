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

    /// Settles what core was doing when it stopped. Run at startup.
    ///
    /// A message running on a runtime that is not durable may or may not
    /// have executed, and Carillon does not infer that repeating it is safe:
    /// it is interrupted. A running observation only reads native records,
    /// so it is asked again and delivers its report once, when it completes.
    ///
    /// Of the `durable` runtimes, a message core handed over goes back to
    /// the queue to be handed over again in order: one started on the
    /// synchronous path before its runtime became durable would otherwise
    /// stay running for good, and every turn the runtime holds is handed to
    /// it again. It takes a turn it accepted before as the same one. A turn
    /// the runtime opened itself stays running. Their observations are
    /// interrupted with `observation_notice`: a durable runtime answers none.
    pub(crate) fn recover_running_turns(
        &self,
        durable: &[&str],
        observation_notice: &str,
    ) -> Result<usize> {
        let mut connection = self.connect()?;
        let transaction = write_transaction(&mut connection)?;
        let now = timestamp()?;
        let durable = serde_json::to_string(durable)?;
        let interrupt = |which: &str, notice: &str, error: &str| -> Result<usize> {
            transaction.execute(
                &format!(
                    "INSERT INTO deliveries(turn_id, interface_id, target_id, feedback_target_id,
                         content, state, updated_at)
                     SELECT id, interface_id, target_id, feedback_target_id, ?1, 'pending', ?2
                     FROM turns WHERE {which} ORDER BY acceptance_sequence"
                ),
                params![notice, now, durable],
            )?;
            Ok(transaction.execute(
                &format!(
                    "UPDATE turns SET state = 'interrupted', error = ?1, finished_at = ?2
                     WHERE {which}"
                ),
                params![error, now, durable],
            )?)
        };
        let mut recovered = interrupt(
            "state = 'running' AND follow_up_json IS NULL
             AND runtime_id NOT IN (SELECT value FROM json_each(?3))",
            "Carillon restarted during that turn. It was not run again. Send another message to continue in this conversation.",
            "core restarted while the runtime was executing",
        )?;
        recovered += interrupt(
            "state IN ('queued', 'running') AND follow_up_json IS NOT NULL
             AND runtime_id IN (SELECT value FROM json_each(?3))",
            observation_notice,
            "a durable runtime answers no observation",
        )?;
        transaction.execute(
            "UPDATE turns SET state = 'queued'
             WHERE state = 'running' AND follow_up_json IS NULL
               AND runtime_id IN (SELECT value FROM json_each(?1))
               AND substr(event_id, 1, length(?2)) <> ?2",
            params![durable, RUNTIME_EVENT_PREFIX],
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
