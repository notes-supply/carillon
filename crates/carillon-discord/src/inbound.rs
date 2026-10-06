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
};
use anyhow::{Result, anyhow};
use axum::http::StatusCode;
use carillon_protocol::model::{EventReceipt, EventStatus, ExternalEvent};
use serde_json::Value;
use std::{future::pending, sync::Arc, time::Duration};
use tokio::time::{Instant, sleep, sleep_until, timeout};
use tokio_util::sync::CancellationToken;
use tracing::warn;
use twilight_http::Client as DiscordHttp;

pub(crate) mod lanes;

/// How long a message's files may take from admission until they are
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
const LATE: &str = "Carillon could not accept these attachments in time, so this message was not accepted. Please send it again.";
/// Why a message still being prepared when the provider stopped was not accepted.
const STOPPED: &str =
    "Carillon was restarting, so this message was not accepted. Please send it again.";

#[derive(Clone, Debug)]
pub(crate) struct QueuedMessage {
    pub(crate) request: IncomingMessage,
    pub(crate) native: Value,
}

/// What preparing a message needs, shared by every lane.
pub(crate) struct Ingress {
    pub(crate) core: CoreClient,
    pub(crate) config: DiscordConfig,
    pub(crate) participation: Arc<Participation>,
    pub(crate) http: Arc<DiscordHttp>,
    /// The reactions that show each message's progress, in order.
    pub(crate) reactions: Reactions,
    pub(crate) downloads: Arc<Downloads>,
    /// Cancelled when the provider stops: work not yet handed to core ends,
    /// and a submission core may have received is settled first.
    pub(crate) stopping: CancellationToken,
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

    /// Why preparation that ended early did not finish.
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
/// refused with feedback, or ignored by admission.
pub(crate) async fn prepare(ingress: &Ingress, mut inbound: QueuedMessage) {
    let discord = &ingress.config;
    let http = &ingress.http;
    let channel = &inbound.request.channel_id;
    let route = ingress
        .retry(None, "resolve a Discord message route", || {
            discord_parent_channel(http, discord, &ingress.participation, channel)
        })
        .await;
    inbound.request.parent_channel_id = match route {
        Some(Ok(parent)) => parent,
        Some(Err(failure)) => {
            let source = format!("{channel}:{}", inbound.request.event_id);
            ingress.reactions.outcome(&source, Feedback::Failed).await;
            warn!(error = %failure, "Discord message route is unavailable");
            return;
        }
        None => return,
    };
    let Ok(Some(plan)) = admit_message(
        discord,
        &ingress.participation,
        inbound.request,
        inbound.native,
    ) else {
        return;
    };
    if plan.attachments.is_empty() {
        prepare_text(ingress, plan).await;
    } else {
        prepare_files(ingress, plan).await;
    }
}

async fn prepare_text(ingress: &Ingress, plan: MessagePlan) {
    let (reactions, http) = (&ingress.reactions, &*ingress.http);
    let source = format!("{}:{}", plan.channel_id, plan.message_id);
    let stopped = async || {
        explain(http, &source, &ingress.unfinished().0).await;
        reactions.outcome(&source, Feedback::Failed).await;
    };
    // A message core already holds, such as one the gateway replayed, is
    // not admitted again: its feedback belongs to its first admission. A
    // stop ends the lookup and starts no submission; core's answer then
    // decides the feedback, which the reactions own like any other.
    match ingress
        .until(None, known_receipt(&ingress.core, &plan.message_id))
        .await
    {
        Some(None) => {}
        Some(Some(_)) => return,
        None => {
            reactions.spawn(settle_stopped_text(
                ingress.core.clone(),
                reactions.clone(),
                ingress.http.clone(),
                plan.message_id,
                source,
                ingress.unfinished(),
            ));
            return;
        }
    }
    let opened = ingress
        .retry(None, "prepare a Discord conversation", || {
            prepare_conversation(&ingress.participation, &ingress.http, plan.clone())
        })
        .await;
    let resolved = match opened {
        Some(Ok(resolved)) => resolved,
        Some(Err(failure)) => {
            warn!(error = %failure, "Discord conversation cannot be prepared");
            reactions.outcome(&source, Feedback::Failed).await;
            return;
        }
        None => {
            stopped().await;
            return;
        }
    };
    reactions.admission(&resolved.event).await;
    // Unless the provider has already stopped, submission makes an attempt
    // core may receive. A stop cuts none short; when core has not answered
    // by then, it is asked whether it holds the event, as for files.
    let attempted = !ingress.stopping.is_cancelled();
    let receipt =
        match submit_event_with_retry(&ingress.core, &ingress.stopping, &resolved.event).await {
            Some(receipt) => Some(receipt),
            None if attempted => known_receipt(&ingress.core, &resolved.event.event_id)
                .await
                .map(Ok),
            None => None,
        };
    let Some(receipt) = receipt else {
        stopped().await;
        return;
    };
    // Accepted, even as a duplicate of its own earlier attempt, the message
    // gets its outcome from delivery.
    if let Err(failure) = receipt {
        warn!(error = %failure.message, "Discord event submission failed");
        reactions
            .outcome(&resolved.event.feedback_target_id, Feedback::Failed)
            .await;
    }
}

/// Feedback for a text stopped before core said whether it holds it: none
/// when core holds it, as its first admission's feedback stands; the
/// explanation and failure when core does not; uncertain when core cannot
/// say.
async fn settle_stopped_text(
    core: CoreClient,
    reactions: Reactions,
    http: Arc<DiscordHttp>,
    event_id: String,
    source: String,
    refusal: Refusal,
) {
    match core.event_status(&event_id).await {
        Ok(EventStatus::Accepted(_) | EventStatus::Capturing) => {}
        Ok(EventStatus::Unknown) => {
            explain(&http, &source, &refusal.0).await;
            reactions.outcome(&source, Feedback::Failed).await;
        }
        Err(failure) => {
            warn!(error = %failure.message, %event_id, "Discord stopped before core said whether it holds a message");
            reactions.outcome(&source, Feedback::Uncertain).await;
        }
    }
}

/// Download, stage and submit a message with files. While they download the
/// sender sees typing; the processing reaction appears once they are handed
/// to core. Everything before submission ends by the preparation deadline.
async fn prepare_files(ingress: &Ingress, plan: MessagePlan) {
    let (reactions, http) = (&ingress.reactions, &*ingress.http);
    let source = format!("{}:{}", plan.channel_id, plan.message_id);
    let refuse = async |refusal: Refusal| {
        explain(http, &source, &refusal.0).await;
        reactions.outcome(&source, Feedback::Failed).await;
    };
    if let Err(refusal) = ingress.downloads.admit(&plan.attachments) {
        refuse(refusal).await;
        return;
    }
    let deadline = Instant::now() + PREPARATION_DEADLINE;
    // A message core already holds or is still capturing, such as one the
    // gateway replayed, is not downloaded again.
    match ingress
        .until(
            Some(deadline),
            known_receipt(&ingress.core, &plan.message_id),
        )
        .await
    {
        Some(Some(_)) => return,
        Some(None) => {}
        None => {
            refuse(ingress.unfinished()).await;
            return;
        }
    }
    let typing = tokio::spawn(keep_typing(
        ingress.http.clone(),
        ingress.config.clone(),
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
            return;
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
                reactions.outcome(&source, Feedback::Failed).await;
            } else {
                refuse(ingress.unfinished()).await;
            }
            return;
        }
    };
    resolved.event.attachments = Some(staged.manifest.clone());
    // As for text, the processing reaction precedes submission, and the
    // delivered or failed reaction that follows always replaces it.
    reactions.admission(&resolved.event).await;
    let settled = submit_files(ingress, &resolved.event, deadline).await;
    typing.abort();
    // Core holds its own copy once it answers; the staged files go.
    drop(staged);
    if let Err(refusal) = settled {
        refuse(refusal).await;
    }
}

async fn keep_typing(http: Arc<DiscordHttp>, config: DiscordConfig, channel: String) {
    loop {
        trigger_typing(&http, &config, &channel).await;
        sleep(Duration::from_secs(8)).await;
    }
}

/// The receipt of an event core already accepted. A capture still running
/// is waited for; an unreachable core is waited out, as for any submission.
pub(crate) async fn known_receipt(core: &CoreClient, event_id: &str) -> Option<EventReceipt> {
    loop {
        match core.event_status(event_id).await {
            Ok(EventStatus::Accepted(receipt)) => return Some(receipt),
            Ok(EventStatus::Unknown) => return None,
            Ok(EventStatus::Capturing) => {}
            Err(failure) if failure.is_transient() => {
                warn!(error = %failure.message, %event_id, "waiting to ask core about a Discord event");
            }
            Err(failure) => {
                warn!(error = %failure.message, %event_id, "core refused an event status request");
                return None;
            }
        }
        sleep(Duration::from_secs(2)).await;
    }
}

/// Submit staged files until core accepts or refuses them. Retries hold no
/// slot for active work while they wait, and no attempt starts once the
/// deadline has passed or the provider is stopping, even one that waited
/// for a slot. An unanswered submission is then settled by asking core
/// rather than assumed lost.
async fn submit_files(
    ingress: &Ingress,
    event: &ExternalEvent,
    deadline: Instant,
) -> Result<EventReceipt, Refusal> {
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
            Err(failure) if !failure.is_transient() => return Err(refusal(&failure)),
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
    if submitted {
        known_receipt(&ingress.core, &event.event_id)
            .await
            .ok_or_else(|| ingress.unfinished())
    } else {
        Err(ingress.unfinished())
    }
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
