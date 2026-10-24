//! Native delivery and feedback. Core remains the owner of delivery claims/outcomes.
use crate::{
    activity::Delivering,
    client::CoreClient,
    config::{DiscordConfig, DiscordEmoji, Settings},
};
use anyhow::{Context, Result, bail};
use carillon_protocol::{
    model::{Delivery, DeliveryOutcome, ExternalEvent, TurnState},
    turns::{ControlAction, Silence},
};
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
    Steered,
    Stopped,
}

impl From<Silence> for Feedback {
    fn from(silence: Silence) -> Self {
        match silence {
            Silence::Steered => Self::Steered,
            Silence::Stopped => Self::Stopped,
        }
    }
}

/// A message's reactions, in order: its processing reaction, then the
/// delivered, failed or uncertain one that replaces it.
///
/// A processing reaction is a request these reactions own until Discord
/// answers it or [`FEEDBACK_LIMIT`] passes, and a message admitted again
/// meanwhile shares it. Admission waits for it at most
/// [`PROMPT_FEEDBACK`], and stopping that wait cancels only the wait: a
/// request Discord already received may still be applied, so the request
/// goes on. The message's outcome reaction waits for it to end before
/// removing it, so a processing reaction Discord answered cannot be added
/// after the outcome. One Discord never answers leaves its effect unknown:
/// it may still be applied later, and that is logged, not assumed away.
///
/// Feedback that runs beside work runs as tasks these reactions own until
/// [`Reactions::stop`]: processing requests, one per message at a time, at
/// most [`OUTCOMES`] outcome reactions, and the lanes' overload refusals.
/// After the stop nothing is sent, not even an outcome a caller awaits.
///
/// They also own the control reactions a message offers while its turn
/// waits in its runtime, changed one request at a time, and withdraw them
/// before the message's outcome shows. A message without a feedback target,
/// such as a turn its runtime opened, gets no reaction at all.
///
/// A message's reactions follow the settings it was admitted with, so its
/// outcome replaces the processing reaction that was added, whichever emoji
/// serves by then. Asking for the outcome ends the message's hold on those
/// settings, whether or not the outcome is shown; a message admitted again
/// after that, or one an earlier process admitted, follows the settings
/// serving then.
#[derive(Clone)]
pub(crate) struct Reactions(Arc<Owned>);

struct Owned {
    http: Arc<DiscordHttp>,
    settings: Settings,
    /// The settings each message this process admitted was admitted with,
    /// by feedback target, until its outcome is asked for.
    admitted: Mutex<HashMap<String, Arc<DiscordConfig>>>,
    /// Processing requests not yet ended, one per feedback target. A
    /// receiver closes when its request ends.
    unanswered: Mutex<HashMap<String, watch::Receiver<()>>>,
    /// Slots for the outcome reactions shown beside work.
    outcomes: Arc<Semaphore>,
    /// The control emoji each message offers, by feedback target, changed by
    /// one request at a time.
    controls: Mutex<HashMap<String, Arc<tokio::sync::Mutex<Vec<String>>>>>,
    tasks: TaskTracker,
    stopped: CancellationToken,
}

impl Reactions {
    pub(crate) fn new(http: Arc<DiscordHttp>, settings: Settings) -> Self {
        Self(Arc::new(Owned {
            http,
            settings,
            admitted: Mutex::new(HashMap::new()),
            unanswered: Mutex::new(HashMap::new()),
            outcomes: Arc::new(Semaphore::new(OUTCOMES)),
            controls: Mutex::new(HashMap::new()),
            tasks: TaskTracker::new(),
            stopped: CancellationToken::new(),
        }))
    }

    /// The processing reaction and a typing indicator, as the settings the
    /// message was admitted with say, before it is handed to core. The
    /// processing request is registered when admission is asked for, so the
    /// message's outcome waits for it; the returned wait for both ends within
    /// [`PROMPT_FEEDBACK`], and the processing request goes on without it.
    pub(crate) fn admission<'a>(
        &'a self,
        event: &'a ExternalEvent,
        config: &'a Arc<DiscordConfig>,
    ) -> impl Future<Output = ()> + 'a {
        let Owned { http, stopped, .. } = &*self.0;
        let processing = self.processing(&event.feedback_target_id, config);
        let shown = async move {
            tokio::join!(
                async {
                    if let Some(mut ended) = processing {
                        let _ = ended.changed().await;
                    }
                },
                trigger_typing(http, config, &event.delivery_target_id),
            );
        };
        async move {
            if timeout(PROMPT_FEEDBACK, stopped.run_until_cancelled(shown))
                .await
                .is_err()
            {
                warn!(event_id = %event.event_id, "Discord feedback was not shown in time; the message is admitted without waiting for it");
            }
        }
    }

    /// Sends the message's processing reaction as a task these reactions
    /// own, unless its request is still under way, and remembers the
    /// settings it was admitted with. A message admitted again before its
    /// outcome is asked for keeps the settings it was first admitted with,
    /// whose emoji its request added.
    /// The returned receiver closes when the request has ended.
    fn processing(&self, target: &str, config: &Arc<DiscordConfig>) -> Option<watch::Receiver<()>> {
        let admitted = lock(&self.0.admitted)
            .entry(target.to_owned())
            .or_insert_with(|| config.clone())
            .clone();
        if !admitted.reactions {
            return None;
        }
        let (sent, ended) = match lock(&self.0.unanswered).entry(target.to_owned()) {
            // A message admitted again shares the request under way: a
            // second one could be applied after an outcome that waited only
            // for the first.
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
                create_feedback_reaction(&owned.http, &target, &admitted.processing_emoji).await
            {
                // Unanswered, Discord may still apply it after the outcome.
                warn!(error = %failure, %target, "Discord processing reaction failed or was not answered");
            }
            lock(&owned.unanswered).remove(&target);
            drop(sent);
        });
        Some(ended)
    }

    /// The settings serving, which feedback follows.
    pub(crate) fn settings(&self) -> &Settings {
        &self.0.settings
    }

    /// The Discord client feedback is sent with.
    pub(crate) fn http(&self) -> &Arc<DiscordHttp> {
        &self.0.http
    }

    /// Runs `feedback` as a task these reactions own, until it ends or they
    /// stop.
    pub(crate) fn spawn(&self, feedback: impl Future<Output = ()> + Send + 'static) {
        let Owned { tasks, stopped, .. } = &*self.0;
        tasks.spawn(stopped.clone().run_until_cancelled_owned(feedback));
    }

    /// Waits until the message's processing request, if one is under way,
    /// has ended.
    async fn after_processing(&self, target: &str) {
        let under_way = lock(&self.0.unanswered).get(target).cloned();
        if let Some(mut ended) = under_way {
            let _ = ended.changed().await;
        }
    }

    /// The settings the message's outcome follows: those of its admission
    /// when this process admitted it, otherwise `known`, the settings the
    /// caller took it with, otherwise those serving now.
    fn settled(&self, target: &str, known: Option<&Arc<DiscordConfig>>) -> Arc<DiscordConfig> {
        let Owned {
            settings, admitted, ..
        } = &*self.0;
        let admitted = lock(admitted).remove(target);
        admitted
            .or_else(|| known.cloned())
            .unwrap_or_else(|| settings.current())
    }

    /// Replaces the message's processing reaction with its outcome, as the
    /// settings it was admitted with say, once the processing request has
    /// ended, unless these reactions stop first. The message's settings are
    /// taken when this is called, so an outcome never awaited still ends
    /// their hold.
    pub(crate) fn outcome<'a>(
        &'a self,
        target: &'a str,
        feedback: Feedback,
    ) -> impl Future<Output = ()> + 'a {
        let config = self.settled(target, None);
        self.show(target, feedback, config)
    }

    /// As [`Reactions::outcome`], for a message the caller took with
    /// `settings`, which its outcome follows even when it ends before
    /// admission.
    pub(crate) fn outcome_as<'a>(
        &'a self,
        target: &'a str,
        feedback: Feedback,
        settings: &Arc<DiscordConfig>,
    ) -> impl Future<Output = ()> + 'a {
        let config = self.settled(target, Some(settings));
        self.show(target, feedback, config)
    }

    async fn show(&self, target: &str, feedback: Feedback, config: Arc<DiscordConfig>) {
        let Owned { http, stopped, .. } = &*self.0;
        if target.is_empty() {
            return;
        }
        let emoji = match feedback {
            Feedback::Delivered => &config.delivered_emoji,
            Feedback::Failed => &config.failed_emoji,
            Feedback::Uncertain => &config.uncertain_emoji,
            Feedback::Steered => &config.steered_emoji,
            Feedback::Stopped => &config.stopped_emoji,
        };
        let shown = stopped.run_until_cancelled(async {
            self.change_controls(target, Vec::new()).await;
            if !config.reactions {
                return;
            }
            self.after_processing(target).await;
            if let Err(failure) =
                delete_feedback_reaction(http, target, &config.processing_emoji).await
            {
                warn!(error = %failure, "Discord processing reaction removal failed");
            }
            if let Err(failure) = create_feedback_reaction(http, target, emoji).await {
                warn!(error = %failure, "Discord terminal reaction failed");
            }
        });
        if shown.await.is_none() {
            warn!(%target, "Discord outcome reaction ended by the provider's stop");
        }
    }

    /// Shows the message's outcome as a task these reactions own, so the
    /// work that asked for it goes on. Beyond [`OUTCOMES`] under way, it is
    /// not shown, and that is logged. Either way, as for
    /// [`Reactions::outcome`], the message's settings are taken now.
    pub(crate) fn show_outcome(&self, target: String, feedback: Feedback) {
        let config = self.settled(&target, None);
        let Ok(slot) = self.0.outcomes.clone().try_acquire_owned() else {
            warn!(%target, "Discord outcome reaction not shown: too many are under way");
            return;
        };
        let this = self.clone();
        self.spawn(async move {
            this.show(&target, feedback, config).await;
            drop(slot);
        });
    }

    /// Offers `actions` on the message as the reactions the settings serving
    /// name for them, withdrawing those it offered before and no longer
    /// does: none withdraws them all. With reactions turned off nothing is
    /// offered.
    pub(crate) async fn offer_controls(&self, target: &str, actions: &[ControlAction]) {
        let config = self.0.settings.current();
        let wanted = actions
            .iter()
            .filter(|_| config.reactions)
            .map(|action| config.control_emoji(*action).to_owned())
            .collect();
        self.0
            .stopped
            .run_until_cancelled(self.change_controls(target, wanted))
            .await;
    }

    /// Makes the message's control reactions `wanted`, after any change
    /// under way. A reaction Discord may not have removed stays recorded, so
    /// a later change removes it.
    async fn change_controls(&self, target: &str, wanted: Vec<String>) {
        if target.is_empty() {
            return;
        }
        let offer = {
            let mut controls = lock(&self.0.controls);
            if wanted.is_empty() && !controls.contains_key(target) {
                return;
            }
            controls.entry(target.to_owned()).or_default().clone()
        };
        let mut offered = offer.lock().await;
        let http = &self.0.http;
        let mut kept = Vec::new();
        for emoji in offered.drain(..) {
            if wanted.contains(&emoji) {
                kept.push(emoji);
            } else if let Err(failure) = delete_feedback_reaction(http, target, &emoji).await {
                warn!(error = %failure, %target, "Discord control reaction removal failed");
                kept.push(emoji);
            }
        }
        for emoji in wanted {
            if kept.contains(&emoji) {
                continue;
            }
            if let Err(failure) = create_feedback_reaction(http, target, &emoji).await {
                warn!(error = %failure, %target, "Discord control reaction failed");
            }
            kept.push(emoji);
        }
        *offered = kept;
        let mut controls = lock(&self.0.controls);
        // Nobody else waits on an empty offer: it is forgotten.
        if offered.is_empty() && Arc::strong_count(&offer) == 2 {
            controls.remove(target);
        }
    }

    /// Ends the feedback under way when the provider stops: its tasks get
    /// `grace` to end, then the rest, and any outcome a caller awaits, are
    /// cancelled, as is anything asked for later. Returns how many tasks
    /// were cancelled. A cancelled request Discord already received may
    /// still be applied; its effect is unknown.
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

/// Claims, sends and acknowledges deliveries until the provider stops.
/// Stopping ends the wait for work, never a delivery already claimed: it is
/// sent and its outcome recorded first. Answers are plain text. `delivering`
/// hears of each claimed delivery's job before it is sent, and of its
/// outcome once core recorded it, without waiting for whoever receives it.
pub(crate) async fn delivery_loop(
    core: CoreClient,
    reactions: Reactions,
    http: Arc<DiscordHttp>,
    delivering: mpsc::UnboundedSender<Delivering>,
    stopping: CancellationToken,
) -> Result<()> {
    while !stopping.is_cancelled() {
        let delivery = match core.claim_delivery().await {
            Ok(Some(delivery)) => delivery,
            Ok(None) => {
                let _ = timeout(Duration::from_millis(500), stopping.cancelled()).await;
                continue;
            }
            // A briefly unreachable core must not cost the gateway session,
            // so transient claim failures wait in place. A claim core took
            // without answering is taken up by the next claim or, once the
            // provider stops, recorded as uncertain by the next instance.
            Err(failure) if failure.is_transient() => {
                warn!(error = %failure.message, "waiting to claim Discord delivery");
                tokio::select! {
                    reconciled = reconcile_after_claim_failure(&core, &reactions) => reconciled?,
                    () = stopping.cancelled() => break,
                }
                continue;
            }
            Err(failure) => bail!("delivery claim rejected: {}", failure.message),
        };
        // Nobody may be listening: activity cards can be disabled.
        let _ = delivering.send(Delivering::Claimed(delivery.origin.turn_id.clone()));
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
        // The reactions run beside delivery, so a reaction Discord is slow
        // to take never holds up the next answer.
        reactions.show_outcome(delivery.feedback_target_id.clone(), shown);
        let _ = delivering.send(Delivering::Recorded);
    }
    Ok(())
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

/// Sends a delivery's text and says how it went: its outcome, the messages
/// Discord confirmed, and the reaction to show.
async fn attempt_delivery(
    http: &DiscordHttp,
    delivery: &Delivery,
) -> (DeliveryOutcome, Option<String>, Feedback) {
    // A turn steered into another or stopped sends nothing: its message
    // shows how it ended.
    if let Some(silence) = delivery.silence {
        return (DeliveryOutcome::Delivered, None, silence.into());
    }
    let channel = match delivery.target_id.parse::<Id<ChannelMarker>>() {
        Ok(channel) => channel,
        Err(failure) => {
            warn!(error = %failure, "invalid Discord delivery target");
            return (DeliveryOutcome::Uncertain, None, Feedback::Uncertain);
        }
    };
    match send_discord_message(http, channel, &delivery.content).await {
        Ok(messages) => (
            DeliveryOutcome::Delivered,
            Some(
                messages
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
            ),
            if delivery.turn_state == TurnState::Completed {
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

/// Sends `content` in as many messages as it takes and returns the messages
/// in order.
pub(crate) async fn send_discord_message(
    http: &DiscordHttp,
    channel_id: Id<ChannelMarker>,
    content: &str,
) -> Result<Vec<Id<MessageMarker>>> {
    let chunks = discord_chunks(content);
    if chunks.is_empty() {
        bail!("Discord delivery content is empty");
    }
    // Delivered text is the agent's and the runtime's, not the user's: it
    // must not notify anyone, whatever mention syntax it contains.
    let silent = AllowedMentions::default();
    let mut ids = Vec::with_capacity(chunks.len());
    for chunk in &chunks {
        let request = http
            .create_message(channel_id)
            .allowed_mentions(Some(&silent))
            .content(chunk);
        ids.push(request.await?.model().await?.id);
    }
    Ok(ids)
}

pub(crate) fn discord_chunks(content: &str) -> Vec<String> {
    const LIMIT: usize = 2_000;
    let mut remaining = content.trim();
    let mut chunks = Vec::new();
    // One scan of at most LIMIT characters per chunk finds where the rest
    // exceeds the limit, if it does; the rest is never counted whole.
    while let Some((end, _)) = remaining.char_indices().nth(LIMIT) {
        let (candidate, _) = remaining.split_at(end);
        let split = candidate
            .rfind("\n\n")
            .or_else(|| candidate.rfind('\n'))
            .or_else(|| candidate.rfind(char::is_whitespace))
            .filter(|index| *index > 0)
            .unwrap_or(end);
        let (chunk, rest) = remaining.split_at(split);
        chunks.push(chunk.trim_end().to_owned());
        remaining = rest.trim_start();
    }
    if !remaining.is_empty() {
        chunks.push(remaining.to_owned());
    }
    chunks
}

#[cfg(test)]
pub(crate) mod tests;
