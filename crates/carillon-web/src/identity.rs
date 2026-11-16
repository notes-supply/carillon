//! How a web conversation and a message map onto core's identities.
//!
//! | Core field           | Web value                                   |
//! |----------------------|---------------------------------------------|
//! | `interface_id`       | the provider's ID, from its credential      |
//! | `audience_id`        | the conversation's UUID, which the page mints |
//! | `channel_id`         | the same UUID: a chat is its own settings subject |
//! | `delivery_target_id` | the same UUID, so a turn the runtime opens lands in the conversation |
//! | `agent_id`           | named by the page with every request        |
//! | `event_id`           | `m-<uuid>`, minted by the page once per send |
//! | `feedback_target_id` | the event ID                                |
//! | `user_id`            | the installing user's login name            |
//!
//! The provider stores none of it. A conversation is named by its agent and
//! its audience together, as core keys it, and exists in core from its
//! first admitted message.

use std::fmt;

use axum::http::StatusCode;
use carillon_interface::InterfaceError;
use carillon_protocol::{model::ConversationQuery, observation};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{browser::refusal, serve::Web};

/// Whether `text` is a UUID as the page writes one: lowercase with
/// hyphens, so one conversation has one name.
pub(crate) fn canonical(text: &str) -> bool {
    Uuid::try_parse(text).is_ok_and(|uuid| uuid.hyphenated().to_string() == text)
}

/// The UUID the page minted for a conversation: its audience, channel and
/// delivery target in core.
#[derive(Clone, Deserialize, Serialize)]
#[serde(try_from = "String")]
pub(crate) struct ConversationId(String);

/// A conversation as the page names one everywhere: its agent and the
/// audience the page minted.
#[derive(Clone, Serialize)]
pub(crate) struct ConversationKey {
    pub(crate) agent_id: String,
    pub(crate) audience_id: ConversationId,
}

impl ConversationKey {
    /// The key of a conversation core lists, or `None` for one the page did
    /// not make, whose audience is no UUID.
    pub(crate) fn of(key: &observation::ConversationKey) -> Option<Self> {
        Some(Self {
            agent_id: key.agent_id().to_owned(),
            audience_id: ConversationId::try_from(key.audience_id().to_owned()).ok()?,
        })
    }

    /// Refuses a key whose agent this installation does not have, in words
    /// a person can read and before anything is staged or asked of core.
    pub(crate) fn configured(&self, web: &Web) -> Result<(), InterfaceError> {
        match web.registry.value.agent(&self.agent_id) {
            Ok(_) => Ok(()),
            Err(_) => Err(refusal(
                StatusCode::UNPROCESSABLE_ENTITY,
                &format!("Carillon has no agent {}.", self.agent_id),
            )),
        }
    }

    /// What core is asked about the conversation's runtime.
    pub(crate) fn query(&self) -> ConversationQuery {
        ConversationQuery {
            agent_id: self.agent_id.clone(),
            channel_id: self.audience_id.0.clone(),
            audience_id: Some(self.audience_id.0.clone()),
        }
    }
}

/// A message's event ID, `m-<uuid>`: the page mints it before the first
/// attempt and sends it again with every retry, and core answers a repeat
/// with the first receipt.
#[derive(Deserialize)]
#[serde(try_from = "String")]
pub(crate) struct EventId(String);

impl ConversationId {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl EventId {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// Why a path names no conversation or message. The provider answers the page
/// with its text.
#[derive(Debug)]
pub(crate) enum Malformed {
    Conversation,
    Message,
}

impl fmt::Display for Malformed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Conversation => "a conversation is a lowercase UUID",
            Self::Message => "a message is `m-` and a lowercase UUID",
        })
    }
}

impl std::error::Error for Malformed {}

impl TryFrom<String> for ConversationId {
    type Error = Malformed;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        if canonical(&text) {
            Ok(Self(text))
        } else {
            Err(Malformed::Conversation)
        }
    }
}

impl TryFrom<String> for EventId {
    type Error = Malformed;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        if text.strip_prefix("m-").is_some_and(canonical) {
            Ok(Self(text))
        } else {
            Err(Malformed::Message)
        }
    }
}
