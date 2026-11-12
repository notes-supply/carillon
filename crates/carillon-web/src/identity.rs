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

use std::fmt;

use axum::http::StatusCode;
use carillon_interface::{CoreClient, InterfaceError};
use carillon_protocol::conversations::ConversationSummary;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{browser::refusal, serve::Web};

/// A UUID as the page writes it: lowercase with hyphens, so one
/// conversation has one name.
fn canonical(text: &str) -> Option<Uuid> {
    let uuid = Uuid::try_parse(text).ok()?;
    (uuid.hyphenated().to_string() == text).then_some(uuid)
}

/// A conversation: its audience, channel and delivery target in core.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct ConversationId(Uuid);

impl ConversationId {
    pub(crate) fn parse(text: &str) -> Option<Self> {
        canonical(text).map(Self)
    }
}

impl TryFrom<String> for ConversationId {
    type Error = &'static str;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text).ok_or("a conversation is a lowercase UUID")
    }
}

impl fmt::Display for ConversationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.hyphenated().fmt(formatter)
    }
}

impl From<ConversationId> for String {
    fn from(id: ConversationId) -> Self {
        id.to_string()
    }
}

/// A message's event ID, `m-<uuid>`: the page mints it before the first
/// attempt and sends it again with every retry, and core answers a repeat
/// with the first receipt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(try_from = "String")]
pub(crate) struct EventId(String);

impl EventId {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for EventId {
    type Error = &'static str;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        match text.strip_prefix("m-").and_then(canonical) {
            Some(_) => Ok(Self(text)),
            None => Err("a message is `m-` and a lowercase UUID"),
        }
    }
}

/// The conversation as core has it, or `None` before its first message.
/// Core keys a conversation by its agent too, so it answers an audience
/// with one conversation for each agent. The page gives an audience one
/// agent for good, so more than one is refused.
pub(crate) async fn find(
    core: &CoreClient,
    id: ConversationId,
) -> Result<Option<ConversationSummary>, InterfaceError> {
    let mut found = core.conversations_of(&id.to_string()).await?;
    if found.len() > 1 {
        return Err(refusal(
            StatusCode::CONFLICT,
            &format!("Conversation {id} has more than one agent, which the web chat never makes."),
        ));
    }
    Ok(found.pop())
}

/// The conversation, which must exist.
pub(crate) async fn known(
    core: &CoreClient,
    id: ConversationId,
) -> Result<ConversationSummary, InterfaceError> {
    find(core, id).await?.ok_or_else(|| {
        refusal(
            StatusCode::NOT_FOUND,
            &format!("Carillon has no web conversation {id}."),
        )
    })
}

/// The agent a message to conversation `id` goes to. A conversation core
/// has keeps its agent, and naming another is refused. One core does not
/// have yet takes the agent `named`, which must be configured.
pub(crate) async fn agent_of(
    web: &Web,
    id: ConversationId,
    named: Option<String>,
) -> Result<String, InterfaceError> {
    match (find(&web.core, id).await?, named) {
        (Some(existing), Some(named)) if existing.agent_id != named => Err(refusal(
            StatusCode::CONFLICT,
            &format!(
                "This conversation is with {}, and a conversation keeps its agent.",
                existing.agent_id
            ),
        )),
        (Some(existing), _) => Ok(existing.agent_id),
        (None, Some(named)) if web.registry.value.agent(&named).is_ok() => Ok(named),
        (None, Some(named)) => Err(refusal(
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("Carillon has no agent {named}."),
        )),
        (None, None) => Err(refusal(
            StatusCode::UNPROCESSABLE_ENTITY,
            "A new conversation's first message names its agent.",
        )),
    }
}
