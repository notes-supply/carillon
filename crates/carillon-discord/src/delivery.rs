//! Native delivery and feedback. Core remains the owner of delivery claims/outcomes.
use crate::{
    activity::Outcome,
    client::CoreClient,
    config::{DiscordConfig, DiscordEmoji},
};
use anyhow::{Context, Result, bail};
use carillon_protocol::model::{Delivery, DeliveryOutcome, ExternalEvent};
use std::{sync::Arc, time::Duration};
use tokio::{
    sync::mpsc,
    task::JoinSet,
    time::{sleep, timeout},
};
use tracing::warn;
use twilight_http::{Client as DiscordHttp, request::channel::reaction::RequestReactionType};
use twilight_model::{
    channel::message::AllowedMentions,
    id::{
        Id,
        marker::{ChannelMarker, EmojiMarker, MessageMarker},
    },
};
/// How long one feedback request may take, rate-limit waits included.
const FEEDBACK_LIMIT: Duration = Duration::from_secs(30);
/// How long feedback may hold up the work that waits for it: a message's
/// admission, or a command's answer. Feedback Discord is slower to take is
/// left out, with a warning, so the work it reports on goes first.
pub(crate) const PROMPT_FEEDBACK: Duration = Duration::from_secs(2);
/// Terminal reactions shown at once beside delivery, which never waits for
/// them; beyond this many, the next delivery waits for one to finish.
const FEEDBACK_IN_FLIGHT: usize = 8;

/// A feedback request, bounded as a whole.
async fn bounded<T>(
    request: impl IntoFuture<Output = Result<T, twilight_http::Error>>,
) -> Result<T> {
    Ok(timeout(FEEDBACK_LIMIT, request)
        .await
        .context("Discord did not answer in time")??)
}

#[derive(Clone, Copy)]
pub(crate) enum Feedback {
    Delivered,
    Failed,
    Uncertain,
}

/// `feedback` that work waits for, bounded by [`PROMPT_FEEDBACK`].
pub(crate) async fn promptly(feedback: impl Future<Output = ()>, what: &str) {
    if timeout(PROMPT_FEEDBACK, feedback).await.is_err() {
        warn!("Discord {what} did not finish in time and was left out");
    }
}

/// The processing reaction and a typing indicator, together, before a
/// message is handed to core. Admission goes first: what Discord has not
/// taken within [`PROMPT_FEEDBACK`] is left out. Either way it is settled
/// before core has the message, so the delivered or failed reaction that
/// follows always replaces it, never the other way round.
pub(crate) async fn admission_feedback(
    http: &DiscordHttp,
    config: &DiscordConfig,
    event: &ExternalEvent,
) {
    promptly(
        async {
            tokio::join!(
                processing_feedback(http, config, &event.feedback_target_id),
                trigger_typing(http, config, &event.delivery_target_id),
            );
        },
        "feedback before admission",
    )
    .await;
}

async fn processing_feedback(http: &DiscordHttp, config: &DiscordConfig, target: &str) {
    if config.reactions
        && let Err(failure) = create_feedback_reaction(http, target, &config.processing_emoji).await
    {
        warn!(error = %failure, "Discord processing reaction failed");
    }
}

pub(crate) async fn clear_processing_feedback(
    http: &DiscordHttp,
    config: &DiscordConfig,
    target: &str,
) {
    if config.reactions
        && let Err(failure) = delete_feedback_reaction(http, target, &config.processing_emoji).await
    {
        warn!(error = %failure, "Discord processing reaction removal failed");
    }
}

pub(crate) async fn terminal_feedback(
    http: &DiscordHttp,
    config: &DiscordConfig,
    target: &str,
    feedback: Feedback,
) {
    if !config.reactions {
        return;
    }
    clear_processing_feedback(http, config, target).await;
    let emoji = match feedback {
        Feedback::Delivered => &config.delivered_emoji,
        Feedback::Failed => &config.failed_emoji,
        Feedback::Uncertain => &config.uncertain_emoji,
    };
    if let Err(failure) = create_feedback_reaction(http, target, emoji).await {
        warn!(error = %failure, "Discord terminal reaction failed");
    }
}

async fn create_feedback_reaction(
    http: &DiscordHttp,
    target: &str,
    configured: &str,
) -> Result<()> {
    let (channel, message) = feedback_ids(target)?;
    let emoji = DiscordEmoji::parse(configured)?;
    bounded(http.create_reaction(channel, message, &reaction_type(&emoji))).await?;
    Ok(())
}

async fn delete_feedback_reaction(
    http: &DiscordHttp,
    target: &str,
    configured: &str,
) -> Result<()> {
    let (channel, message) = feedback_ids(target)?;
    let emoji = DiscordEmoji::parse(configured)?;
    bounded(http.delete_current_user_reaction(channel, message, &reaction_type(&emoji))).await?;
    Ok(())
}

fn reaction_type(emoji: &DiscordEmoji) -> RequestReactionType<'_> {
    match emoji {
        DiscordEmoji::Unicode(name) => RequestReactionType::Unicode { name },
        DiscordEmoji::Custom { name, id } => RequestReactionType::Custom {
            id: Id::<EmojiMarker>::new(*id),
            name: Some(name),
        },
    }
}

fn feedback_ids(target: &str) -> Result<(Id<ChannelMarker>, Id<MessageMarker>)> {
    let (channel, message) = target
        .split_once(':')
        .context("Discord feedback target is invalid")?;
    Ok((
        channel
            .parse()
            .context("Discord feedback channel is invalid")?,
        message
            .parse()
            .context("Discord feedback message is invalid")?,
    ))
}

/// Tell the sender of an admitted message why it was not accepted, replying
/// to it without notifying anyone.
pub(crate) async fn explain(http: &DiscordHttp, source: &str, text: &str) {
    let (channel, message) = match feedback_ids(source) {
        Ok(ids) => ids,
        Err(failure) => {
            warn!(error = %failure, "Discord explanation target is invalid");
            return;
        }
    };
    let silent = AllowedMentions::default();
    if let Err(failure) = bounded(
        http.create_message(channel)
            .reply(message)
            .fail_if_not_exists(false)
            .allowed_mentions(Some(&silent))
            .content(text),
    )
    .await
    {
        warn!(error = %failure, "Discord explanation failed");
    }
}

/// One typing indicator at admission. While the turn then runs, the
/// activity monitor keeps typing.
pub(crate) async fn trigger_typing(http: &DiscordHttp, config: &DiscordConfig, target: &str) {
    if !config.typing {
        return;
    }
    let channel = match target.parse::<Id<ChannelMarker>>() {
        Ok(channel) => channel,
        Err(failure) => {
            warn!(error = %failure, "Discord typing target is invalid");
            return;
        }
    };
    if let Err(failure) = bounded(http.create_typing_trigger(channel)).await {
        warn!(error = %failure, "Discord typing trigger failed");
    }
}

/// Claims, sends and acknowledges deliveries. Each recorded outcome is
/// reported to `outcomes` without waiting for whoever receives it.
pub(crate) async fn delivery_loop(
    core: CoreClient,
    config: DiscordConfig,
    http: Arc<DiscordHttp>,
    outcomes: mpsc::UnboundedSender<Outcome>,
) -> Result<()> {
    let config = Arc::new(config);
    // Terminal reactions run beside delivery, owned by this loop and ending
    // with it, so a reaction Discord is slow to take never holds up the
    // next answer.
    let mut feedback = JoinSet::new();
    loop {
        while feedback.try_join_next().is_some() {}
        let delivery = match core.claim_delivery().await {
            Ok(Some(delivery)) => delivery,
            Ok(None) => {
                sleep(Duration::from_millis(500)).await;
                continue;
            }
            // A briefly unreachable core must not cost the gateway session,
            // so transient claim failures wait in place.
            Err(failure) if failure.is_transient() => {
                warn!(error = %failure.message, "waiting to claim Discord delivery");
                reconcile_after_claim_failure(&core, &config, &http).await?;
                continue;
            }
            Err(failure) => bail!("delivery claim rejected: {}", failure.message),
        };
        let (outcome, native_ref, shown) = attempt_delivery(&http, &delivery).await;
        // The Discord outcome is known at this point. Abandoning the
        // acknowledgement would let a restart downgrade it to uncertain, so
        // transient core failures are waited out while the claim is held.
        loop {
            match core
                .acknowledge_delivery(delivery.id, outcome, native_ref.clone())
                .await
            {
                Ok(()) => break,
                Err(failure) if failure.is_transient() => {
                    warn!(
                        error = %failure.message,
                        delivery_id = delivery.id,
                        "waiting to acknowledge Discord delivery"
                    );
                    sleep(Duration::from_secs(1)).await;
                }
                Err(failure) => bail!("delivery acknowledgement rejected: {}", failure.message),
            }
        }
        // Nobody may be listening: activity cards can be disabled.
        let _ = outcomes.send(Outcome {
            turn_id: delivery.turn_id.clone(),
            turn_state: delivery.turn_state,
            delivery: outcome,
        });
        if feedback.len() >= FEEDBACK_IN_FLIGHT {
            feedback.join_next().await;
        }
        let (http, config) = (http.clone(), config.clone());
        feedback.spawn(async move {
            terminal_feedback(&http, &config, &delivery.feedback_target_id, shown).await;
        });
    }
}

/// A failed claim response is ambiguous: core may have committed the claim
/// before disconnecting. Reconciliation preserves this instance's claim so
/// polling can recover it, while surfacing claims abandoned by older instances.
async fn reconcile_after_claim_failure(
    core: &CoreClient,
    config: &DiscordConfig,
    http: &DiscordHttp,
) -> Result<()> {
    loop {
        match core.reconcile().await {
            Ok(reconciled) => {
                for target in reconciled.feedback_target_ids {
                    terminal_feedback(http, config, &target, Feedback::Uncertain).await;
                }
                return Ok(());
            }
            Err(failure) if failure.is_transient() => {
                warn!(error = %failure.message, "waiting to reconcile Discord delivery claims");
                sleep(Duration::from_secs(1)).await;
            }
            Err(failure) => bail!("delivery reconciliation rejected: {}", failure.message),
        }
    }
}

async fn attempt_delivery(
    http: &DiscordHttp,
    delivery: &Delivery,
) -> (DeliveryOutcome, Option<String>, Feedback) {
    let channel = match delivery.target_id.parse::<Id<ChannelMarker>>() {
        Ok(channel) => channel,
        Err(failure) => {
            warn!(error = %failure, "invalid Discord delivery target");
            return (DeliveryOutcome::Uncertain, None, Feedback::Uncertain);
        }
    };
    match send_discord_message(http, channel, &delivery.content).await {
        Ok(native_ref) => (
            DeliveryOutcome::Delivered,
            Some(native_ref),
            if delivery.turn_state == carillon_protocol::model::TurnState::Completed {
                Feedback::Delivered
            } else {
                Feedback::Uncertain
            },
        ),
        Err(failure) => {
            warn!(error = %failure, delivery_id = delivery.id, "Discord delivery outcome uncertain");
            (DeliveryOutcome::Uncertain, None, Feedback::Uncertain)
        }
    }
}

pub(crate) async fn send_discord_message(
    http: &DiscordHttp,
    channel_id: Id<ChannelMarker>,
    content: &str,
) -> Result<String> {
    let chunks = discord_chunks(content);
    if chunks.is_empty() {
        bail!("Discord delivery content is empty");
    }
    // Delivered text is the agent's and the runtime's, not the user's: it
    // must not notify anyone, whatever mention syntax it contains.
    let silent = AllowedMentions::default();
    let mut ids = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        let message = http
            .create_message(channel_id)
            .allowed_mentions(Some(&silent))
            .content(&chunk)
            .await?;
        ids.push(message.model().await?.id.to_string());
    }
    Ok(ids.join(","))
}

pub(crate) fn discord_chunks(content: &str) -> Vec<String> {
    const LIMIT: usize = 2_000;
    let mut remaining = content.trim();
    let mut chunks = Vec::new();
    while remaining.chars().count() > LIMIT {
        let end = remaining
            .char_indices()
            .nth(LIMIT)
            .map_or(remaining.len(), |(index, _)| index);
        let candidate = &remaining[..end];
        let split = candidate
            .rfind("\n\n")
            .or_else(|| candidate.rfind('\n'))
            .or_else(|| candidate.rfind(char::is_whitespace))
            .filter(|index| *index > 0)
            .unwrap_or(end);
        chunks.push(remaining[..split].trim_end().to_owned());
        remaining = remaining[split..].trim_start();
    }
    if !remaining.is_empty() {
        chunks.push(remaining.to_owned());
    }
    chunks
}

#[cfg(test)]
mod tests;
