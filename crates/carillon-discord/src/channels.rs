//! A channel's settings page: this provider's lines for the channel's
//! route, then the lines of the runtime serving the route's agent, read and
//! changed through core. The route lines change through the provider's one
//! writer of its settings file; an unrouted channel's page has only its
//! Agent line, and choosing an agent creates the route. The provider builds
//! the channel's subject itself from the route, whoever asks, so a caller
//! can only ever reach the channels this provider routes.

use std::time::Duration;

use axum::http::StatusCode;
use carillon_discord_module::contract::Notice;
use carillon_installation::config::AgentConfig;
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
    config::{
        DiscordActivation, DiscordConfig, DiscordConversation, DiscordRoute, Refused, Writer,
    },
};

/// A channel as the settings serving route it.
#[derive(Clone, Debug)]
pub(crate) enum Routing {
    Routed(DiscordRoute),
    /// A channel of the configured guild no route names yet.
    Unrouted(String),
}

impl Routing {
    pub(crate) fn channel_id(&self) -> &str {
        match self {
            Self::Routed(route) => &route.channel_id,
            Self::Unrouted(channel_id) => channel_id,
        }
    }

    pub(crate) const fn route(&self) -> Option<&DiscordRoute> {
        match self {
            Self::Routed(route) => Some(route),
            Self::Unrouted(_) => None,
        }
    }

    pub(crate) fn of(discord: &DiscordConfig, channel_id: &str) -> Self {
        discord
            .routes
            .iter()
            .find(|route| route.channel_id == channel_id)
            .map_or_else(
                || Self::Unrouted(channel_id.to_owned()),
                |route| Self::Routed(route.clone()),
            )
    }
}

pub(crate) fn subject(core: &CoreClient, route: &DiscordRoute) -> Subject {
    Subject::Channel {
        interface_id: core.provider_id().to_owned(),
        channel_id: route.channel_id.clone(),
        agent_id: route.agent.clone(),
    }
}

/// The page by `until`. An unrouted channel's makes no runtime call.
pub(crate) async fn page(
    core: &CoreClient,
    writer: &Writer,
    routing: &Routing,
    title: String,
    until: Instant,
) -> Page {
    let runtime = match routing.route() {
        Some(route) => {
            let read = SettingsRequest {
                subject: subject(core, route),
                change: None,
            };
            Some(runtime(core, &read, until).await)
        }
        None => None,
    };
    composed(core, writer, routing, title, runtime)
}

/// Applies `change` by `until` and answers the page it leaves. A route
/// line's change goes to the settings file, and the page then shows the
/// runtime of the agent the route names; a runtime's refusal keeps its
/// status and words.
pub(crate) async fn change(
    core: &CoreClient,
    writer: &Writer,
    routing: &Routing,
    title: String,
    change: Change,
    until: Instant,
) -> Result<Page, InterfaceError> {
    if ROUTE_LINES.contains(&change.setting.as_str()) {
        let routing = reroute(core, writer, routing.channel_id(), change, until).await?;
        return Ok(page(core, writer, &routing, title, until).await);
    }
    let Some(route) = routing.route() else {
        return Err(InterfaceError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            message: "This channel has no agent yet; choose one first.".to_owned(),
        });
    };
    let request = SettingsRequest {
        subject: subject(core, route),
        change: Some(change),
    };
    let runtime = runtime(core, &request, until).await?;
    Ok(composed(core, writer, routing, title, Some(Ok(runtime))))
}

/// Writes a route line's change to the settings file and serves it, when
/// the line is still at the revision the change was drawn from in the file
/// and no other change holds the file until `until`. Choosing the agent of
/// an unrouted channel creates its route, answering only when mentioned, a
/// thread per conversation. A change whose file is in place but not
/// confirmed durable serves, and is reported as an error.
async fn reroute(
    core: &CoreClient,
    writer: &Writer,
    channel_id: &str,
    change: Change,
    until: Instant,
) -> Result<Routing, InterfaceError> {
    let interface_id = core.provider_id();
    let agents = &writer.installation().value.agents;
    let written = writer
        .change(until, |discord| {
            let index = discord
                .routes
                .iter()
                .position(|route| route.channel_id == channel_id);
            let lines = route_lines(
                interface_id,
                agents,
                &index.map_or_else(
                    || Routing::Unrouted(channel_id.to_owned()),
                    |index| Routing::Routed(discord.routes[index].clone()),
                ),
            );
            let line = lines
                .iter()
                .find(|line| line.id == change.setting)
                .ok_or_else(|| Refused::Invalid("This channel has no agent yet.".to_owned()))?;
            if line.revision != change.expected {
                return Err(Refused::Stale);
            }
            let chosen = match &change.value {
                Some(Value::One { id }) if line.choices.iter().any(|choice| &choice.id == id) => id,
                Some(value) => {
                    return Err(Refused::Invalid(format!(
                        "{} is not a choice of {}.",
                        match value {
                            Value::One { id } => id.clone(),
                            Value::Many { ids } => ids.join(","),
                        },
                        line.name
                    )));
                }
                None => {
                    return Err(Refused::Invalid(format!(
                        "{} has no default to return to; choose one.",
                        line.name
                    )));
                }
            };
            let Some(index) = index else {
                let route = DiscordRoute {
                    channel_id: channel_id.to_owned(),
                    agent: chosen.clone(),
                    activation: DiscordActivation::Mention,
                    conversation: DiscordConversation::Thread,
                };
                discord.routes.push(route.clone());
                return Ok(route);
            };
            let route = &mut discord.routes[index];
            match (line.id.as_str(), chosen.as_str()) {
                (AGENT, agent) => agent.clone_into(&mut route.agent),
                (LISTENS, MENTION) => route.activation = DiscordActivation::Mention,
                (LISTENS, _) => route.activation = DiscordActivation::Always,
                (REPLIES, THREAD) => route.conversation = DiscordConversation::Thread,
                (_, _) => route.conversation = DiscordConversation::Channel,
            }
            Ok(route.clone())
        })
        .await?;
    if let Some(error) = written.unsynced {
        return Err(InterfaceError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("Changed, but a system crash may undo it: {error:#}"),
        });
    }
    Ok(Routing::Routed(written.value))
}

/// How long before the end of a card's calls a change gives up, or half the
/// time left when that is less, so the page can still be read again after a
/// change that got no answer.
const REREAD: Duration = Duration::from_secs(2);

/// What a card shows after a change: the page it left, or the page read again
/// by `until` with what became of the change. A change the runtime did not
/// confirm is never sent again.
pub(crate) async fn settled(
    core: &CoreClient,
    writer: &Writer,
    routing: &Routing,
    title: String,
    change: Change,
    until: Instant,
) -> (Page, Option<Notice>) {
    let left = until.saturating_duration_since(Instant::now());
    let written_by = until - REREAD.min(left / 2);
    let failure = match self::change(core, writer, routing, title.clone(), change, written_by).await
    {
        Ok(page) => return (page, None),
        Err(failure) => failure,
    };
    let notice = match failure.status {
        StatusCode::CONFLICT => Notice::Stale,
        StatusCode::GATEWAY_TIMEOUT => Notice::Unconfirmed,
        _ => Notice::Refused(failure.message),
    };
    // A refused route change may still leave the channel routed otherwise
    // than the card was drawn: the page follows the served settings.
    let routing = Routing::of(&writer.current(), routing.channel_id());
    (
        page(core, writer, &routing, title, until).await,
        Some(notice),
    )
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
    writer: &Writer,
    routing: &Routing,
    title: String,
    runtime: Option<Result<RuntimeSettings, InterfaceError>>,
) -> Page {
    Page {
        title,
        subject: routing.route().map(|route| subject(core, route)),
        interface: route_lines(
            core.provider_id(),
            &writer.installation().value.agents,
            routing,
        ),
        runtime: match runtime {
            Some(Ok(settings)) => RuntimePart::Settings(settings),
            Some(Err(failure)) => RuntimePart::Unavailable {
                reason: failure.message,
            },
            None => RuntimePart::Settings(RuntimeSettings::default()),
        },
    }
}

const AGENT: &str = "agent";
const LISTENS: &str = "listens";
const REPLIES: &str = "replies";
const ROUTE_LINES: [&str; 3] = [AGENT, LISTENS, REPLIES];
const MENTION: &str = "mention";
const THREAD: &str = "thread";

/// The route's agent, when it answers and where it replies, each offering
/// its choices under the names the page shows; an unrouted channel has only
/// the agent line, without a value.
fn route_lines(interface_id: &str, agents: &[AgentConfig], routing: &Routing) -> Vec<Setting> {
    let choices = |choices: &[(&str, &str)]| {
        choices
            .iter()
            .map(|(id, name)| Choice {
                id: (*id).to_owned(),
                name: (*name).to_owned(),
                group: None,
            })
            .collect()
    };
    let line = |id: &str, name: &str, choices: Vec<Choice>, value: Option<&str>| Setting {
        id: id.to_owned(),
        name: name.to_owned(),
        choices,
        value: value.map(|value| Value::One {
            id: value.to_owned(),
        }),
        inherited: false,
        revision: revision(id, interface_id, routing, value),
    };
    let agents: Vec<(&str, &str)> = agents
        .iter()
        .map(|agent| (agent.id.as_str(), agent.id.as_str()))
        .collect();
    let Routing::Routed(route) = routing else {
        return vec![line(AGENT, "Agent", choices(&agents), None)];
    };
    vec![
        line(AGENT, "Agent", choices(&agents), Some(&route.agent)),
        line(
            LISTENS,
            "Listens",
            choices(&[(MENTION, "mentions"), ("always", "always")]),
            Some(match route.activation {
                DiscordActivation::Mention => MENTION,
                DiscordActivation::Always => "always",
            }),
        ),
        line(
            REPLIES,
            "Replies",
            choices(&[(THREAD, "threads"), ("channel", "channel")]),
            Some(match route.conversation {
                DiscordConversation::Thread => THREAD,
                DiscordConversation::Channel => "channel",
            }),
        ),
    ]
}

/// SHA-256 of the canonical JSON `[setting, subject key, value]`, so a line
/// drawn for one route never matches another.
fn revision(setting: &str, interface_id: &str, routing: &Routing, value: Option<&str>) -> Revision {
    let key = json!([
        "channel",
        interface_id,
        routing.channel_id(),
        routing.route().map(|route| &route.agent)
    ]);
    let digest = Sha256::digest(json!([setting, key, value]).to_string());
    match Revision::try_from(hex::encode(digest)) {
        Ok(revision) => revision,
        Err(reason) => unreachable!("a SHA-256 digest in hexadecimal is a revision: {reason}"),
    }
}

#[cfg(test)]
mod tests;
