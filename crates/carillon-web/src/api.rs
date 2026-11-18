//! The browser API, under `/api`: what the page asks of the provider beside
//! its stream, which [`crate::stream`] serves. Every route but `sign_in` is
//! behind the browser's token. The API is the page's own and changes with it.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post, put},
};
use carillon_interface::InterfaceError;
use carillon_protocol::{model::CommandCatalog, turns::ControlAction};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::{
    browser::refusal,
    identity::{ConversationId, ConversationKey, EventId},
    send,
    serve::Web,
    stream,
};

/// What the page is told while Carillon cannot answer it: core is away, as
/// it is for a moment at every restart, or the provider is stopping.
pub(crate) const AWAY: &str = "Carillon is not answering right now. Try again in a moment.";

/// What the page is told when core did not do what was asked of it. A
/// refusal core wrote for an interface keeps its words. Anything else is
/// one status a page tries again after, and what core said goes to the
/// log and never to the page: it can name a file of this host.
pub(crate) fn from_core(failure: InterfaceError) -> InterfaceError {
    if failure.status.is_client_error() || failure.status == StatusCode::INSUFFICIENT_STORAGE {
        return failure;
    }
    warn!(status = %failure.status, error = format!("{failure:#}"), "core did not answer for the page");
    refusal(StatusCode::SERVICE_UNAVAILABLE, AWAY)
}

pub(crate) fn routes() -> Router<Arc<Web>> {
    Router::new()
        .route("/stream", get(stream::stream))
        // A message's files are bounded part by part as they are staged, and
        // what it holds between parts is bounded as the body is read.
        .route(
            "/conversations/{agent_id}/{audience_id}/messages/{event_id}",
            put(send::send),
        )
        .route(
            "/conversations/{agent_id}/{audience_id}/commands",
            get(commands),
        )
        .route("/messages/{event_id}/control", post(control))
}

#[derive(Deserialize)]
pub(crate) struct SignIn {
    password: String,
}

#[derive(Serialize)]
pub(crate) struct SignedIn {
    token: String,
}

/// `POST /api/sign-in`: trades the web chat's password for the browser's
/// token. The password is the request's whole authority.
pub(crate) async fn sign_in(
    State(web): State<Arc<Web>>,
    Json(SignIn { password }): Json<SignIn>,
) -> Result<Json<SignedIn>, InterfaceError> {
    let token = web.browser.sign_in(password).await?;
    Ok(Json(SignedIn { token }))
}

#[derive(Deserialize)]
struct Control {
    action: ControlAction,
}

/// `POST /api/messages/{event_id}/control`: what a person does with the
/// turn of a message while a snapshot lists that control on it. Core relays
/// it to the runtime and keeps nothing of it.
async fn control(
    State(web): State<Arc<Web>>,
    Path(event_id): Path<EventId>,
    Json(Control { action }): Json<Control>,
) -> Result<StatusCode, InterfaceError> {
    web.core
        .control(event_id.as_str(), action)
        .await
        .map_err(from_core)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/conversations/{agent_id}/{audience_id}/commands`: the commands
/// the agent's runtime offers in the conversation, in the runtime's order,
/// for the page to suggest. A message invokes one with `/` and its name
/// first in the text.
async fn commands(
    State(web): State<Arc<Web>>,
    Path((agent_id, audience_id)): Path<(String, ConversationId)>,
) -> Result<Json<CommandCatalog>, InterfaceError> {
    let key = ConversationKey {
        agent_id,
        audience_id,
    };
    key.configured(&web)?;
    Ok(Json(
        web.core.commands(&key.query()).await.map_err(from_core)?,
    ))
}

#[cfg(test)]
mod tests;
