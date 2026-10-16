use super::{
    Capture, Database, EventReceipt, ExternalEvent, Result, TurnState, Value, bail,
    conversation_runtime, event_receipt, params, timestamp, write_transaction,
};

#[derive(Clone, Copy)]
pub(crate) enum Admission<'a> {
    Queued {
        providers: &'a Value,
        resource_snapshot_id: &'a str,
    },
    Stranded {
        explanation: &'a str,
    },
}

impl Database {
    /// Records an admitted event and its initial outcome, or returns the receipt of
    /// the turn a previous submission of the same event already created.
    /// A published capture of the event's attachments is referenced only by
    /// the turn this call creates; a duplicate receipt leaves it unreferenced.
    pub(crate) fn accept_event(
        &self,
        interface_id: &str,
        runtime_id: &str,
        event: &ExternalEvent,
        admission: Admission<'_>,
        capture: Option<&Capture>,
    ) -> Result<EventReceipt> {
        let mut connection = self.connect()?;
        let transaction = write_transaction(&mut connection)?;
        if let Some(receipt) = event_receipt(&transaction, interface_id, &event.event_id)? {
            transaction.commit()?;
            return Ok(receipt);
        }

        if let Some(owner) = conversation_runtime(
            &transaction,
            interface_id,
            &event.agent_id,
            &event.audience_id,
        )? && owner != runtime_id
        {
            bail!("conversation is owned by runtime {owner}, not {runtime_id}");
        }
        let (providers, resource_snapshot_id, notice) = match admission {
            Admission::Queued {
                providers,
                resource_snapshot_id,
            } => (
                serde_json::to_string(providers)?,
                resource_snapshot_id.to_owned(),
                None,
            ),
            Admission::Stranded { explanation } => {
                let (providers, resources) = transaction.query_row(
                    "SELECT provider_snapshot_json, resource_snapshot_id FROM turns
                     WHERE interface_id = ?1 AND agent_id = ?2 AND audience_id = ?3
                     ORDER BY acceptance_sequence LIMIT 1",
                    params![interface_id, event.agent_id, event.audience_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )?;
                (providers, resources, Some(explanation))
            }
        };
        let now = timestamp()?;
        let turn_id = uuid::Uuid::now_v7().to_string();
        transaction.execute(
            "INSERT INTO turns(id, interface_id, event_id, agent_id, audience_id, user_id,
                 target_id, feedback_target_id, input, native_json, runtime_id,
                 provider_snapshot_json, resource_snapshot_id, state, accepted_at,
                 origin_turn_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, 'queued', ?14, ?1)",
            params![
                turn_id,
                interface_id,
                event.event_id,
                event.agent_id,
                event.audience_id,
                event.user_id,
                event.delivery_target_id,
                event.feedback_target_id,
                event.content,
                serde_json::to_string(&event.native)?,
                runtime_id,
                providers,
                resource_snapshot_id,
                now,
            ],
        )?;
        if let Some(capture) = capture {
            reference_capture(&transaction, event, &turn_id, capture)?;
        }
        let state = if let Some(notice) = notice {
            transaction.execute(
                "UPDATE turns SET state = 'interrupted', error = ?2, finished_at = ?3
                 WHERE id = ?1",
                params![turn_id, notice, now],
            )?;
            transaction.execute(
                "INSERT INTO deliveries(turn_id, interface_id, target_id, feedback_target_id,
                     content, state, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6)",
                params![
                    turn_id,
                    interface_id,
                    event.delivery_target_id,
                    event.feedback_target_id,
                    notice,
                    now,
                ],
            )?;
            TurnState::Interrupted
        } else {
            TurnState::Queued
        };
        transaction.commit()?;
        Ok(EventReceipt {
            event_id: event.event_id.clone(),
            turn_id,
            state,
            duplicate: false,
        })
    }
}

fn reference_capture(
    transaction: &rusqlite::Transaction<'_>,
    event: &ExternalEvent,
    turn_id: &str,
    capture: &Capture,
) -> Result<()> {
    let files = event
        .attachments
        .as_ref()
        .map(|staged| staged.files.as_slice())
        .unwrap_or_default();
    if files.len() != capture.digests.len() {
        bail!("captured attachments do not match the event");
    }
    for (ordinal, (file, digest)) in files.iter().zip(&capture.digests).enumerate() {
        transaction.execute(
            "INSERT INTO turn_attachments(turn_id, ordinal, capture_id, name, size,
                 content_type, sha256)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                turn_id,
                u32::try_from(ordinal)?,
                capture.id,
                file.name,
                i64::try_from(file.size)?,
                file.content_type,
                digest,
            ],
        )?;
    }
    Ok(())
}
