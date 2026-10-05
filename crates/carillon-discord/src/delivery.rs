//! Native delivery and feedback. Core remains the owner of delivery claims/outcomes.
use crate::{
    activity::Outcome,
    client::CoreClient,
    config::{DiscordConfig, DiscordEmoji},
};
use anyhow::{Context, Result, bail};
use carillon_protocol::model::{Delivery, DeliveryOutcome, ExternalEvent};
use std::{
    collections::{HashMap, hash_map::Entry},
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};
use tokio::{
    sync::{Semaphore, mpsc, watch},
    time::{sleep, timeout},
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
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
/// How long work waits for feedback it does not depend on before going on
/// without it: a message's admission, for its processing reaction and
/// typing indicator, and the provider's stop, for reactions under way.
pub(crate) const PROMPT_FEEDBACK: Duration = Duration::from_secs(2);
/// Outcome reactions shown beside work that are owned at once, from when
/// they are asked for until they end. Beyond them an outcome reaction is
/// not shown, and that is logged: the work never waits for its feedback.
const OUTCOMES: usize = 64;

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

/// A message's reactions, in order: its processing reaction, then the
/// delivered, failed or uncertain one that replaces it.
///
/// A processing reaction is a request these reactions own until Discord
/// answers it or [`FEEDBACK_LIMIT`] passes, and a message admitted again
/// meanwhile, such as a duplicate, shares it. Admission waits for it at most
/// [`PROMPT_FEEDBACK`], and stopping that wait cancels only the wait: a
/// request Discord already received may still be applied, so the request
/// goes on. The message's outcome reaction waits for it to end before
/// removing it, so a processing reaction Discord answered cannot be added
/// after the outcome. One Discord never answers leaves its effect unknown:
/// it may still be applied later, and that is logged, not assumed away.
///
/// Reactions that run beside work are tasks these reactions own until
/// [`Reactions::stop`]: processing requests, one per message at a time, and
/// at most [`OUTCOMES`] outcome reactions.
#[derive(Clone)]
pub(crate) struct Reactions(Arc<Owned>);

struct Owned {
    http: Arc<DiscordHttp>,
    config: DiscordConfig,
    /// Processing requests not yet ended, one per feedback target. A
    /// receiver closes when its request ends.
    unanswered: Mutex<HashMap<String, watch::Receiver<()>>>,
    /// Slots for the outcome reactions shown beside work.
    outcomes: Arc<Semaphore>,
    tasks: TaskTracker,
    stopped: CancellationToken,
}

impl Reactions {
    pub(crate) fn new(http: Arc<DiscordHttp>, config: DiscordConfig) -> Self {
        Self(Arc::new(Owned {
            http,
            config,
            unanswered: Mutex::new(HashMap::new()),
            outcomes: Arc::new(Semaphore::new(OUTCOMES)),
            tasks: TaskTracker::new(),
            stopped: CancellationToken::new(),
        }))
    }

    /// The processing reaction and a typing indicator, before a message is
    /// handed to core. Admission waits for them at most
    /// [`PROMPT_FEEDBACK`]; the processing request goes on without it.
    pub(crate) async fn admission(&self, event: &ExternalEvent) {
        let processing = self.processing(&event.feedback_target_id);
        let shown = async {
            tokio::join!(
                async {
                    if let Some(mut ended) = processing {
                        let _ = ended.changed().await;
                    }
                },
                trigger_typing(&self.0.http, &self.0.config, &event.delivery_target_id),
            );
        };
        if timeout(PROMPT_FEEDBACK, shown).await.is_err() {
            warn!(event_id = %event.event_id, "Discord feedback was not shown in time; the message is admitted without waiting for it");
        }
    }

    /// Sends the message's processing reaction as a task these reactions
    /// own, unless its request is still under way. The returned receiver
    /// closes when the request has ended.
    fn processing(&self, target: &str) -> Option<watch::Receiver<()>> {
        if !self.0.config.reactions {
            return None;
        }
        let (sent, ended) = match lock(&self.0.unanswered).entry(target.to_owned()) {
            // A message admitted again, such as a duplicate, shares the
            // request under way: a second one could be applied after an
            // outcome that waited only for the first.
            Entry::Occupied(under_way) => return Some(under_way.get().clone()),
            Entry::Vacant(vacant) => {
                let (sent, ended) = watch::channel(());
                (sent, vacant.insert(ended).clone())
            }
        };
        let (this, target) = (self.clone(), target.to_owned());
        self.spawn(async move {
            let owned = &this.0;
            if let Err(failure) =
                create_feedback_reaction(&owned.http, &target, &owned.config.processing_emoji).await
            {
                // Unanswered, Discord may still apply it after the outcome.
                warn!(error = %failure, %target, "Discord processing reaction failed or was not answered");
            }
            lock(&owned.unanswered).remove(&target);
            drop(sent);
        });
        Some(ended)
    }

    /// Runs `reaction` as a task these reactions own, until it ends or they
    /// stop.
    fn spawn(&self, reaction: impl Future<Output = ()> + Send + 'static) {
        let Owned { tasks, stopped, .. } = &*self.0;
        tasks.spawn(stopped.clone().run_until_cancelled_owned(reaction));
    }

    /// Waits until the message's processing request, if one is under way,
    /// has ended.
    async fn after_processing(&self, target: &str) {
        let under_way = lock(&self.0.unanswered).get(target).cloned();
        if let Some(mut ended) = under_way {
            let _ = ended.changed().await;
        }
    }

    /// Replaces the message's processing reaction with its outcome, once
    /// the processing request has ended.
    pub(crate) async fn outcome(&self, target: &str, feedback: Feedback) {
        let Owned { http, config, .. } = &*self.0;
        if !config.reactions {
            return;
        }
        self.after_processing(target).await;
        if let Err(failure) = delete_feedback_reaction(http, target, &config.processing_emoji).await
        {
            warn!(error = %failure, "Discord processing reaction removal failed");
        }
        let emoji = match feedback {
            Feedback::Delivered => &config.delivered_emoji,
            Feedback::Failed => &config.failed_emoji,
            Feedback::Uncertain => &config.uncertain_emoji,
        };
        if let Err(failure) = create_feedback_reaction(http, target, emoji).await {
            warn!(error = %failure, "Discord terminal reaction failed");
        }
    }

    /// Shows the message's outcome as a task these reactions own, so the
    /// work that asked for it goes on. Beyond [`OUTCOMES`] under way, it is
    /// not shown, and that is logged.
    pub(crate) fn show_outcome(&self, target: String, feedback: Feedback) {
        let Ok(slot) = self.0.outcomes.clone().try_acquire_owned() else {
            warn!(%target, "Discord outcome reaction not shown: too many are under way");
            return;
        };
        let this = self.clone();
        self.spawn(async move {
            this.outcome(&target, feedback).await;
            drop(slot);
        });
    }

    /// Removes the processing reaction of a message that gets no outcome of
    /// its own, such as a duplicate, once its request has ended.
    pub(crate) async fn clear(&self, target: &str) {
        let Owned { http, config, .. } = &*self.0;
        if !config.reactions {
            return;
        }
        self.after_processing(target).await;
        if let Err(failure) = delete_feedback_reaction(http, target, &config.processing_emoji).await
        {
            warn!(error = %failure, "Discord processing reaction removal failed");
        }
    }

    /// Ends the reactions under way when the provider stops: they get
    /// `grace` to end, then the rest are cancelled and no reaction starts
    /// again. Returns how many were cancelled. A cancelled request Discord
    /// already received may still be applied; its effect is unknown.
    pub(crate) async fn stop(&self, grace: Duration) -> usize {
        let Owned { tasks, stopped, .. } = &*self.0;
        tasks.close();
        let unfinished = match timeout(grace, tasks.wait()).await {
            Ok(()) => 0,
            Err(_) => tasks.len(),
        };
        stopped.cancel();
        if unfinished > 0 {
            warn!(
                unfinished,
                "Discord reactions still under way at stop were cancelled; Discord may still apply those it received"
            );
            tasks.wait().await;
        }
        unfinished
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
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
    reactions: Reactions,
    http: Arc<DiscordHttp>,
    outcomes: mpsc::UnboundedSender<Outcome>,
) -> Result<()> {
    loop {
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
                reconcile_after_claim_failure(&core, &reactions).await?;
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
        // The reactions run beside delivery, so a reaction Discord is slow
        // to take never holds up the next answer.
        reactions.show_outcome(delivery.feedback_target_id, shown);
    }
}

/// A failed claim response is ambiguous: core may have committed the claim
/// before disconnecting. Reconciliation preserves this instance's claim so
/// polling can recover it, while surfacing claims abandoned by older instances.
async fn reconcile_after_claim_failure(core: &CoreClient, reactions: &Reactions) -> Result<()> {
    loop {
        match core.reconcile().await {
            Ok(reconciled) => {
                for target in reconciled.feedback_target_ids {
                    reactions.outcome(&target, Feedback::Uncertain).await;
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
    // One scan of at most LIMIT characters per chunk finds where the rest
    // exceeds the limit, if it does; the rest is never counted whole.
    while let Some((end, _)) = remaining.char_indices().nth(LIMIT) {
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
pub(crate) mod tests;
