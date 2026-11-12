//! A channel's settings page: this provider's lines for the channel's
//! route, then the lines of the runtime serving the route's agent, read and
//! changed through core. The route lines change through the provider's one
//! writer of its settings file; an unrouted channel's page has only its
//! Agent line, and choosing an agent creates the route. Direct messages
//! have a page of their own under [`DIRECT_MESSAGES`], whose only route line
//! is their agent. The provider builds
//! the channel's subject itself from the route, whoever asks, so a caller
//! can only ever reach the channels this provider routes.

use std::time::Duration;

use axum::http::StatusCode;
use carillon_discord_module::contract::Notice;
use carillon_installation::config::AgentConfig;
use carillon_interface::{CoreClient, RUNTIME_LATE};
use carillon_protocol::settings::{
    Change, Choice, Page, Revision, RuntimePart, RuntimeSettings, Setting, SettingsRequest,
    Subject, Value,
};
use serde_json::json;
use sha2::{Digest as _, Sha256};
use tokio::time::{Instant, timeout_at};

use crate::{
    InterfaceError,
    config::{
        DIRECT_MESSAGES, DirectMessages, DiscordActivation, DiscordConfig, DiscordConversation,
        DiscordRoute, Refused, Writer,
    },
};

/// A channel as the settings serving route it.
#[derive(Clone, Debug)]
pub(crate) enum Routing {
    Routed(DiscordRoute),
    /// A channel of the configured guild no route names yet.
    Unrouted(String),
    /// Direct messages, with the agent that answers them once one is
    /// chosen.
    Direct(Option<String>),
}

impl Routing {
    pub(crate) fn channel_id(&self) -> &str {
        match self {
            Self::Routed(route) => &route.channel_id,
            Self::Unrouted(channel_id) => channel_id,
            Self::Direct(_) => DIRECT_MESSAGES,
        }
    }

    pub(crate) const fn route(&self) -> Option<&DiscordRoute> {
        match self {
            Self::Routed(route) => Some(route),
            Self::Unrouted(_) | Self::Direct(_) => None,
        }
    }

    fn agent(&self) -> Option<&str> {
        match self {
            Self::Routed(route) => Some(&route.agent),
            Self::Direct(agent) => agent.as_deref(),
            Self::Unrouted(_) => None,
        }
    }

    pub(crate) fn of(discord: &DiscordConfig, channel_id: &str) -> Self {
        if channel_id == DIRECT_MESSAGES {
            return Self::Direct(
                discord
                    .direct_messages
                    .as_ref()
                    .map(|direct| direct.agent.clone()),
            );
        }
        discord
            .routes
            .iter()
            .find(|route| route.channel_id == channel_id)
            .map_or_else(
                || Self::Unrouted(channel_id.to_owned()),
                |route| Self::Routed(route.clone()),
            )
    }

    /// Whose settings the page shows: the channel's under its agent, once
    /// it has one.
    fn subject(&self, core: &CoreClient) -> Option<Subject> {
        Some(Subject::Channel {
            interface_id: core.provider_id().to_owned(),
            channel_id: self.channel_id().to_owned(),
            agent_id: self.agent()?.to_owned(),
        })
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
    let runtime = match routing.subject(core) {
        Some(subject) => {
            let read = SettingsRequest {
                subject,
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
    let Some(subject) = routing.subject(core) else {
        return Err(InterfaceError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            message: "This channel has no agent yet; choose one first.".to_owned(),
        });
    };
    let request = SettingsRequest {
        subject,
        change: Some(change),
    };
    let runtime = runtime(core, &request, until).await?;
    Ok(composed(core, writer, routing, title, Some(Ok(runtime))))
}

/// Writes a route line's change to the settings file and serves it, when
/// the line is still at the revision the change was drawn from in the file
/// and no other change holds the file until `until`. Choosing the agent of
/// an unrouted channel creates its route, answering only when mentioned, a
/// thread per conversation; choosing the agent of direct messages admits
/// them. A change whose file is in place but not confirmed durable serves,
/// and is reported as an error.
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
            let routing = Routing::of(discord, channel_id);
            let lines = route_lines(interface_id, agents, &routing);
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
            let route = match routing {
                Routing::Direct(_) => {
                    discord.direct_messages = Some(DirectMessages {
                        agent: chosen.clone(),
                    });
                    return Ok(Routing::Direct(Some(chosen.clone())));
                }
                Routing::Unrouted(_) => {
                    let route = DiscordRoute {
                        channel_id: channel_id.to_owned(),
                        agent: chosen.clone(),
                        activation: DiscordActivation::Mention,
                        conversation: DiscordConversation::Thread,
                    };
                    discord.routes.push(route.clone());
                    return Ok(Routing::Routed(route));
                }
                Routing::Routed(_) => discord
                    .routes
                    .iter_mut()
                    .find(|route| route.channel_id == channel_id)
                    .ok_or_else(|| Refused::Invalid("This channel has no agent yet.".to_owned()))?,
            };
            match (line.id.as_str(), chosen.as_str()) {
                (AGENT, agent) => agent.clone_into(&mut route.agent),
                (LISTENS, MENTION) => route.activation = DiscordActivation::Mention,
                (LISTENS, _) => route.activation = DiscordActivation::Always,
                (REPLIES, THREAD) => route.conversation = DiscordConversation::Thread,
                (_, _) => route.conversation = DiscordConversation::Channel,
            }
            Ok(Routing::Routed(route.clone()))
        })
        .await?;
    if let Some(error) = written.unsynced {
        return Err(InterfaceError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("Changed, but a system crash may undo it: {error:#}"),
        });
    }
    Ok(written.value)
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
        subject: routing.subject(core),
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
/// its choices under the names the page shows. An unrouted channel has only
/// the agent line, without a value; direct messages only the agent line,
/// as they are always answered, in one conversation per person.
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
        return vec![line(AGENT, "Agent", choices(&agents), routing.agent())];
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
        routing.agent()
    ]);
    let digest = Sha256::digest(json!([setting, key, value]).to_string());
    match Revision::try_from(hex::encode(digest)) {
        Ok(revision) => revision,
        Err(reason) => unreachable!("a SHA-256 digest in hexadecimal is a revision: {reason}"),
    }
}

#[cfg(test)]
mod tests;
