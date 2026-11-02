use std::sync::Arc;

use anyhow::Result;
use carillon_discord_module::contract::{InteractionKind, Reply};
use serde_json::Value;
use tracing::{error, info, warn};
use twilight_model::{
    application::interaction::{
        Interaction as DiscordInteraction, InteractionData, InteractionType,
    },
    channel::{
        ChannelType,
        message::{AllowedMentions, MessageFlags},
    },
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
};

use super::DiscordState;
use crate::{
    config::DiscordConfig,
    conversations::{AdmittedInteraction, InteractionSource, admit_interaction},
    module::{
        InteractionCalls, Submissions,
        answer::{Answer, answer},
    },
};

/// The provider's view of a gateway interaction, or `None` for kinds the
/// provider does not serve (pings, autocomplete, modals).
pub(super) fn interaction_source(interaction: &DiscordInteraction) -> Option<InteractionSource> {
    let (kind, name, values, options) = match (&interaction.kind, interaction.data.as_ref()?) {
        (InteractionType::ApplicationCommand, InteractionData::ApplicationCommand(data)) => (
            InteractionKind::Command,
            data.name.clone(),
            Vec::new(),
            serde_json::to_value(&data.options).unwrap_or(Value::Null),
        ),
        (InteractionType::MessageComponent, InteractionData::MessageComponent(data)) => (
            InteractionKind::Component,
            data.custom_id.clone(),
            data.values.clone(),
            Value::Null,
        ),
        _ => return None,
    };
    let channel = interaction.channel.as_ref()?;
    let parent_channel_id = matches!(
        channel.kind,
        ChannelType::AnnouncementThread | ChannelType::PrivateThread | ChannelType::PublicThread
    )
    .then(|| channel.parent_id.map(|id| id.to_string()))
    .flatten();
    Some(InteractionSource {
        guild_id: interaction.guild_id.map(|id| id.to_string()),
        user_id: interaction.author_id()?.to_string(),
        channel_id: channel.id.to_string(),
        parent_channel_id,
        kind,
        name,
        values,
        options,
    })
}

/// Runs an admitted interaction under the settings it was admitted with.
pub(super) async fn run_interaction(
    state: &DiscordState,
    admitted: AdmittedInteraction,
    settings: Arc<DiscordConfig>,
) -> Result<Reply> {
    let mut calls = InteractionCalls::new(state.core.clone(), admitted.route, admitted.audience_id);
    // Without a Discord connection there is nowhere to anchor a submission.
    if let Some(connection) = &state.connection {
        calls = calls.with_submissions(Submissions {
            http: Arc::clone(&connection.app.http),
            reactions: connection.reactions.clone(),
            lanes: connection.lanes.clone(),
            origin: admitted.origin,
            settings,
        });
    }
    state.module.invoke(calls, admitted.interaction).await
}

/// Answers one gateway interaction: acknowledge within Discord's window,
/// run the module, then edit the acknowledgement into the reply. Refusals
/// and failures are told to the user in general terms and logged in full.
pub(super) async fn handle_interaction(state: DiscordState, interaction: DiscordInteraction) {
    let Some(app) = state.app() else {
        return;
    };
    let Some(source) = interaction_source(&interaction) else {
        return;
    };
    let client = app.http.interaction(app.application_id);
    let deferred = InteractionResponse {
        kind: if source.kind == InteractionKind::Component {
            InteractionResponseType::DeferredUpdateMessage
        } else {
            InteractionResponseType::DeferredChannelMessageWithSource
        },
        data: Some(InteractionResponseData {
            flags: Some(MessageFlags::EPHEMERAL),
            ..InteractionResponseData::default()
        }),
    };
    if let Err(error) = client
        .create_response(interaction.id, &interaction.token, &deferred)
        .await
    {
        warn!(%error, "Discord interaction acknowledgement failed");
        return;
    }
    // The interaction keeps the settings it is admitted with.
    let settings = state.settings.current();
    let mut answer = match admit_interaction(&settings, source) {
        Ok(admitted) => match run_interaction(&state, admitted, settings).await {
            Ok(reply) => answer(&reply).unwrap_or_else(|error| {
                error!(%error, "Discord module reply was unusable");
                Answer::notice("The command produced an unusable reply.")
            }),
            Err(error) => {
                error!(%error, "Discord module invocation failed");
                Answer::notice("The command could not be completed.")
            }
        },
        Err(refusal) => {
            info!(reason = %refusal.message, "Discord interaction refused");
            Answer::notice("This command is not available here.")
        }
    };
    for attempt in 0..2 {
        match client
            .update_response(&interaction.token)
            .content(Some(&answer.content))
            .embeds(Some(&answer.embeds))
            .components(Some(&answer.components))
            .allowed_mentions(Some(&AllowedMentions::default()))
            .await
        {
            Ok(_) => return,
            Err(error) => {
                if attempt == 0
                    && matches!(error.kind(), twilight_http::error::ErrorType::Response { status, .. } if status.get() == 400)
                    && answer.remove_option_emojis()
                {
                    warn!(%error, "Discord refused interaction badges; retrying without option emoji");
                    continue;
                }
                warn!(%error, "Discord interaction reply failed");
                return;
            }
        }
    }
}
