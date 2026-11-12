//! The operator API, under `/v1`, behind the operator's credential: the
//! calls `carillon web open` and `carillon web sign-out` make, and the
//! settings pages `carillon settings` reads.

use std::sync::Arc;

use anyhow::Context as _;
use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use carillon_interface::InterfaceError;
use serde::{Deserialize, Serialize};

use crate::serve::Web;

pub(crate) fn routes() -> Router<Arc<Web>> {
    Router::new()
        .route("/sign-in", post(sign_in))
        .route("/sign-out", post(sign_out))
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
