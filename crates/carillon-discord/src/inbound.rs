//! Native message preparation, feedback and durable submission.
//!
//! [`lanes`] decides when a message is prepared; [`prepare`] settles one: it
//! resolves the route, admits and activates the message, and only then
//! downloads any files, opens the conversation and submits the event. A
//! message with files is accepted whole or not at all, and its sender is
//! told why when it is not.
use crate::{
    InterfaceError,
    attachments::{Downloads, Refusal},
    client::CoreClient,
    config::DiscordConfig,
    conversations::{
        IncomingMessage, MessagePlan, Participation, admit_message, discord_parent_channel,
        prepare_conversation,
    },
    delivery::{Feedback, Reactions, explain, trigger_typing},
    error::discord_failure_is_terminal,
    gateway::Dispatch,
};
use anyhow::{Result, anyhow};
use axum::http::StatusCode;
use carillon_protocol::model::{EventReceipt, EventStatus, ExternalEvent};
use serde_json::Value;
use std::{future::pending, sync::Arc, time::Duration};
use tokio::{
    sync::{Semaphore, mpsc},
    time::{Instant, sleep, sleep_until, timeout},
};
use tokio_util::sync::CancellationToken;
use tracing::warn;
use twilight_http::Client as DiscordHttp;

pub(crate) mod lanes;

/// How long preparing a message may take, from resolving its route through
/// opening its conversation and, for files, transferring them, until it is
/// submitted. A submission core may have received is then settled by asking
/// core, however long that takes.
pub(crate) const PREPARATION_DEADLINE: Duration = Duration::from_mins(30);
/// The longest wait between attempts to submit downloaded files.
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// The wait between attempts to resolve a route or open a conversation.
const RETRY: Duration = Duration::from_millis(250);
/// How long one such attempt may take, waiting for Discord's rate limits
/// and reading its answer included; a slower one is tried again.
const ATTEMPT: Duration = Duration::from_secs(30);
/// Why a message whose preparation ran out of time was not accepted.
const LATE: &str = "Carillon could not prepare this message in time, so it was not accepted. Please send it again.";
/// Why a message still being prepared when the provider stopped was not accepted.
const STOPPED: &str =
    "Carillon was restarting, so this message was not accepted. Please send it again.";

#[derive(Clone, Debug)]
pub(crate) struct QueuedMessage {
    pub(crate) request: IncomingMessage,
    pub(crate) native: Value,
    /// The settings serving when the gateway took the message: its route,
    /// admission and feedback follow them however the settings change.
    pub(crate) settings: Arc<DiscordConfig>,
    /// The gateway dispatch, which the gateway's replay floor stays below
    /// until the message is settled; `None` for a message that did not come
    /// from the gateway.
    pub(crate) dispatch: Option<Dispatch>,
}

/// What became of a gateway message, as a replay of it after a restart
/// would meet it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Prepared {
    /// Ignored or refused before anything ran for it: not routed, not
    /// activated, or not from an allowlisted person.
    Ignored,
    /// Not accepted, for a reason the provider settled: its sender was told
    /// why, or its time to be prepared ran out.
    Refused,
    /// Core holds it, from this preparation or an earlier one.
    Accepted,
    /// The stop cut its preparation short before core said whether it holds
    /// it, or core would not say. When the stop cut short asking core, the
    /// answer that comes later, as feedback, may still report it settled;
    /// otherwise only a replay, which core's event identity resolves,
    /// settles it.
    Unconfirmed,
}

/// Where preparations report the gateway messages they settled. A message
/// still queued, being prepared, or [`Prepared::Unconfirmed`] is never
/// reported, so the gateway's replay floor stays below it.
#[derive(Clone, Debug)]
pub(crate) struct Settlements(mpsc::UnboundedSender<Dispatch>);

impl Settlements {
    pub(crate) const fn new(sender: mpsc::UnboundedSender<Dispatch>) -> Self {
        Self(sender)
    }

    /// Reports the message of `dispatch` settled, unless `prepared` is
    /// unconfirmed. A gateway that has stopped listening needs no report.
    pub(crate) fn report(&self, dispatch: Option<Dispatch>, prepared: Prepared) {
        if let Some(dispatch) = dispatch
            && prepared != Prepared::Unconfirmed
        {
            let _ = self.0.send(dispatch);
        }
    }
}

/// What preparing a message needs, shared by every lane.
pub(crate) struct Ingress {
    pub(crate) core: CoreClient,
    pub(crate) participation: Arc<Participation>,
    pub(crate) http: Arc<DiscordHttp>,
    /// The reactions that show each message's progress, in order.
    pub(crate) reactions: Reactions,
    pub(crate) downloads: Arc<Downloads>,
    /// Overload replies being sent at once, for the lanes and the gateway
    /// alike; beyond them refusals are only logged.
    pub(crate) refusals: Arc<Semaphore>,
    /// Cancelled when the provider stops: work not yet handed to core ends,
    /// and a submission core may have received is settled first.
    pub(crate) stopping: CancellationToken,
    pub(crate) settlements: Settlements,
}

impl Ingress {
    /// `work`, unless the provider stops or `deadline` passes first.
    async fn until<T>(
        &self,
        deadline: Option<Instant>,
        work: impl Future<Output = T>,
    ) -> Option<T> {
        let expired = async {
            match deadline {
                Some(deadline) => sleep_until(deadline).await,
                None => pending().await,
            }
        };
        tokio::select! {
            output = work => Some(output),
            () = expired => None,
            () = self.stopping.cancelled() => None,
        }
    }

    /// Repeat a Discord `attempt` until it succeeds or fails for good,
    /// bounding each try as a whole. `None` once the provider stops or
    /// `deadline` passes: an abandoned try that may have created a thread is
    /// reconciled by any later try for the same message.
    async fn retry<T, F: Future<Output = Result<T>>>(
        &self,
        deadline: Option<Instant>,
        doing: &str,
        mut attempt: impl FnMut() -> F,
    ) -> Option<Result<T>> {
        loop {
            let failure = match self.until(deadline, timeout(ATTEMPT, attempt())).await? {
                Ok(Ok(value)) => return Some(Ok(value)),
                Ok(Err(failure)) if discord_failure_is_terminal(&failure) => {
                    return Some(Err(failure));
                }
                Ok(Err(failure)) => failure,
                Err(_) => anyhow!("Discord did not answer within {}s", ATTEMPT.as_secs()),
            };
            warn!(error = %failure, "waiting to {doing}");
            self.until(deadline, sleep(RETRY)).await?;
        }
    }

    /// What preparation that ended early without telling its sender
    /// settled: nothing when the stop ended it, and a refusal when its time
    /// ran out.
    fn cut_short(&self) -> Prepared {
        if self.stopping.is_cancelled() {
            Prepared::Unconfirmed
        } else {
            Prepared::Refused
        }
    }

    fn unfinished(&self) -> Refusal {
        Refusal(
            if self.stopping.is_cancelled() {
                STOPPED
            } else {
                LATE
            }
            .to_owned(),
        )
    }
}

/// Settle one gateway message: by the time this returns it was accepted,
/// refused with feedback, ignored by admission, or left unconfirmed by the
/// stop or by core. Everything before submission ends by one preparation
/// deadline; a message whose route is not known by then was never admitted
/// and is dropped silently.
pub(crate) async fn prepare(ingress: &Ingress, mut inbound: QueuedMessage) -> Prepared {
    let deadline = Instant::now() + PREPARATION_DEADLINE;
    let settings = inbound.settings;
    let discord = &*settings;
    let http = &ingress.http;
    let channel = &inbound.request.channel_id;
    let route = ingress
        .retry(Some(deadline), "resolve a Discord message route", || {
            discord_parent_channel(http, discord, &ingress.participation, channel)
        })
        .await;
    inbound.request.parent_channel_id = match route {
        Some(Ok(parent)) => parent,
        Some(Err(failure)) => {
            let source = format!("{channel}:{}", inbound.request.event_id);
            ingress
                .reactions
                .outcome_as(&source, Feedback::Failed, &settings)
                .await;
            warn!(error = %failure, "Discord message route is unavailable");
            return Prepared::Refused;
        }
        None => return ingress.cut_short(),
    };
    let Ok(Some(plan)) = admit_message(
        discord,
        &ingress.participation,
        inbound.request,
        inbound.native,
    ) else {
        return Prepared::Ignored;
    };
    if plan.attachments.is_empty() {
        prepare_text(ingress, plan, &settings, deadline, inbound.dispatch).await
    } else {
        prepare_files(ingress, plan, &settings, deadline).await
    }
}

/// Open a text's conversation and start submitting it by `deadline`, then
/// submit it until core answers: once submitted, core may hold it, so only
/// the stop ends that.
async fn prepare_text(
    ingress: &Ingress,
    plan: MessagePlan,
    settings: &Arc<DiscordConfig>,
    deadline: Instant,
    dispatch: Option<Dispatch>,
) -> Prepared {
    let (reactions, http) = (&ingress.reactions, &*ingress.http);
    let source = format!("{}:{}", plan.channel_id, plan.message_id);
    let unfinished = async || {
        explain(http, &source, &ingress.unfinished().0).await;
        reactions
            .outcome_as(&source, Feedback::Failed, settings)
            .await;
    };
    // A message core already holds, such as one the gateway replayed, is
    // not admitted again: its feedback belongs to its first admission. A
    // stop or the deadline ends the lookup and starts no submission; core's
    // answer then decides the feedback, which the reactions own like any
    // other, and settles the message for the gateway once given.
    match ingress
        .until(
            Some(deadline),
            known_receipt(&ingress.core, &plan.message_id),
        )
        .await
    {
        Some(None) => {}
        Some(Some(_)) => return Prepared::Accepted,
        None => {
            let settling = settle_unfinished_text(
                ingress.core.clone(),
                reactions.clone(),
                ingress.http.clone(),
                plan.message_id,
                source,
                ingress.unfinished(),
                settings.clone(),
            );
            let settlements = ingress.settlements.clone();
            reactions.spawn(async move { settlements.report(dispatch, settling.await) });
            return Prepared::Unconfirmed;
        }
    }
    let opened = ingress
        .retry(Some(deadline), "prepare a Discord conversation", || {
            prepare_conversation(&ingress.participation, &ingress.http, plan.clone())
        })
        .await;
    let resolved = match opened {
        Some(Ok(resolved)) => resolved,
        Some(Err(failure)) => {
            warn!(error = %failure, "Discord conversation cannot be prepared");
            reactions
                .outcome_as(&source, Feedback::Failed, settings)
                .await;
            return Prepared::Refused;
        }
        None => {
            unfinished().await;
            return Prepared::Refused;
        }
    };
    reactions.admission(&resolved.event, settings).await;
    // Admission feedback can outlast the window; a text it closed on is not
    // submitted.
    if Instant::now() >= deadline {
        unfinished().await;
        return Prepared::Refused;
    }
    // Unless the provider has already stopped, submission makes an attempt
    // core may receive. A stop cuts none short; when core has not answered
    // by then, it is asked whether it holds the event, as for files.
    let attempted = !ingress.stopping.is_cancelled();
    let submitted =
        match submit_event_with_retry(&ingress.core, &ingress.stopping, &resolved.event).await {
            Some(Ok(receipt)) => Ok(receipt),
            Some(Err(failure)) => {
                warn!(error = %failure.message, "Discord event submission failed");
                reactions
                    .outcome_as(&source, Feedback::Failed, settings)
                    .await;
                return Prepared::Refused;
            }
            None if attempted => unanswered(ingress, &resolved.event.event_id).await,
            None => Err(NotAccepted::Refused(ingress.unfinished())),
        };
    // Accepted, even as a duplicate of its own earlier attempt, the message
    // gets its outcome from delivery.
    settle_submission(ingress, &source, settings, submitted).await
}

/// Feedback for a text whose preparation the stop or the deadline ended
/// before core said whether it holds it: none when core holds it, as its
/// first admission's feedback stands; the explanation and failure when core
/// does not; uncertain when core cannot say.
async fn settle_unfinished_text(
    core: CoreClient,
    reactions: Reactions,
    http: Arc<DiscordHttp>,
    event_id: String,
    source: String,
    refusal: Refusal,
    settings: Arc<DiscordConfig>,
) -> Prepared {
    match core.event_status(&event_id).await {
        Ok(EventStatus::Accepted(_) | EventStatus::Capturing) => Prepared::Accepted,
        Ok(EventStatus::Unknown) => {
            explain(&http, &source, &refusal.0).await;
            reactions
                .outcome_as(&source, Feedback::Failed, &settings)
                .await;
            Prepared::Refused
        }
        Err(failure) => {
            warn!(error = %failure.message, %event_id, "Discord stopped before core said whether it holds a message");
            reactions
                .outcome_as(&source, Feedback::Uncertain, &settings)
                .await;
            Prepared::Unconfirmed
        }
    }
}

/// Download, stage and submit a message with files. While they download the
/// sender sees typing; the processing reaction appears once they are handed
/// to core. Everything before submission ends by `deadline`.
async fn prepare_files(
    ingress: &Ingress,
    plan: MessagePlan,
    settings: &Arc<DiscordConfig>,
    deadline: Instant,
) -> Prepared {
    let (reactions, http) = (&ingress.reactions, &*ingress.http);
    let source = format!("{}:{}", plan.channel_id, plan.message_id);
    let refuse = async |refusal: Refusal| {
        explain(http, &source, &refusal.0).await;
        reactions
            .outcome_as(&source, Feedback::Failed, settings)
            .await;
    };
    if let Err(refusal) = ingress.downloads.admit(&plan.attachments) {
        refuse(refusal).await;
        return Prepared::Refused;
    }
    // A message core already holds or is still capturing, such as one the
    // gateway replayed, is not downloaded again.
    match replayed(ingress, &plan.message_id, deadline).await {
        Replayed::Held => return Prepared::Accepted,
        Replayed::Absent => {}
        Replayed::Unfinished => {
            refuse(ingress.unfinished()).await;
            return Prepared::Refused;
        }
        Replayed::Uncertain => {
            reactions
                .outcome_as(&source, Feedback::Uncertain, settings)
                .await;
            return Prepared::Unconfirmed;
        }
    }
    let typing = tokio::spawn(keep_typing(
        ingress.http.clone(),
        settings.clone(),
        plan.channel_id.clone(),
    ));
    let typing = typing.abort_handle();
    let staged = ingress
        .until(
            Some(deadline),
            ingress.downloads.stage(Some(http), &plan, deadline),
        )
        .await
        .unwrap_or_else(|| Err(ingress.unfinished()));
    let staged = match staged {
        Ok(staged) => staged,
        Err(refusal) => {
            typing.abort();
            refuse(refusal).await;
            return Prepared::Refused;
        }
    };
    let opened = ingress
        .retry(Some(deadline), "prepare a Discord conversation", || {
            prepare_conversation(&ingress.participation, &ingress.http, plan.clone())
        })
        .await;
    let mut resolved = match opened {
        Some(Ok(resolved)) => resolved,
        unopened => {
            typing.abort();
            // The staged files go before the sender hears anything.
            drop(staged);
            if let Some(Err(failure)) = unopened {
                warn!(error = %failure, "Discord conversation cannot be prepared");
                reactions
                    .outcome_as(&source, Feedback::Failed, settings)
                    .await;
            } else {
                refuse(ingress.unfinished()).await;
            }
            return Prepared::Refused;
        }
    };
    resolved.event.attachments = Some(staged.manifest.clone());
    // As for text, the processing reaction precedes submission, and the
    // delivered or failed reaction that follows always replaces it.
    reactions.admission(&resolved.event, settings).await;
    let submitted = submit_files(ingress, &resolved.event, deadline).await;
    typing.abort();
    // Core holds its own copy once it answers; the staged files go.
    drop(staged);
    settle_submission(ingress, &source, settings, submitted).await
}

/// Why core does not confirm holding a submission.
#[derive(Debug)]
enum NotAccepted {
    /// Core refused it, or was never sent it.
    Refused(Refusal),
    /// Core may have received it, but would not say whether it holds it.
    Unconfirmed,
}

/// Shows the sender what became of a submission, and what it settled: an
/// accepted one gets its outcome from delivery, a refused one its reason,
/// and one core would not confirm the uncertain reaction, never a refusal.
async fn settle_submission(
    ingress: &Ingress,
    source: &str,
    settings: &Arc<DiscordConfig>,
    submitted: Result<EventReceipt, NotAccepted>,
) -> Prepared {
    match submitted {
        Ok(_) => Prepared::Accepted,
        Err(NotAccepted::Refused(refusal)) => {
            explain(&ingress.http, source, &refusal.0).await;
            ingress
                .reactions
                .outcome_as(source, Feedback::Failed, settings)
                .await;
            Prepared::Refused
        }
        Err(NotAccepted::Unconfirmed) => {
            ingress
                .reactions
                .outcome_as(source, Feedback::Uncertain, settings)
                .await;
            Prepared::Unconfirmed
        }
    }
}

/// What core says of a submission it may have received without answering:
/// accepted, refused when core does not hold it, or unconfirmed when it
/// will not say.
async fn unanswered(ingress: &Ingress, event_id: &str) -> Result<EventReceipt, NotAccepted> {
    match settled(&ingress.core, event_id).await {
        Ok(Some(receipt)) => Ok(receipt),
        Ok(None) => Err(NotAccepted::Refused(ingress.unfinished())),
        Err(failure) => {
            warn!(error = %failure.message, %event_id, "core would not say whether it holds an event it may have received");
            Err(NotAccepted::Unconfirmed)
        }
    }
}

async fn keep_typing(http: Arc<DiscordHttp>, config: Arc<DiscordConfig>, channel: String) {
    loop {
        trigger_typing(&http, &config, &channel).await;
        sleep(Duration::from_secs(8)).await;
    }
}

/// What core says of a message before its files are downloaded.
enum Replayed {
    /// Core accepted it: its feedback stands.
    Held,
    /// Core does not hold it; the files are downloaded.
    Absent,
    /// The deadline or the stop came before core said it was capturing it.
    Unfinished,
    /// Core was capturing it and then would not say what became of it. The
    /// capture may still be core's, so nothing more is sent.
    Uncertain,
}

/// Ask core about `event_id`. The deadline and the stop end the lookup only
/// until core says it is capturing the message: core owns that capture, and
/// its outcome then settles the message.
async fn replayed(ingress: &Ingress, event_id: &str, deadline: Instant) -> Replayed {
    loop {
        let Some(status) = ingress
            .until(Some(deadline), ingress.core.event_status(event_id))
            .await
        else {
            return Replayed::Unfinished;
        };
        match status {
            Ok(EventStatus::Accepted(_)) => return Replayed::Held,
            Ok(EventStatus::Unknown) => return Replayed::Absent,
            Ok(EventStatus::Capturing) => break,
            Err(failure) if failure.is_transient() => {
                warn!(error = %failure.message, %event_id, "waiting to ask core about a Discord event");
                if ingress
                    .until(Some(deadline), sleep(Duration::from_secs(2)))
                    .await
                    .is_none()
                {
                    return Replayed::Unfinished;
                }
            }
            Err(failure) => {
                warn!(error = %failure.message, %event_id, "core refused an event status request");
                return Replayed::Absent;
            }
        }
    }
    match settled(&ingress.core, event_id).await {
        Ok(Some(_)) => Replayed::Held,
        // The capture ended without an acceptance.
        Ok(None) => Replayed::Absent,
        Err(failure) => {
            warn!(error = %failure.message, %event_id, "core refused an event status request");
            Replayed::Uncertain
        }
    }
}

/// The receipt of an event core already accepted. A capture still running
/// is waited for; an unreachable core is waited out, as for any submission.
pub(crate) async fn known_receipt(core: &CoreClient, event_id: &str) -> Option<EventReceipt> {
    settled(core, event_id).await.unwrap_or_else(|failure| {
        warn!(error = %failure.message, %event_id, "core refused an event status request");
        None
    })
}

/// What core settles for one of this provider's events: its receipt once
/// accepted, `None` when core does not hold it. A capture still running is
/// waited for, and an unreachable core is waited out; a refusal to answer
/// is returned.
async fn settled(
    core: &CoreClient,
    event_id: &str,
) -> Result<Option<EventReceipt>, InterfaceError> {
    loop {
        match core.event_status(event_id).await {
            Ok(EventStatus::Accepted(receipt)) => return Ok(Some(receipt)),
            Ok(EventStatus::Unknown) => return Ok(None),
            Ok(EventStatus::Capturing) => {}
            Err(failure) if failure.is_transient() => {
                warn!(error = %failure.message, %event_id, "waiting to ask core about a Discord event");
            }
            Err(failure) => return Err(failure),
        }
        sleep(Duration::from_secs(2)).await;
    }
}

/// Submit staged files until core accepts or refuses them. Retries hold no
/// slot for active work while they wait, and no attempt starts once the
/// deadline has passed or the provider is stopping, even one that waited
/// for a slot. An unanswered submission is then settled by asking core
/// rather than assumed lost, and is unconfirmed when core will not say.
async fn submit_files(
    ingress: &Ingress,
    event: &ExternalEvent,
    deadline: Instant,
) -> Result<EventReceipt, NotAccepted> {
    let mut delay = Duration::from_secs(1);
    let mut submitted = false;
    loop {
        let Some(active) = ingress
            .until(Some(deadline), ingress.downloads.active())
            .await
        else {
            break;
        };
        if Instant::now() >= deadline || ingress.stopping.is_cancelled() {
            break;
        }
        submitted = true;
        let attempt = ingress.core.submit_event(event).await;
        drop(active);
        let failure = match attempt {
            Ok(receipt) => return Ok(receipt),
            Err(failure) if !failure.is_transient() => {
                return Err(NotAccepted::Refused(refusal(&failure)));
            }
            Err(failure) => failure,
        };
        warn!(error = %failure.message, event_id = %event.event_id, "waiting to submit Discord attachments");
        if Instant::now() + delay >= deadline
            || ingress.until(Some(deadline), sleep(delay)).await.is_none()
        {
            break;
        }
        delay = (delay * 2).min(MAX_BACKOFF);
    }
    if !submitted {
        return Err(NotAccepted::Refused(ingress.unfinished()));
    }
    unanswered(ingress, &event.event_id).await
}

/// Core's refusal of files, in Discord's words for the sender.
fn refusal(failure: &InterfaceError) -> Refusal {
    Refusal(
        match failure.status {
            StatusCode::INSUFFICIENT_STORAGE => {
                "Carillon's attachment storage is full, so this message was not accepted."
            }
            StatusCode::BAD_REQUEST => {
                "Carillon could not accept these attachments, so this message was not accepted."
            }
            _ => "Carillon could not accept this message right now.",
        }
        .to_owned(),
    )
}

/// Submit `event` until core accepts or refuses it. No attempt starts once
/// the provider is `stopping`, whether or not this call made one: `None`
/// then, as core has not answered. An attempt under way runs to its end,
/// within the client's own bound.
pub(crate) async fn submit_event_with_retry(
    core: &CoreClient,
    stopping: &CancellationToken,
    event: &ExternalEvent,
) -> Option<Result<EventReceipt, InterfaceError>> {
    loop {
        // However the wait before it ended, a stop by then starts no attempt.
        if stopping.is_cancelled() {
            return None;
        }
        match core.submit_event(event).await {
            Err(failure) if failure.is_transient() => {
                warn!(error = %failure.message, event_id = %event.event_id, "waiting to submit Discord event");
                stopping.run_until_cancelled(sleep(RETRY)).await;
            }
            outcome => return Some(outcome),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
