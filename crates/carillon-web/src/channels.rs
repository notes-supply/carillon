//! The operator API, under `/v1`, behind the operator's credential: the
//! calls `carillon web open` and `carillon web sign-out` make, and the
//! settings pages `carillon settings` reads.

use std::sync::Arc;

use anyhow::Context as _;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};
use carillon_interface::InterfaceError;
use carillon_protocol::{
    conversations::ConversationSummary,
    settings::{Change, Page, RuntimePart, RuntimeSettings, SettingsRequest, Subject},
};
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;

use crate::{
    browser::refusal,
    identity::{ConversationId, find},
    serve::Web,
};

pub(crate) fn routes() -> Router<Arc<Web>> {
    Router::new()
        .route("/sign-in", post(sign_in))
        .route("/sign-out", post(sign_out))
        .route("/channels", get(list))
        .route("/channels/{channel_id}/settings", get(show).put(change))
}

/// What `POST /v1/sign-in` answers: the code of a link that signs one
/// browser in.
#[derive(Deserialize, Serialize)]
pub(crate) struct SignInCode {
    pub(crate) code: String,
}

async fn sign_in(State(web): State<Arc<Web>>) -> Json<SignInCode> {
    Json(SignInCode {
        code: web.browser.issue_code(),
    })
}

async fn sign_out(State(web): State<Arc<Web>>) -> Result<StatusCode, InterfaceError> {
    tokio::task::spawn_blocking(move || web.browser.sign_out())
        .await
        .context("replace the browser key")??;
    Ok(StatusCode::NO_CONTENT)
}

/// How many conversations `GET /v1/channels` lists: the latest active.
const RECENT: usize = 20;

/// A web channel is a conversation. Its agent is fixed, so the page has no
/// lines of this provider's own, only the runtime's.
fn subject(web: &Web, conversation: &ConversationSummary) -> Subject {
    Subject::Channel {
        interface_id: web.core.provider_id().to_owned(),
        channel_id: conversation.channel_id.clone(),
        agent_id: conversation.agent_id.clone(),
    }
}

fn page(
    web: &Web,
    conversation: &ConversationSummary,
    runtime: Result<RuntimeSettings, InterfaceError>,
) -> Page {
    let title = match conversation.title.as_str() {
        "" => "Untitled",
        title => title,
    };
    Page {
        title: format!("{title} · {}", conversation.agent_id),
        subject: Some(subject(web, conversation)),
        interface: Vec::new(),
        runtime: match runtime {
            Ok(settings) => RuntimePart::Settings(settings),
            Err(failure) => RuntimePart::Unavailable {
                reason: failure.message,
            },
        },
    }
}

/// The conversation's page as its runtime has it now. A runtime that does
/// not answer leaves the page listed, saying why.
async fn read(web: &Web, conversation: &ConversationSummary) -> Page {
    let request = SettingsRequest {
        subject: subject(web, conversation),
        change: None,
    };
    page(web, conversation, web.core.settings(&request).await)
}

/// The conversation a channel ID names, or 404: `carillon settings` and
/// the operator reach only this interface's own conversations.
async fn channel(web: &Web, channel_id: &str) -> Result<ConversationSummary, InterfaceError> {
    let unknown = || {
        refusal(
            StatusCode::NOT_FOUND,
            &format!("Channel {channel_id} is not a web conversation."),
        )
    };
    let id = ConversationId::parse(channel_id).ok_or_else(unknown)?;
    find(&web.core, id).await?.ok_or_else(unknown)
}

/// `GET /v1/channels`: the pages of the [`RECENT`] latest active
/// conversations, in that order.
async fn list(State(web): State<Arc<Web>>) -> Result<Json<Vec<Page>>, InterfaceError> {
    let recent = web.core.conversations(None).await?.conversations;
    let own = recent
        .into_iter()
        .filter(|listed| ConversationId::parse(&listed.audience_id).is_some());
    let mut reads = JoinSet::new();
    for (index, conversation) in own.take(RECENT).enumerate() {
        let web = Arc::clone(&web);
        reads.spawn(async move { (index, read(&web, &conversation).await) });
    }
    let mut pages = reads.join_all().await;
    pages.sort_by_key(|(index, _)| *index);
    Ok(Json(pages.into_iter().map(|(_, page)| page).collect()))
}

async fn show(
    State(web): State<Arc<Web>>,
    Path(channel_id): Path<String>,
) -> Result<Json<Page>, InterfaceError> {
    let conversation = channel(&web, &channel_id).await?;
    Ok(Json(read(&web, &conversation).await))
}

/// Applies the change at the runtime and answers the page it leaves, or
/// the runtime's refusal with its status.
async fn change(
    State(web): State<Arc<Web>>,
    Path(channel_id): Path<String>,
    Json(change): Json<Change>,
) -> Result<Json<Page>, InterfaceError> {
    let conversation = channel(&web, &channel_id).await?;
    let request = SettingsRequest {
        subject: subject(&web, &conversation),
        change: Some(change),
    };
    let settings = web.core.settings(&request).await?;
    Ok(Json(page(&web, &conversation, Ok(settings))))
}

#[cfg(test)]
mod tests;
