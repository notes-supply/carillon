//! What a view shows an interface of its conversations, as storage has it:
//! the list, and one open conversation's newest turns. One transaction reads
//! all of it, so every part of a snapshot is of the same moment. Nothing
//! here writes.

use std::io;

use anyhow::bail;
use carillon_protocol::{
    conversations::TurnSource,
    model::{TurnActivity, TurnState},
    observation::{
        Conversation, ConversationKey, Ending, Limit, ObservedTurn, OpenView, Opened, Progress,
        SNAPSHOT_LIMIT, Snapshot, View,
    },
    turns::Silence,
};
use rusqlite::{Connection, Row};
use serde::Serialize;

use super::{
    CapturedFile, Database, OptionalExtension, RUNTIME_EVENT_PREFIX, Result, answer_files,
    captured_files, optional_json_column, optional_state_column, params, state_column,
};

#[derive(Debug, thiserror::Error)]
#[error(
    "this view's snapshot takes at least {0} bytes, over the {SNAPSHOT_LIMIT} a snapshot may take: ask for fewer turns"
)]
pub(crate) struct TooLarge(pub(crate) usize);

/// A snapshot's JSON, written part after part into `W`, which
/// [`io::Sink`] only counts. A write that takes it over [`SNAPSHOT_LIMIT`]
/// fails.
pub(crate) struct Bounded<W> {
    written: usize,
    into: W,
}

impl<W: io::Write> Bounded<W> {
    pub(crate) const fn new(into: W) -> Self {
        Self { written: 0, into }
    }

    /// Writes `part` after what was written before, or refuses it with
    /// [`TooLarge`] once the whole is over the limit.
    pub(crate) fn add(&mut self, part: &impl Serialize) -> Result<()> {
        serde_json::to_writer(&mut *self, part).map_err(|error| {
            if error.is_io() {
                TooLarge(self.written).into()
            } else {
                error.into()
            }
        })
    }

    pub(crate) fn into_inner(self) -> W {
        self.into
    }
}

impl<W: io::Write> io::Write for Bounded<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.written += bytes.len();
        if self.written > SNAPSHOT_LIMIT {
            return Err(io::ErrorKind::FileTooLarge.into());
        }
        self.into.write_all(bytes)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.into.flush()
    }
}

/// A running turn and the runtime that holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HeldTurn {
    pub(crate) turn_id: String,
    pub(crate) runtime_id: String,
}

/// What [`conversation`] reads of a conversation `c` of the interface `?1`.
const CONVERSATION_COLUMNS: &str =
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

    /// Everything `view` shows the interface. A running turn is listed
    /// without activity: that is its runtime's to say. Each part is counted
    /// as the snapshot writes it, so the read stops at the part that takes
    /// the snapshot over [`SNAPSHOT_LIMIT`].
    pub(crate) fn observed(&self, interface_id: &str, view: &View) -> Result<Snapshot> {
        let mut connection = self.read()?;
        let transaction = connection.transaction()?;
        let mut written = Bounded::new(io::sink());
        let (conversations, more) =
            listed(&transaction, interface_id, view.conversations, &mut written)?;
        let open = view
            .open
            .as_ref()
            .map(|open| opened(&transaction, interface_id, open, &mut written))
            .transpose()?;
        Ok(Snapshot {
            conversations,
            more,
            open,
        })
    }
}

/// The interface's latest active conversations, at most `limit`, and
/// whether it has more, each counted into `written` before the next is read.
fn listed(
    connection: &Connection,
    interface_id: &str,
    limit: Limit,
    written: &mut Bounded<io::Sink>,
) -> Result<(Vec<Conversation>, bool)> {
    let limit = limit.get();
    let mut statement = connection.prepare(&format!(
        "SELECT {CONVERSATION_COLUMNS} FROM conversations c
         WHERE interface_id = ?1 ORDER BY activity DESC LIMIT ?2"
    ))?;
    let mut rows = statement.query(params![interface_id, u32::from(limit) + 1])?;
    let mut conversations = Vec::new();
    while let Some(row) = rows.next()? {
        if conversations.len() == usize::from(limit) {
            return Ok((conversations, true));
        }
        let shown = conversation(row)?;
        written.add(&shown)?;
        conversations.push(shown);
    }
    Ok((conversations, false))
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
fn source(event_id: String, input: String, command: bool) -> TurnSource {
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

/// How far the turn `turn_id` has come: its stored `state`, when it ended,
/// and of its delivery what it says and the silence in place of it. A row
/// that `db.rs`'s invariants rule out is a fault of storage, never a kind
/// of progress.
fn progress(
    turn_id: &str,
    state: TurnState,
    finished_at: Option<i64>,
    delivered: Option<(String, Option<Silence>)>,
    report: Option<TurnActivity>,
) -> Result<Progress> {
    let ended = |finished_at, ending| Progress::Ended {
        finished_at,
        ending,
        report,
    };
    Ok(match (state, finished_at, delivered) {
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
            "turn {turn_id} is stored {} with {} and {}",
            state.as_str(),
            finished_at.map_or("no time it ended", |_| "a time it ended"),
            match delivered {
                None => "no delivery",
                Some((_, None)) => "a delivery",
                Some((_, Some(Silence::Steered))) => "a steered delivery",
                Some((_, Some(Silence::Stopped))) => "a stopped delivery",
            }
        ),
    })
}

/// The interface's conversation `open` names, with the newest turns it
/// asks for, each with its files, counted into `written` as it is read.
fn opened(
    connection: &Connection,
    interface_id: &str,
    open: &OpenView,
    written: &mut Bounded<io::Sink>,
) -> Result<Opened> {
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
        return Ok(Opened::Unrecorded { key: key.clone() });
    };
    written.add(&conversation)?;
    let limit = open.turns.get();
    let mut newest = connection.prepare(&format!(
        "SELECT t.id, t.event_id, t.input,
             COALESCE(json_extract(t.native_json, '$.intent') = 'command', 0),
             t.state, t.accepted_at, t.finished_at, t.activity_json, d.content, d.silence
         FROM (SELECT * {WINDOW}) t LEFT JOIN deliveries d ON d.turn_id = t.id
         ORDER BY t.acceptance_sequence DESC"
    ))?;
    let mut rows = newest.query(params![
        interface_id,
        key.agent_id(),
        key.audience_id(),
        u32::from(limit) + 1
    ])?;
    let (mut turns, mut earlier) = (Vec::new(), false);
    while let Some(row) = rows.next()? {
        if turns.len() == usize::from(limit) {
            earlier = true;
            break;
        }
        let turn_id: String = row.get(0)?;
        let delivered = match row.get::<_, Option<String>>(8)? {
            Some(content) => Some((content, optional_state_column(row, 9)?)),
            None => None,
        };
        let report = optional_json_column(row, 7)?;
        let progress = progress(
            &turn_id,
            state_column(row, 4)?,
            row.get(6)?,
            delivered,
            report,
        )?;
        let mut turn = ObservedTurn {
            turn_id,
            source: source(row.get(1)?, row.get(2)?, row.get(3)?),
            accepted_at: row.get(5)?,
            progress,
        };
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
        written.add(&turn)?;
        turns.push(turn);
    }
    turns.reverse();
    Ok(Opened::Recorded {
        conversation,
        turns,
        earlier,
    })
}
