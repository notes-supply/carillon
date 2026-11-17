//! What a conversation's turns come from, and the title core gives a
//! conversation. An interface sees both in the snapshots of
//! [`observation`](crate::observation).

use serde::{Deserialize, Serialize};

use crate::model::TurnAttachment;

/// The longest title core derives, in characters.
pub const TITLE_LENGTH: usize = 80;

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
