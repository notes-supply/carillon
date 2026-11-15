//! What a view shows an interface of its conversations, as storage has it:
//! the list, and one open conversation's newest turns. One transaction reads
//! all of it, so every part of a snapshot is of the same moment. Nothing
//! here writes.

use anyhow::bail;
use carillon_protocol::{
    conversations::TurnSource,
    model::{TurnActivity, TurnState},
    observation::{
        Conversation, ConversationKey, Ending, ObservedTurn, OpenView, Opened, Progress, Snapshot,
        View,
    },
    turns::Silence,
};
use rusqlite::{Connection, Row};

use super::{
    CapturedFile, Database, OptionalExtension, RUNTIME_EVENT_PREFIX, Result, answer_files,
    captured_files, optional_json_column, optional_state_column, params, state_column,
};

/// A running turn and the runtime that holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HeldTurn {
    pub(crate) turn_id: String,
    pub(crate) runtime_id: String,
}

/// What [`conversation`] reads of a conversation `c` of the interface `?1`.
pub(super) const CONVERSATION_COLUMNS: &str =
    "agent_id, audience_id, channel_id, title, started_at, active_at, activity,
         (SELECT COUNT(*) FROM turns INDEXED BY turns_interface_unfinished
          WHERE interface_id = ?1 AND state IN ('queued', 'running')
            AND agent_id = c.agent_id AND audience_id = c.audience_id)";

/// The newest `?4` turns of the conversation of agent `?2` with audience
/// `?3`: the window both reads of an open conversation take.
const WINDOW: &str = "FROM turns WHERE interface_id = ?1 AND agent_id = ?2 AND audience_id = ?3
     ORDER BY acceptance_sequence DESC LIMIT ?4";

impl Database {
    /// The running turns among the newest turns `open` shows of the
    /// interface's conversation, oldest first.
    pub(crate) fn running_in_view(
        &self,
        interface_id: &str,
        open: &OpenView,
    ) -> Result<Vec<HeldTurn>> {
        let key = &open.conversation;
        let running = self
            .read()?
            .prepare(&format!(
                "SELECT id, runtime_id FROM (SELECT id, runtime_id, state, acceptance_sequence {WINDOW})
                 WHERE state = 'running' ORDER BY acceptance_sequence"
            ))?
            .query_map(
                params![
                    interface_id,
                    key.agent_id(),
                    key.audience_id(),
                    open.turns.get()
                ],
                |row| {
                    Ok(HeldTurn {
                        turn_id: row.get(0)?,
                        runtime_id: row.get(1)?,
                    })
                },
            )?
            .collect::<rusqlite::Result<_>>()?;
        Ok(running)
    }

    /// Everything `view` shows the interface, and the running turns it
    /// lists, oldest first. A running turn is listed without activity:
    /// that is its runtime's to say.
    pub(crate) fn observed(
        &self,
        interface_id: &str,
        view: &View,
    ) -> Result<(Snapshot, Vec<HeldTurn>)> {
        let mut connection = self.read()?;
        let transaction = connection.transaction()?;
        let limit = view.conversations.get();
        let mut conversations = transaction
            .prepare(&format!(
                "SELECT {CONVERSATION_COLUMNS} FROM conversations c
                 WHERE interface_id = ?1 ORDER BY activity DESC LIMIT ?2"
            ))?
            .query_map(params![interface_id, u32::from(limit) + 1], conversation)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let more = conversations.len() > usize::from(limit);
        conversations.truncate(limit.into());
        let (open, running) = match &view.open {
            Some(open) => {
                let (opened, running) = opened(&transaction, interface_id, open)?;
                (Some(opened), running)
            }
            None => (None, Vec::new()),
        };
        Ok((
            Snapshot {
                conversations,
                more,
                open,
            },
            running,
        ))
    }
}

fn conversation(row: &Row<'_>) -> rusqlite::Result<Conversation> {
    let key = ConversationKey::new(row.get(0)?, row.get(1)?).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })?;
    Ok(Conversation {
        key,
        channel_id: row.get(2)?,
        title: row.get(3)?,
        started_at: row.get(4)?,
        active_at: row.get(5)?,
        activity: row.get(6)?,
        at_work: row.get(7)?,
    })
}

/// Where a turn with `event_id` came from: a runtime's opening, whose
/// `input` is its label, or a person's message, whose `input` is its text.
/// A message's files are read apart.
pub(super) fn source(event_id: String, input: String, command: bool) -> TurnSource {
    if event_id.starts_with(RUNTIME_EVENT_PREFIX) {
        TurnSource::Opened {
            label: Some(input).filter(|label| !label.is_empty()),
        }
    } else {
        TurnSource::Message {
            event_id,
            text: input,
            command,
            files: Vec::new(),
        }
    }
}

/// A turn's row of the open window.
struct Stored {
    turn_id: String,
    source: TurnSource,
    state: TurnState,
    accepted_at: i64,
    finished_at: Option<i64>,
    report: Option<TurnActivity>,
    runtime_id: String,
    /// What its delivery says, and the silence in place of it.
    delivered: Option<(String, Option<Silence>)>,
}

impl Stored {
    /// The turn as far as it has come, and its holder while it runs. A row
    /// that `db.rs`'s invariants rule out is a fault of storage, never a
    /// kind of progress.
    fn observed(self) -> Result<(ObservedTurn, Option<HeldTurn>)> {
        let report = self.report;
        let ended = |finished_at, ending| Progress::Ended {
            finished_at,
            ending,
            report,
        };
        let progress = match (self.state, self.finished_at, self.delivered) {
            (TurnState::Queued, None, None) => Progress::Queued,
            (TurnState::Running, None, None) => Progress::Running { activity: None },
            (TurnState::Completed, Some(at), Some((text, None))) => {
                let files = Vec::new();
                ended(at, Ending::Answered { text, files })
            }
            (TurnState::Interrupted, Some(at), Some((notice, None))) => {
                ended(at, Ending::Interrupted { notice })
            }
            (TurnState::Completed, Some(at), Some((_, Some(Silence::Steered)))) => {
                ended(at, Ending::Steered)
            }
            (TurnState::Interrupted, Some(at), Some((_, Some(Silence::Stopped)))) => {
                ended(at, Ending::Stopped)
            }
            (state, finished_at, delivered) => bail!(
                "turn {} is stored {} with {} and {}",
                self.turn_id,
                state.as_str(),
                finished_at.map_or("no time it ended", |_| "a time it ended"),
                match delivered {
                    None => "no delivery",
                    Some((_, None)) => "a delivery",
                    Some((_, Some(Silence::Steered))) => "a steered delivery",
                    Some((_, Some(Silence::Stopped))) => "a stopped delivery",
                }
            ),
        };
        let running = matches!(progress, Progress::Running { .. }).then(|| HeldTurn {
            turn_id: self.turn_id.clone(),
            runtime_id: self.runtime_id,
        });
        let turn = ObservedTurn {
            turn_id: self.turn_id,
            source: self.source,
            accepted_at: self.accepted_at,
            progress,
        };
        Ok((turn, running))
    }
}

/// The interface's conversation `open` names, with the newest turns it
/// asks for and those of them that run.
fn opened(
    connection: &Connection,
    interface_id: &str,
    open: &OpenView,
) -> Result<(Opened, Vec<HeldTurn>)> {
    let key = &open.conversation;
    let Some(conversation) = connection
        .query_row(
            &format!(
                "SELECT {CONVERSATION_COLUMNS} FROM conversations c
                 WHERE interface_id = ?1 AND agent_id = ?2 AND audience_id = ?3"
            ),
            params![interface_id, key.agent_id(), key.audience_id()],
            conversation,
        )
        .optional()?
    else {
        return Ok((Opened::Unrecorded { key: key.clone() }, Vec::new()));
    };
    let limit = open.turns.get();
    let mut stored = connection
        .prepare(&format!(
            "SELECT t.id, t.event_id, t.input,
                 COALESCE(json_extract(t.native_json, '$.intent') = 'command', 0),
                 t.state, t.accepted_at, t.finished_at, t.activity_json, t.runtime_id,
                 d.content, d.silence
             FROM (SELECT * {WINDOW}) t LEFT JOIN deliveries d ON d.turn_id = t.id
             ORDER BY t.acceptance_sequence DESC"
        ))?
        .query_map(
            params![
                interface_id,
                key.agent_id(),
                key.audience_id(),
                u32::from(limit) + 1
            ],
            |row| {
                let delivered = row
                    .get::<_, Option<String>>(9)?
                    .map(|content| {
                        Ok::<_, rusqlite::Error>((content, optional_state_column(row, 10)?))
                    })
                    .transpose()?;
                Ok(Stored {
                    turn_id: row.get(0)?,
                    source: source(row.get(1)?, row.get(2)?, row.get(3)?),
                    state: state_column(row, 4)?,
                    accepted_at: row.get(5)?,
                    finished_at: row.get(6)?,
                    report: optional_json_column(row, 7)?,
                    runtime_id: row.get(8)?,
                    delivered,
                })
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let earlier = stored.len() > usize::from(limit);
    stored.truncate(limit.into());
    let (mut turns, mut running) = (Vec::with_capacity(stored.len()), Vec::new());
    for stored in stored.into_iter().rev() {
        let (mut turn, holder) = stored.observed()?;
        if let TurnSource::Message { files, .. } = &mut turn.source {
            *files = captured_files(connection, Some(&turn.turn_id))?
                .iter()
                .map(CapturedFile::wire)
                .collect();
        }
        if let Progress::Ended {
            ending: Ending::Answered { files, .. },
            ..
        } = &mut turn.progress
        {
            *files = answer_files(connection, &turn.turn_id)?;
        }
        turns.push(turn);
        running.extend(holder);
    }
    Ok((
        Opened::Recorded {
            conversation,
            turns,
            earlier,
        },
        running,
    ))
}
