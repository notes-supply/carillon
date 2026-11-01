//! Each routed channel's settings page for the operator, as `/settings`
//! draws it in Discord, titled with the channel's name.

use std::time::Duration;

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
};
use carillon_protocol::settings::{Change, Page};
use tokio::{
    task::JoinSet,
    time::{Instant, timeout},
};

use super::{DiscordState, authorize_operator};
use crate::{InterfaceError, channels, client::RUNTIME_CALL_TIMEOUT, config::DiscordRoute};

/// How long a title waits for Discord to name the channel.
const NAMING_LIMIT: Duration = Duration::from_secs(2);

/// One page per route, in the order the settings serving list them.
pub(super) async fn list(
    State(state): State<DiscordState>,
    headers: HeaderMap,
) -> Result<Json<Vec<Page>>, InterfaceError> {
    authorize_operator(&state, &headers)?;
    let mut reads = JoinSet::new();
    for (index, route) in state.settings.current().routes.iter().cloned().enumerate() {
        let state = state.clone();
        reads.spawn(async move { (index, page(&state, &route).await) });
    }
    let mut pages = reads.join_all().await;
    pages.sort_by_key(|(index, _)| *index);
    Ok(Json(pages.into_iter().map(|(_, page)| page).collect()))
}

pub(super) async fn show(
    State(state): State<DiscordState>,
    headers: HeaderMap,
    Path(channel_id): Path<String>,
) -> Result<Json<Page>, InterfaceError> {
    authorize_operator(&state, &headers)?;
    let route = routed(&state, &channel_id)?;
    Ok(Json(page(&state, &route).await))
}

/// Applies the change and answers the page it leaves, or the refusal with
/// its status: 422 for a read-only line, and the runtime's own otherwise.
pub(super) async fn change(
    State(state): State<DiscordState>,
    headers: HeaderMap,
    Path(channel_id): Path<String>,
    body: Result<Json<Change>, JsonRejection>,
) -> Result<Json<Page>, InterfaceError> {
    authorize_operator(&state, &headers)?;
    let route = routed(&state, &channel_id)?;
    let Json(change) = body.map_err(|rejection| InterfaceError {
        status: StatusCode::BAD_REQUEST,
        message: rejection.body_text(),
    })?;
    let title = title(&state, &route).await;
    let until = Instant::now() + RUNTIME_CALL_TIMEOUT;
    Ok(Json(
        channels::change(&state.core, &route, title, change, until).await?,
    ))
}

fn routed(state: &DiscordState, channel_id: &str) -> Result<DiscordRoute, InterfaceError> {
    state
        .settings
        .current()
        .routes
        .iter()
        .find(|route| route.channel_id == channel_id)
        .cloned()
        .ok_or_else(|| InterfaceError {
            status: StatusCode::NOT_FOUND,
            message: format!("Channel {channel_id} is not a Discord route."),
        })
}

async fn page(state: &DiscordState, route: &DiscordRoute) -> Page {
    let title = title(state, route).await;
    channels::page(
        &state.core,
        route,
        title,
        Instant::now() + RUNTIME_CALL_TIMEOUT,
    )
    .await
}

/// `#<name>` as Discord names the channel while the provider is connected,
/// otherwise `#<channel ID>`.
async fn title(state: &DiscordState, route: &DiscordRoute) -> String {
    let named = match (state.app(), route.channel_id.parse()) {
        (Some(app), Ok(id)) => timeout(NAMING_LIMIT, async {
            app.http.channel(id).await.ok()?.model().await.ok()?.name
        })
        .await
        .ok()
        .flatten(),
        _ => None,
    };
    format!("#{}", named.as_deref().unwrap_or(&route.channel_id))
}
