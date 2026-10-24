//! People's control reactions on a message whose turn waits in the runtime
//! that holds it. Admission is the same as a message's; core relays an
//! admitted control to the runtime, which decides.

use axum::http::StatusCode;
use carillon_protocol::turns::ControlAction;
use tracing::{debug, warn};
use twilight_model::gateway::GatewayReaction;

use crate::{client::CoreClient, config::DiscordConfig, conversations::admit_identity};

/// The control a reaction asks for: one the settings name, added by an
/// allowlisted person in the guild, never by the bot itself.
pub(crate) fn admitted(
    settings: &DiscordConfig,
    reaction: &GatewayReaction,
) -> Option<ControlAction> {
    let user = reaction.user_id.to_string();
    if user == settings.bot_user_id {
        return None;
    }
    let guild = reaction.guild_id.map(|guild| guild.to_string());
    admit_identity(settings, guild.as_deref(), &user).ok()?;
    settings.control_action(&reaction.emoji)
}

/// Asks core to relay `action` on the turn of the message `message_id` to
/// its runtime. A refusal shows nothing: the message's reactions say how its
/// turn stands. A control of a turn that no longer waits, or of a message
/// with no turn, is routine, since people react as turns move on.
pub(crate) async fn relay(core: CoreClient, message_id: String, action: ControlAction) {
    let Err(failure) = core.control(&message_id, action).await else {
        return;
    };
    let (status, error) = (failure.status, failure.message);
    if matches!(status, StatusCode::NOT_FOUND | StatusCode::CONFLICT) {
        debug!(%status, error, message_id, ?action, "a turn control was not taken");
    } else {
        warn!(%status, error, message_id, ?action, "a turn control was not taken");
    }
}

#[cfg(test)]
mod tests;
