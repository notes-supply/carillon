//! An interface's read of its own conversations. Core keeps every turn, its
//! files, its answer and how the answer's delivery stands; these types let
//! the interface that owns a conversation read them back, so it need keep no
//! transcript of its own. Core scopes every read to the authenticated
//! interface. Only Rust reads these types.

use serde::{Deserialize, Serialize};

use crate::{
    model::{AnswerFile, DeliveryState, TurnActivity, TurnAttachment, TurnState},
    turns::Silence,
};

/// The most conversations one page lists, and the most turns one page of a
/// transcript holds.
pub const HISTORY_PAGE_LIMIT: usize = 50;

/// The longest title core derives, in characters.
pub const TITLE_LENGTH: usize = 80;

/// One page of an interface's conversations, the latest active first.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ConversationPage {
    pub conversations: Vec<ConversationSummary>,
    /// More conversations follow: read them with `before` set to this, the
    /// `activity` of the last one listed. A conversation that becomes
    /// active between two reads moves to the first page, and is on no later
    /// one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<i64>,
}

/// A conversation of the calling interface, which `agent_id` and
/// `audience_id` identify together.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ConversationSummary {
    pub agent_id: String,
    pub audience_id: String,
    /// The channel recorded with the conversation's first message.
    pub channel_id: String,
    /// The first non-blank line of the conversation's first message,
    /// trimmed, at most [`TITLE_LENGTH`] characters. Empty when that message
    /// had only files.
    pub title: String,
    /// When core accepted the conversation's first message, Unix seconds.
    pub started_at: i64,
    /// When `activity` last rose, Unix seconds.
    pub active_at: i64,
    /// Rises whenever core accepts a message of the conversation, its
    /// runtime opens a turn in it, or one of its turns ends. An interface's
    /// conversations are listed by it, the highest first, and no two share
    /// one. While it and `at_work` stay as they were, the conversation's
    /// transcript has no new turn and no new answer. A turn's state while it
    /// is unfinished, and an external answer's delivery state, are not
    /// covered: an interface that shows them reads the jobs of its
    /// unfinished turns.
    pub activity: i64,
    /// How many of the conversation's turns are queued or running.
    pub at_work: u32,
}

/// One page of a conversation's turns, oldest first.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Transcript {
    pub turns: Vec<TranscriptTurn>,
    /// Older turns exist: read them with `before` set to the first turn
    /// listed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub earlier: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TranscriptTurn {
    pub turn_id: String,
    pub source: TurnSource,
    pub state: TurnState,
    /// Unix seconds.
    pub accepted_at: i64,
    /// When the turn ended, Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,
    /// An ended turn's final report, as its runtime gave it with the
    /// outcome. A running turn's snapshot comes from a job read instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<TurnActivity>,
    /// What core recorded for the conversation when the turn ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<TranscriptAnswer>,
}

/// Where a turn came from.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnSource {
    /// A person's message, which the interface submitted as `event_id`.
    Message {
        event_id: String,
        /// What the person wrote, without the context the interface added
        /// for the model.
        text: String,
        /// The message invoked one of the runtime's commands.
        command: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        files: Vec<TurnAttachment>,
    },
    /// A turn the runtime opened itself, for a schedule or a subagent's
    /// report. No person or message began it.
    Opened {
        /// What the runtime said the turn is for.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
}

/// The answer or notice core recorded for a turn, as an interface delivers
/// it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TranscriptAnswer {
    /// Empty for a silence.
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub silence: Option<Silence>,
    /// The files core keeps with the answer, in the order the agent
    /// attached them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<AnswerFile>,
    pub delivery: DeliveryState,
}
