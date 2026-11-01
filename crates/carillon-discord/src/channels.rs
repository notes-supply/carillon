//! A routed channel's settings page: this provider's lines for the route,
//! read-only for now, then the lines of the runtime serving the route's
//! agent, read and changed through core. The provider builds the channel's
//! subject itself from the route, whoever asks, so a caller can only ever
//! reach the channels this provider routes.

use axum::http::StatusCode;
use carillon_discord_module::contract::Notice;
use carillon_protocol::settings::{
    Change, Choice, Page, Revision, RuntimePart, RuntimeSettings, Setting, SettingsRequest,
    Subject, Value,
};
use serde_json::json;
use sha2::{Digest as _, Sha256};
use tokio::time::{Instant, timeout_at};

use crate::{
    InterfaceError,
    client::{CoreClient, RUNTIME_LATE},
    config::{DiscordActivation, DiscordConversation, DiscordRoute},
};

pub(crate) fn subject(core: &CoreClient, route: &DiscordRoute) -> Subject {
    Subject::Channel {
        interface_id: core.provider_id().to_owned(),
        channel_id: route.channel_id.clone(),
        agent_id: route.agent.clone(),
    }
}

pub(crate) async fn page(
    core: &CoreClient,
    route: &DiscordRoute,
    title: String,
    until: Instant,
) -> Page {
    let read = SettingsRequest {
        subject: subject(core, route),
        change: None,
    };
    let runtime = runtime(core, &read, until).await;
    composed(core, route, title, runtime)
}

/// Applies `change` by `until` and answers the page it leaves. A route line
/// is refused as read-only with 422, as the runtime refuses a value it does
/// not offer; a runtime's refusal keeps its status and words.
pub(crate) async fn change(
    core: &CoreClient,
    route: &DiscordRoute,
    title: String,
    change: Change,
    until: Instant,
) -> Result<Page, InterfaceError> {
    if let Some(line) = route_lines(core.provider_id(), route)
        .into_iter()
        .find(|line| line.id == change.setting)
    {
        return Err(InterfaceError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            message: format!("{} is read-only.", line.name),
        });
    }
    let request = SettingsRequest {
        subject: subject(core, route),
        change: Some(change),
    };
    let runtime = runtime(core, &request, until).await?;
    Ok(composed(core, route, title, Ok(runtime)))
}

/// What a card shows after a change: the page it left, or the page read again
/// by `until` with what became of the change. A change the runtime did not
/// confirm is never sent again.
pub(crate) async fn settled(
    core: &CoreClient,
    route: &DiscordRoute,
    title: String,
    change: Change,
    until: Instant,
) -> (Page, Option<Notice>) {
    let failure = match self::change(core, route, title.clone(), change, until).await {
        Ok(page) => return (page, None),
        Err(failure) => failure,
    };
    let notice = match failure.status {
        StatusCode::CONFLICT => Notice::Stale,
        StatusCode::GATEWAY_TIMEOUT => Notice::Unconfirmed,
        _ => Notice::Refused(failure.message),
    };
    (page(core, route, title, until).await, Some(notice))
}

async fn runtime(
    core: &CoreClient,
    request: &SettingsRequest,
    until: Instant,
) -> Result<RuntimeSettings, InterfaceError> {
    timeout_at(until, core.settings(request))
        .await
        .unwrap_or_else(|_| {
            Err(InterfaceError {
                status: StatusCode::GATEWAY_TIMEOUT,
                message: RUNTIME_LATE.to_owned(),
            })
        })
}

fn composed(
    core: &CoreClient,
    route: &DiscordRoute,
    title: String,
    runtime: Result<RuntimeSettings, InterfaceError>,
) -> Page {
    Page {
        title,
        subject: subject(core, route),
        interface: route_lines(core.provider_id(), route),
        runtime: match runtime {
            Ok(settings) => RuntimePart::Settings(settings),
            Err(failure) => RuntimePart::Unavailable {
                reason: failure.message,
            },
        },
    }
}

/// The route's agent, when it answers and where it replies. Each offers
/// only its value, so it is shown and not editable.
fn route_lines(interface_id: &str, route: &DiscordRoute) -> Vec<Setting> {
    let line = |id: &str, name: &str, value: &str, shown: &str| Setting {
        id: id.to_owned(),
        name: name.to_owned(),
        choices: vec![Choice {
            id: value.to_owned(),
            name: shown.to_owned(),
            group: None,
        }],
        value: Some(Value::One {
            id: value.to_owned(),
        }),
        inherited: false,
        revision: revision(id, interface_id, route, value),
    };
    vec![
        line("agent", "Agent", &route.agent, &route.agent),
        match route.activation {
            DiscordActivation::Mention => line("listens", "Listens", "mention", "mentions"),
            DiscordActivation::Always => line("listens", "Listens", "always", "always"),
        },
        match route.conversation {
            DiscordConversation::Thread => line("replies", "Replies", "thread", "threads"),
            DiscordConversation::Channel => line("replies", "Replies", "channel", "channel"),
        },
    ]
}

/// SHA-256 of the canonical JSON `[setting, subject key, value]`, so a line
/// drawn for one route never matches another.
fn revision(setting: &str, interface_id: &str, route: &DiscordRoute, value: &str) -> Revision {
    let key = json!(["channel", interface_id, route.channel_id, route.agent]);
    let digest = Sha256::digest(json!([setting, key, value]).to_string());
    match Revision::try_from(hex::encode(digest)) {
        Ok(revision) => revision,
        Err(reason) => unreachable!("a SHA-256 digest in hexadecimal is a revision: {reason}"),
    }
}

#[cfg(test)]
mod tests;
