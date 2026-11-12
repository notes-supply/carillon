//! An interface's reads of its own conversations. The interface is the
//! authenticated caller, never a name in the request.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::HeaderMap,
};
use carillon_protocol::conversations::{ConversationPage, Transcript};
use serde::Deserialize;

use super::{ApiError, AppState, interface_provider};

/// The page of conversations below the activity marker `before`, and only
/// those with `audience_id` when it is given.
#[derive(Deserialize)]
pub(super) struct ListQuery {
    before: Option<i64>,
    audience_id: Option<String>,
}

/// The page of turns before the turn `before`.
#[derive(Deserialize)]
pub(super) struct TranscriptQuery {
    before: Option<String>,
}

/// `GET /v1/conversations[?before=<activity>][&audience_id=<audience>]`: a
/// page of the caller's conversations, the latest active first. With
/// `audience_id` it lists only the conversations of that audience, one for
/// each agent it has talked to, so an interface that knows a conversation
/// by its audience alone can find its agent. The read touches no turn's text,
/// so an interface may poll it and compare each conversation's `activity`
/// and `at_work`: while both are unchanged it has no new turn and no new
/// answer, and its transcript need not be read again. A turn's state while
/// it is unfinished, and an external answer's delivery state, are not
/// covered: an interface that shows them reads the jobs of its unfinished
/// turns.
pub(super) async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Result<Json<ConversationPage>, ApiError> {
    let config = state.config();
    let interface = interface_provider(&headers, &config)?;
    Ok(Json(state.core.conversations(
        &interface.id,
        query.before,
        query.audience_id.as_deref(),
    )?))
}

/// `GET /v1/conversations/{agent_id}/{audience_id}[?before=<turn>]`: a page
/// of one of the caller's conversations: its newest turns, oldest first.
pub(super) async fn transcript(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((agent_id, audience_id)): Path<(String, String)>,
    Query(query): Query<TranscriptQuery>,
) -> Result<Json<Transcript>, ApiError> {
    let config = state.config();
    let interface = interface_provider(&headers, &config)?;
    Ok(Json(state.core.transcript(
        &interface.id,
        &agent_id,
        &audience_id,
        query.before.as_deref(),
    )?))
}
