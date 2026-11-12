//! The browser API, under `/api`: what the page reads and sends. Every
//! route but `sign_in` is behind the browser's token. Core's wire types are
//! parsed here into the page's own view types and never forwarded.

use std::sync::Arc;

use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use carillon_interface::InterfaceError;
use serde::{Deserialize, Serialize};

use crate::{browser::refusal, serve::Web};

pub(crate) fn routes() -> Router<Arc<Web>> {
    Router::new().route("/session", get(session))
}

#[derive(Deserialize)]
pub(crate) struct SignIn {
    code: String,
}

#[derive(Serialize)]
pub(crate) struct SignedIn {
    token: String,
}

/// `POST /api/sign-in`: trades the code of `carillon web open`'s link for
/// the browser's token. The code is the request's whole authority.
pub(crate) async fn sign_in(
    State(web): State<Arc<Web>>,
    Json(SignIn { code }): Json<SignIn>,
) -> Result<Json<SignedIn>, InterfaceError> {
    let token = web.browser.redeem(&code).ok_or_else(|| {
        refusal(
            StatusCode::UNAUTHORIZED,
            "This sign-in link was used already or is older than two minutes. \
             Run `carillon web open` again.",
        )
    })?;
    Ok(Json(SignedIn { token }))
}

/// What the page needs before it shows anything.
#[derive(Serialize)]
struct Session {
    /// The configured agents, in registry order, for a new conversation to
    /// choose from.
    agents: Vec<Agent>,
    limits: Limits,
    /// The page served is the owner's copy, not the release's.
    custom_site: bool,
}

#[derive(Serialize)]
struct Agent {
    id: String,
    name: String,
}

/// Core's bounds on one message's files, for the page to check before it
/// uploads.
#[derive(Serialize)]
struct Limits {
    files: u32,
    file_bytes: u64,
    message_bytes: u64,
}

async fn session(State(web): State<Arc<Web>>) -> Json<Session> {
    let registry = &web.registry.value;
    let attachments = &registry.core.attachments;
    Json(Session {
        agents: registry
            .agents
            .iter()
            .map(|agent| Agent {
                id: agent.id.clone(),
                name: agent.label(&registry.agents),
            })
            .collect(),
        limits: Limits {
            files: attachments.max_files,
            file_bytes: attachments.max_file_bytes(),
            message_bytes: attachments.max_event_bytes(),
        },
        custom_site: web.site.is_custom(),
    })
}
