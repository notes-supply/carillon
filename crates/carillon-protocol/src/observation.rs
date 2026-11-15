//! What an interface observes of its own conversations: the view it asks
//! for and the complete snapshots core answers with. A snapshot is
//! everything the view shows, so a reader replaces what it had with each
//! one and merges nothing. Core takes the interface from the caller's
//! credential; no type here names one.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{
    conversations::TurnSource,
    model::{AnswerFile, TurnActivity},
};

/// The most bytes one snapshot takes, serialized. Core sends no larger one
/// and a reader accepts none. A realistic window of [`Limit::MAX`] turns
/// takes a sixth of it.
pub const SNAPSHOT_LIMIT: usize = 16 * 1024 * 1024;

/// The name of the event that carries a snapshot on the stream.
pub const SNAPSHOT_EVENT: &str = "snapshot";

/// How long core lets an observation stay silent before it writes a
/// keep-alive comment. A reader gives up on one silent for three of these.
pub const KEEP_ALIVE: Duration = Duration::from_secs(10);

/// The most bytes either part of a [`ConversationKey`] takes.
pub const KEY_PART_LIMIT: usize = 1024;

/// Why a view cannot be asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InvalidView {
    #[error("a window holds 1 to {} items", Limit::MAX)]
    Limit,
    #[error(
        "a conversation is named by its agent and its audience, each of 1 to {KEY_PART_LIMIT} bytes"
    )]
    Key,
    #[error("a window of turns needs the conversation they belong to")]
    TurnsWithoutConversation,
}

/// How many items a window holds: 1 to [`Limit::MAX`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "u16", into = "u16")]
pub struct Limit(u16);

impl Limit {
    pub const MAX: u16 = 500;

    /// # Errors
    ///
    /// Returns [`InvalidView::Limit`] for 0 and for more than [`Self::MAX`].
    pub const fn new(items: u16) -> Result<Self, InvalidView> {
        if items == 0 || items > Self::MAX {
            return Err(InvalidView::Limit);
        }
        Ok(Self(items))
    }

    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

impl Default for Limit {
    fn default() -> Self {
        Self(50)
    }
}

impl TryFrom<u16> for Limit {
    type Error = InvalidView;

    fn try_from(items: u16) -> Result<Self, InvalidView> {
        Self::new(items)
    }
}

impl From<Limit> for u16 {
    fn from(limit: Limit) -> Self {
        limit.0
    }
}

/// One conversation of an interface, which its agent and its audience name
/// together. Neither part is empty.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(try_from = "KeyParts", into = "KeyParts")]
pub struct ConversationKey {
    agent_id: String,
    audience_id: String,
}

#[derive(Deserialize, Serialize)]
struct KeyParts {
    agent_id: String,
    audience_id: String,
}

impl ConversationKey {
    /// # Errors
    ///
    /// Returns [`InvalidView::Key`] when a part is empty or longer than
    /// [`KEY_PART_LIMIT`].
    pub fn new(agent_id: String, audience_id: String) -> Result<Self, InvalidView> {
        let named = |part: &str| !part.is_empty() && part.len() <= KEY_PART_LIMIT;
        if !named(&agent_id) || !named(&audience_id) {
            return Err(InvalidView::Key);
        }
        Ok(Self {
            agent_id,
            audience_id,
        })
    }

    #[must_use]
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    #[must_use]
    pub fn audience_id(&self) -> &str {
        &self.audience_id
    }
}

impl TryFrom<KeyParts> for ConversationKey {
    type Error = InvalidView;

    fn try_from(parts: KeyParts) -> Result<Self, InvalidView> {
        Self::new(parts.agent_id, parts.audience_id)
    }
}

impl From<ConversationKey> for KeyParts {
    fn from(key: ConversationKey) -> Self {
        Self {
            agent_id: key.agent_id,
            audience_id: key.audience_id,
        }
    }
}

/// What an interface looks at: its conversations, the latest active first,
/// and at most one of them open. It travels as a query string,
/// `conversations=<n>[&agent_id=<agent>&audience_id=<audience>[&turns=<n>]]`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "ViewQuery", into = "ViewQuery")]
pub struct View {
    pub conversations: Limit,
    /// The conversation on screen, listed or not.
    pub open: Option<OpenView>,
}

/// The open conversation and how many of its newest turns to show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenView {
    pub conversation: ConversationKey,
    pub turns: Limit,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ViewQuery {
    conversations: Option<Limit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    audience_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    turns: Option<Limit>,
}

impl TryFrom<ViewQuery> for View {
    type Error = InvalidView;

    fn try_from(query: ViewQuery) -> Result<Self, InvalidView> {
        let open = match (query.agent_id, query.audience_id, query.turns) {
            (Some(agent_id), Some(audience_id), turns) => Some(OpenView {
                conversation: ConversationKey::new(agent_id, audience_id)?,
                turns: turns.unwrap_or_default(),
            }),
            (None, None, None) => None,
            (None, None, Some(_)) => return Err(InvalidView::TurnsWithoutConversation),
            (Some(_), None, _) | (None, Some(_), _) => return Err(InvalidView::Key),
        };
        Ok(Self {
            conversations: query.conversations.unwrap_or_default(),
            open,
        })
    }
}

impl From<View> for ViewQuery {
    fn from(view: View) -> Self {
        let mut query = Self {
            conversations: Some(view.conversations),
            agent_id: None,
            audience_id: None,
            turns: None,
        };
        if let Some(open) = view.open {
            query.agent_id = Some(open.conversation.agent_id);
            query.audience_id = Some(open.conversation.audience_id);
            query.turns = Some(open.turns);
        }
        query
    }
}

/// Everything a view shows.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Snapshot {
    /// The latest active first, at most as many as the view asked for.
    pub conversations: Vec<Conversation>,
    /// The interface has more conversations than are listed.
    pub more: bool,
    /// Present exactly when the view opened a conversation.
    pub open: Option<Opened>,
}

/// A conversation as core records it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Conversation {
    pub key: ConversationKey,
    /// The channel recorded with the conversation's first message.
    pub channel_id: String,
    /// The first non-blank line of the conversation's first message,
    /// trimmed. Empty when that message had only files.
    pub title: String,
    /// When core accepted the conversation's first message, Unix seconds.
    pub started_at: i64,
    /// When `activity` last rose, Unix seconds.
    pub active_at: i64,
    /// Rises whenever core accepts a message of the conversation, its
    /// runtime opens a turn in it, or one of its turns ends. No two
    /// conversations of an interface share one, and they are listed by it,
    /// the highest first.
    pub activity: i64,
    /// How many of the conversation's turns are queued or running.
    pub at_work: u32,
}

/// The conversation a view opened.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Opened {
    /// Core records no conversation of the caller under this key. Its first
    /// accepted message makes it `Recorded`.
    Unrecorded { key: ConversationKey },
    Recorded {
        conversation: Conversation,
        /// Its newest turns, oldest first, at most as many as the view
        /// asked for.
        turns: Vec<ObservedTurn>,
        /// The conversation has turns older than those listed.
        earlier: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ObservedTurn {
    pub turn_id: String,
    pub source: TurnSource,
    /// Unix seconds.
    pub accepted_at: i64,
    pub progress: Progress,
}

/// How far a turn has come. An ended turn never changes again, so a reader
/// may keep what it made of one.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Progress {
    Queued,
    Running {
        /// What the runtime said it was doing when core last asked, or
        /// `null` when it gave no answer. A turn without one has not ended.
        /// A turn the runtime opened offers no controls here.
        #[serde(deserialize_with = "crate::fields::required_nullable")]
        activity: Option<TurnActivity>,
    },
    Ended {
        /// Unix seconds.
        finished_at: i64,
        ending: Ending,
        /// The final report the runtime gave with the outcome.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        report: Option<TurnActivity>,
    },
}

/// What core recorded for the conversation when a turn ended.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Ending {
    Answered {
        text: String,
        /// The files core keeps with the answer, in the order the agent
        /// attached them.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        files: Vec<AnswerFile>,
    },
    /// The turn did not complete. `notice` says so to the conversation.
    Interrupted { notice: String },
    /// The message went into a turn already running, whose answer covers it.
    Steered,
    /// Someone stopped the turn.
    Stopped,
}
