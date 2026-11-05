//! Direct messages with the people Carillon admits: the only channels
//! outside the configured guild that message operations reach.
use twilight_http::Client;
use twilight_model::{channel::ChannelType, id::marker::GuildMarker};

use super::{ChannelId, Id, OperationError, UserId};

/// Where operations act: the configured guild, and for messages also the
/// direct messages of its allowlisted people.
#[derive(Debug)]
pub(crate) struct Scope {
    pub(crate) guild: Id<GuildMarker>,
    pub(crate) people: Vec<UserId>,
}

/// A channel message operations may act in.
pub(super) struct Target {
    pub(super) id: ChannelId,
    direct: bool,
}

impl Target {
    /// The address Discord shows a message of this channel at.
    pub(super) fn link(&self, scope: &Scope, message: &str) -> String {
        let place = if self.direct {
            "@me".to_owned()
        } else {
            scope.guild.to_string()
        };
        format!("https://discord.com/channels/{place}/{}/{message}", self.id)
    }
}

/// `id`, when it is a channel of the guild or a direct message with an
/// allowlisted person.
pub(super) async fn conversation(
    http: &Client,
    scope: &Scope,
    id: ChannelId,
) -> Result<Target, OperationError> {
    let channel = http
        .channel(id)
        .await?
        .model()
        .await
        .map_err(|_| OperationError::uncertain())?;
    let direct = channel.kind == ChannelType::Private;
    let admitted = if direct {
        channel.recipients.as_deref().is_some_and(|recipients| {
            !recipients.is_empty()
                && recipients
                    .iter()
                    .all(|person| scope.people.contains(&person.id))
        })
    } else {
        channel.guild_id == Some(scope.guild)
    };
    if !admitted {
        return Err(OperationError::invalid(
            "Channel is neither in the configured Discord guild nor a direct message with an allowlisted person.",
        ));
    }
    Ok(Target { id, direct })
}

/// The channel named, or the direct message with the person named, which
/// Discord opens if it has to.
pub(super) async fn destination(
    http: &Client,
    scope: &Scope,
    channel: Option<ChannelId>,
    person: Option<UserId>,
) -> Result<Target, OperationError> {
    match (channel, person) {
        (Some(channel), None) => conversation(http, scope, channel).await,
        (None, Some(person)) if scope.people.contains(&person) => Ok(Target {
            id: http
                .create_private_channel(person)
                .await?
                .model()
                .await
                .map_err(|_| OperationError::uncertain())?
                .id,
            direct: true,
        }),
        (None, Some(_)) => Err(OperationError::invalid(
            "Direct messages reach only the people Carillon admits.",
        )),
        _ => Err(OperationError::invalid(
            "Name either channel_id or user_id.",
        )),
    }
}
