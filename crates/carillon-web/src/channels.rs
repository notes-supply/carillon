//! The operator API, under `/v1`, behind the operator's credential: the
//! calls `carillon web open` and `carillon web sign-out` make, and what
//! `carillon settings` asks every interface.

use std::sync::Arc;

use anyhow::Context as _;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{any, get, post},
};
use carillon_interface::InterfaceError;
use carillon_protocol::settings::Page;
use serde::{Deserialize, Serialize};

use crate::{browser::refusal, serve::Web};

pub(crate) fn routes() -> Router<Arc<Web>> {
    Router::new()
        .route("/sign-in", post(sign_in))
        .route("/sign-out", post(sign_out))
        .route("/channels", get(channels))
        .route("/channels/{channel_id}/settings", any(channel))
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

/// `GET /v1/channels`: no pages. A web chat is its own channel in core,
/// but it sets nothing of its own: it runs its agent's model.
async fn channels() -> Json<Vec<Page>> {
    Json(Vec::new())
}

async fn channel(Path(channel_id): Path<String>) -> InterfaceError {
    refusal(
        StatusCode::NOT_FOUND,
        &format!(
            "{channel_id} has no settings of its own: a web chat takes its agent's. Change \
             them with `carillon settings --agent`."
        ),
    )
}

#[cfg(test)]
mod tests;
