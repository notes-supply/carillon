//! Each routed channel's settings page for the operator, as `/settings`
//! draws it in Discord, titled with the channel's name, and the page of
//! direct messages under `dm`.

use std::time::Duration;

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
};
use carillon_interface::{RUNTIME_CALL_TIMEOUT, authorize_operator};
use carillon_protocol::settings::{Change, Page};
use tokio::{
    task::JoinSet,
    time::{Instant, timeout},
};

use super::DiscordState;
use crate::{
    InterfaceError,
    channels::{self, Routing},
};

/// How long a title waits for Discord to name the channel.
const NAMING_LIMIT: Duration = Duration::from_secs(2);

/// One page per route, in the order the settings serving list them, then
/// direct messages' when they are routed.
pub(super) async fn list(
    State(state): State<DiscordState>,
    headers: HeaderMap,
) -> Result<Json<Vec<Page>>, InterfaceError> {
    authorize_operator(&state.installation.value, &headers)?;
    let settings = state.settings.current();
    let routes = settings.routes.iter().cloned().map(Routing::Routed);
    let direct = settings
        .direct_messages
        .as_ref()
        .map(|direct| Routing::Direct(Some(direct.agent.clone())));
    let mut reads = JoinSet::new();
    for (index, routing) in routes.chain(direct).enumerate() {
        let state = state.clone();
        reads.spawn(async move { (index, page(&state, &routing).await) });
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
    authorize_operator(&state.installation.value, &headers)?;
    let routing = routed(&state, &channel_id)?;
    Ok(Json(page(&state, &routing).await))
}

/// Applies the change and answers the page it leaves, or the refusal with
/// its status: the settings file's for a route line, and the runtime's own
/// otherwise.
pub(super) async fn change(
    State(state): State<DiscordState>,
    headers: HeaderMap,
    Path(channel_id): Path<String>,
    body: Result<Json<Change>, JsonRejection>,
) -> Result<Json<Page>, InterfaceError> {
    authorize_operator(&state.installation.value, &headers)?;
    let routing = routed(&state, &channel_id)?;
    let Json(change) = body.map_err(|rejection| InterfaceError {
        status: StatusCode::BAD_REQUEST,
        message: rejection.body_text(),
    })?;
    let title = title(&state, &routing).await;
    let until = Instant::now() + RUNTIME_CALL_TIMEOUT;
    Ok(Json(
        channels::change(&state.core, &state.writer(), &routing, title, change, until).await?,
    ))
}

/// A routed channel, or direct messages, routed or not: choosing their
/// agent is how the operator admits them.
fn routed(state: &DiscordState, channel_id: &str) -> Result<Routing, InterfaceError> {
    match Routing::of(&state.settings.current(), channel_id) {
        Routing::Unrouted(_) => Err(InterfaceError {
            status: StatusCode::NOT_FOUND,
            message: format!("Channel {channel_id} is not a Discord route."),
        }),
        routing => Ok(routing),
    }
}

async fn page(state: &DiscordState, routing: &Routing) -> Page {
    let title = title(state, routing).await;
    channels::page(
        &state.core,
        &state.writer(),
        routing,
        title,
        Instant::now() + RUNTIME_CALL_TIMEOUT,
    )
    .await
}

/// `#<name>` as Discord names the channel while the provider is connected,
/// otherwise `#<channel ID>`; `Direct messages` for theirs.
async fn title(state: &DiscordState, routing: &Routing) -> String {
    if let Routing::Direct(_) = routing {
        return "Direct messages".to_owned();
    }
    let channel_id = routing.channel_id();
    let named = match (state.app(), channel_id.parse()) {
        (Some(app), Ok(id)) => timeout(NAMING_LIMIT, async {
            app.http.channel(id).await.ok()?.model().await.ok()?.name
        })
        .await
        .ok()
        .flatten(),
        _ => None,
    };
    format!("#{}", named.as_deref().unwrap_or(channel_id))
}
