//! Each conversation's own facts. Its channel, title and start are written
//! once, with its first admitted message. Its activity marker rises with
//! every turn of it that is accepted, opened or ended, so its interface's
//! conversations order by it and a reader that saw it unchanged missed
//! nothing.

use carillon_protocol::conversations::TITLE_LENGTH;
use rusqlite::Connection;

use super::{Database, ExternalEvent, OptionalExtension, Result, params};

/// The marker above every one the conversations of interface `?1` have.
const NEXT_ACTIVITY: &str =
    "(SELECT COALESCE(MAX(activity), 0) + 1 FROM conversations WHERE interface_id = ?1)";

/// Records the turn `event` became as its conversation's latest activity.
/// The first message of a conversation also gives it its channel and title,
/// which stand from then on.
pub(super) fn accepted(
    connection: &Connection,
    interface_id: &str,
    event: &ExternalEvent,
    now: i64,
) -> Result<()> {
    connection.execute(
        &format!(
            "INSERT INTO conversations(interface_id, agent_id, audience_id, channel_id, title,
                 started_at, activity, active_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, {NEXT_ACTIVITY}, ?6)
             ON CONFLICT(interface_id, agent_id, audience_id) DO UPDATE
             SET activity = excluded.activity, active_at = excluded.active_at"
        ),
        params![
            interface_id,
            event.agent_id,
            event.audience_id,
            event.channel_id,
            title(&event.content),
            now
        ],
    )?;
    Ok(())
}

/// Records that a turn of the conversation was opened or ended at `now`.
pub(super) fn active(
    connection: &Connection,
    interface_id: &str,
    agent_id: &str,
    audience_id: &str,
    now: i64,
) -> Result<()> {
    connection.execute(
        &format!(
            "UPDATE conversations SET activity = {NEXT_ACTIVITY}, active_at = ?4
             WHERE interface_id = ?1 AND agent_id = ?2 AND audience_id = ?3"
        ),
        params![interface_id, agent_id, audience_id, now],
    )?;
    Ok(())
}

/// The first line of `text` that says anything, at most [`TITLE_LENGTH`]
/// characters of it.
fn title(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .chars()
        .take(TITLE_LENGTH)
        .collect()
}

impl Database {
    /// The channel recorded for a conversation, if it has had a turn.
    pub(crate) fn recorded_channel(
        &self,
        interface_id: &str,
        agent_id: &str,
        audience_id: &str,
    ) -> Result<Option<String>> {
        Ok(channel(&self.connect()?, interface_id, agent_id, audience_id).optional()?)
    }
}

/// The channel recorded for a conversation that has a turn.
pub(super) fn channel(
    connection: &Connection,
    interface_id: &str,
    agent_id: &str,
    audience_id: &str,
) -> rusqlite::Result<String> {
    connection.query_row(
        "SELECT channel_id FROM conversations
         WHERE interface_id = ?1 AND agent_id = ?2 AND audience_id = ?3",
        params![interface_id, agent_id, audience_id],
        |row| row.get(0),
    )
}
