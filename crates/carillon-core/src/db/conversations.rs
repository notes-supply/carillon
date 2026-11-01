//! Each conversation's channel, as its interface named it with the
//! conversation's first admitted message.

use rusqlite::Connection;

use super::{ExternalEvent, Result, params};

/// Records the channel `event` names as its conversation's, unless the
/// conversation has one already: the first record stands.
pub(super) fn record_channel(
    connection: &Connection,
    interface_id: &str,
    event: &ExternalEvent,
) -> Result<()> {
    connection.execute(
        "INSERT INTO conversations(interface_id, agent_id, audience_id, channel_id)
         VALUES (?1, ?2, ?3, ?4) ON CONFLICT DO NOTHING",
        params![
            interface_id,
            event.agent_id,
            event.audience_id,
            event.channel_id
        ],
    )?;
    Ok(())
}

/// The channel recorded for a conversation that has a turn.
pub(super) fn channel(
    connection: &Connection,
    interface_id: &str,
    agent_id: &str,
    audience_id: &str,
) -> Result<String> {
    Ok(connection.query_row(
        "SELECT channel_id FROM conversations
         WHERE interface_id = ?1 AND agent_id = ?2 AND audience_id = ?3",
        params![interface_id, agent_id, audience_id],
        |row| row.get(0),
    )?)
}
