//! An interface's conversations and their turns, read back from what core
//! records. Nothing here writes.
//!
//! The list reads `conversations` in the order of `conversations_activity`
//! and counts turns at work in `turns_interface_unfinished`, which covers
//! the count. It reads no turn row and no message text.

use carillon_protocol::conversations::{
    ConversationPage, ConversationSummary, HISTORY_PAGE_LIMIT, Transcript, TranscriptAnswer,
    TranscriptTurn, TurnSource,
};

use super::{
    CapturedFile, Database, OptionalExtension, RUNTIME_EVENT_PREFIX, Result, answer_files,
    captured_files, optional_json_column, optional_state_column, params, state_column,
};

impl Database {
    /// A page of the interface's conversations, the latest active first: at
    /// most [`HISTORY_PAGE_LIMIT`] whose activity marker is below `before`,
    /// and only those with `audience_id` when one is given. An audience
    /// names one conversation for each agent it has talked to.
    pub(crate) fn conversations(
        &self,
        interface_id: &str,
        before: Option<i64>,
        audience_id: Option<&str>,
    ) -> Result<ConversationPage> {
        let connection = self.connect()?;
        let mut conversations = connection
            .prepare(
                "SELECT agent_id, audience_id, channel_id, title, started_at, active_at, activity,
                     (SELECT COUNT(*) FROM turns INDEXED BY turns_interface_unfinished
                      WHERE interface_id = ?1 AND state IN ('queued', 'running')
                        AND agent_id = c.agent_id AND audience_id = c.audience_id)
                 FROM conversations c
                 WHERE interface_id = ?1 AND activity < ?2
                   AND (?4 IS NULL OR audience_id = ?4)
                 ORDER BY activity DESC LIMIT ?3",
            )?
            .query_map(
                params![
                    interface_id,
                    before.unwrap_or(i64::MAX),
                    i64::try_from(HISTORY_PAGE_LIMIT + 1)?,
                    audience_id
                ],
                |row| {
                    Ok(ConversationSummary {
                        agent_id: row.get(0)?,
                        audience_id: row.get(1)?,
                        channel_id: row.get(2)?,
                        title: row.get(3)?,
                        started_at: row.get(4)?,
                        active_at: row.get(5)?,
                        activity: row.get(6)?,
                        at_work: row.get(7)?,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let more = conversations.len() > HISTORY_PAGE_LIMIT;
        conversations.truncate(HISTORY_PAGE_LIMIT);
        let next = conversations
            .last()
            .filter(|_| more)
            .map(|last| last.activity);
        Ok(ConversationPage {
            conversations,
            next,
        })
    }

    /// A page of the turns of the interface's conversation of `agent_id`
    /// with `audience_id`, oldest first: its newest
    /// [`HISTORY_PAGE_LIMIT`], or those before the turn `before`. A
    /// conversation core has no turn of reads as empty, and so does a page
    /// before a turn that is not the conversation's.
    pub(crate) fn transcript(
        &self,
        interface_id: &str,
        agent_id: &str,
        audience_id: &str,
        before: Option<&str>,
    ) -> Result<Transcript> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction()?;
        let before = match before {
            None => i64::MAX,
            Some(turn_id) => transaction
                .query_row(
                    "SELECT acceptance_sequence FROM turns
                     WHERE id = ?1 AND interface_id = ?2 AND agent_id = ?3 AND audience_id = ?4",
                    params![turn_id, interface_id, agent_id, audience_id],
                    |row| row.get(0),
                )
                .optional()?
                .unwrap_or(0),
        };
        let mut turns = transaction
            .prepare(
                "SELECT t.id, t.event_id, t.input,
                     COALESCE(json_extract(t.native_json, '$.intent') = 'command', 0),
                     t.state, t.accepted_at, t.finished_at, t.activity_json,
                     d.content, d.silence, d.state
                 FROM turns t LEFT JOIN deliveries d ON d.turn_id = t.id
                 WHERE t.interface_id = ?1 AND t.agent_id = ?2 AND t.audience_id = ?3
                   AND t.acceptance_sequence < ?4
                 ORDER BY t.acceptance_sequence DESC LIMIT ?5",
            )?
            .query_map(
                params![
                    interface_id,
                    agent_id,
                    audience_id,
                    before,
                    i64::try_from(HISTORY_PAGE_LIMIT + 1)?
                ],
                |row| {
                    let (event_id, input): (String, String) = (row.get(1)?, row.get(2)?);
                    let source = if event_id.starts_with(RUNTIME_EVENT_PREFIX) {
                        TurnSource::Opened {
                            label: Some(input).filter(|label| !label.is_empty()),
                        }
                    } else {
                        TurnSource::Message {
                            event_id,
                            text: input,
                            command: row.get(3)?,
                            files: Vec::new(),
                        }
                    };
                    let answer = row
                        .get::<_, Option<String>>(8)?
                        .map(|content| {
                            Ok::<_, rusqlite::Error>(TranscriptAnswer {
                                content,
                                silence: optional_state_column(row, 9)?,
                                files: Vec::new(),
                                delivery: state_column(row, 10)?,
                            })
                        })
                        .transpose()?;
                    Ok(TranscriptTurn {
                        turn_id: row.get(0)?,
                        source,
                        state: state_column(row, 4)?,
                        accepted_at: row.get(5)?,
                        finished_at: row.get(6)?,
                        report: optional_json_column(row, 7)?,
                        answer,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let earlier = turns.len() > HISTORY_PAGE_LIMIT;
        turns.truncate(HISTORY_PAGE_LIMIT);
        turns.reverse();
        for turn in &mut turns {
            if let TurnSource::Message { files, .. } = &mut turn.source {
                *files = captured_files(&transaction, Some(&turn.turn_id))?
                    .iter()
                    .map(CapturedFile::wire)
                    .collect();
            }
            if let Some(answer) = &mut turn.answer {
                answer.files = answer_files(&transaction, &turn.turn_id)?;
            }
        }
        Ok(Transcript { turns, earlier })
    }
}
