//! How a web conversation and a message map onto core's identities.
//!
//! | Core field           | Web value                                   |
//! |----------------------|---------------------------------------------|
//! | `interface_id`       | the provider's ID, from its credential      |
//! | `audience_id`        | the conversation's UUID, which the page mints |
//! | `channel_id`         | the same UUID: a chat is its own settings subject |
//! | `delivery_target_id` | the same UUID, so a turn the runtime opens lands in the conversation |
//! | `agent_id`           | chosen with the first message, then fixed   |
//! | `event_id`           | `m-<uuid>`, minted by the page once per send |
//! | `feedback_target_id` | the event ID                                |
//! | `user_id`            | the installing user's login name            |
//!
//! The provider stores none of it. A conversation exists in core from its
//! first admitted message.

use axum::http::StatusCode;
use carillon_interface::InterfaceError;
use serde::Deserialize;
use uuid::Uuid;

use crate::{browser::refusal, serve::Web};

/// Whether `text` is a UUID as the page writes one: lowercase with
/// hyphens, so one conversation has one name.
pub(crate) fn canonical(text: &str) -> bool {
    Uuid::try_parse(text).is_ok_and(|uuid| uuid.hyphenated().to_string() == text)
}

/// A conversation: its audience, channel and delivery target in core.
#[derive(Deserialize)]
#[serde(try_from = "String")]
pub(crate) struct ConversationId(String);

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

impl TryFrom<String> for ConversationId {
    type Error = &'static str;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        if canonical(&text) {
            Ok(Self(text))
        } else {
            Err("a conversation is a lowercase UUID")
        }
    }
}

impl TryFrom<String> for EventId {
    type Error = &'static str;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        if text.strip_prefix("m-").is_some_and(canonical) {
            Ok(Self(text))
        } else {
            Err("a message is `m-` and a lowercase UUID")
        }
    }
}

/// The agent of conversation `id`. Core keys a conversation by its agent
/// too, so it answers an audience with one conversation for each agent.
/// The page gives an audience one agent for good: a conversation core has
/// keeps its agent, naming another is refused, and more than one is
/// nothing the page made. A conversation core does not have yet takes the
/// agent `named` with its first message, which must be configured.
pub(crate) async fn agent_of(
    web: &Web,
    id: &ConversationId,
    named: Option<String>,
) -> Result<String, InterfaceError> {
    let mut found = web.core.conversations_of(id.as_str()).await?;
    let existing = found.pop().map(|conversation| conversation.agent_id);
    match (existing, named) {
        (Some(_), _) if !found.is_empty() => Err(refusal(
            StatusCode::CONFLICT,
            "This conversation has more than one agent, which the web chat never makes.",
        )),
        (Some(existing), Some(named)) if existing != named => Err(refusal(
            StatusCode::CONFLICT,
            &format!("This conversation is with {existing}, and a conversation keeps its agent."),
        )),
        (Some(existing), _) => Ok(existing),
        (None, Some(named)) if web.registry.value.agent(&named).is_ok() => Ok(named),
        (None, Some(named)) => Err(refusal(
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("Carillon has no agent {named}."),
        )),
        (None, None) => Err(refusal(
            StatusCode::NOT_FOUND,
            "Carillon has no such web conversation. A first message names its agent.",
        )),
    }
}
