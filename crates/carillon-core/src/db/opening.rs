use carillon_protocol::{
    model::{EventReceipt, TurnState},
    turns::TurnOpening,
};

use super::{
    Database, OptionalExtension, Result, event_receipt, params, timestamp, write_transaction,
};

/// How the event identities of turns runtimes open begin, followed by the
/// runtime's ID and the opening's key. Interfaces may not use them: their
/// events are refused at admission.
pub(crate) const RUNTIME_EVENT_PREFIX: &str = "runtime:";

/// What opening a turn found.
#[derive(Debug)]
pub(crate) enum Opened {
    Receipt(EventReceipt),
    /// The agent runs on another runtime now, or no longer exists.
    Moved,
    /// The conversation has no turn yet.
    NoConversation,
    /// The conversation's latest turn is another runtime's.
    OwnedBy(String),
}

impl Database {
    /// Returns the receipt of the turn the same key opened before, or else
    /// records a running turn `runtime_id` opened in a conversation whose
    /// latest turn is its own and whose agent runs there now, as
    /// `agent_runtime` says, with that turn's target and snapshots and
    /// nothing a person sent.
    pub(crate) fn open_turn(
        &self,
        runtime_id: &str,
        agent_runtime: Option<&str>,
        opening: &TurnOpening,
    ) -> Result<Opened> {
        let event_id = format!("{RUNTIME_EVENT_PREFIX}{runtime_id}:{}", opening.key);
        let mut connection = self.connect()?;
        let transaction = write_transaction(&mut connection)?;
        if let Some(receipt) = event_receipt(&transaction, &opening.interface_id, &event_id)? {
            transaction.commit()?;
            return Ok(Opened::Receipt(receipt));
        }
        if agent_runtime != Some(runtime_id) {
            return Ok(Opened::Moved);
        }
        let latest = transaction
            .query_row(
                "SELECT id, runtime_id FROM turns
                 WHERE interface_id = ?1 AND agent_id = ?2 AND audience_id = ?3
                 ORDER BY acceptance_sequence DESC LIMIT 1",
                params![opening.interface_id, opening.agent_id, opening.audience_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        let latest = match latest {
            None => return Ok(Opened::NoConversation),
            Some((_, owner)) if owner != runtime_id => return Ok(Opened::OwnedBy(owner)),
            Some((latest, _)) => latest,
        };
        let turn_id = uuid::Uuid::now_v7().to_string();
        transaction.execute(
            "INSERT INTO turns(id, interface_id, event_id, agent_id, audience_id, user_id,
                 target_id, feedback_target_id, input, native_json, runtime_id,
                 provider_snapshot_json, resource_snapshot_id, state, accepted_at,
                 origin_turn_id)
             SELECT ?2, interface_id, ?3, agent_id, audience_id, '', target_id, '', ?4, '{}',
                 runtime_id, provider_snapshot_json, resource_snapshot_id, 'running', ?5, ?2
             FROM turns WHERE id = ?1",
            params![
                latest,
                turn_id,
                event_id,
                opening.label.as_deref().unwrap_or_default(),
                timestamp()?,
            ],
        )?;
        transaction.commit()?;
        Ok(Opened::Receipt(EventReceipt {
            event_id,
            turn_id,
            state: TurnState::Running,
            duplicate: false,
        }))
    }
}
