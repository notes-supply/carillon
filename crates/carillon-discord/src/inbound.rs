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
        IncomingMessage, MessagePlan, Participation, ResolvedMessage, admit_message,
        discord_parent_channel, prepare_conversation,
    },
    delivery::{
        Feedback, clear_processing_feedback, explain, processing_feedback, terminal_feedback,
        trigger_typing,
    },
    error::discord_failure_is_terminal,
};
use anyhow::Result;
use axum::http::StatusCode;
use carillon_protocol::model::{EventReceipt, EventStatus, ExternalEvent};
use serde_json::Value;
use std::{sync::Arc, time::Duration};
use tokio::time::{Instant, sleep};
use tracing::warn;
use twilight_http::Client as DiscordHttp;

pub(crate) mod lanes;

/// How long a message's files may take, from admission to core's answer.
const PREPARATION_DEADLINE: Duration = Duration::from_mins(30);
/// The longest wait between attempts to submit downloaded files.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

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
    pub(crate) downloads: Downloads,
}

/// Settle one gateway message: by the time this returns it was accepted,
/// refused with feedback, or ignored by admission.
pub(crate) async fn prepare(ingress: &Ingress, mut inbound: QueuedMessage) {
    let discord = &ingress.config;
    let http = &ingress.http;
    inbound.request.parent_channel_id = loop {
        match discord_parent_channel(
            http,
            discord,
            &ingress.participation,
            &inbound.request.channel_id,
        )
        .await
        {
            Ok(parent) => break parent,
            Err(failure) if discord_failure_is_terminal(&failure) => {
                let source = format!(
                    "{}:{}",
                    inbound.request.channel_id, inbound.request.event_id
                );
                terminal_feedback(http, discord, &source, Feedback::Failed).await;
                warn!(error = %failure, "Discord message route is unavailable");
                return;
            }
            Err(failure) => {
                warn!(error = %failure, "waiting to resolve Discord message route");
                sleep(Duration::from_millis(250)).await;
            }
        }
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
    let (discord, http) = (&ingress.config, &*ingress.http);
    let source = format!("{}:{}", plan.channel_id, plan.message_id);
    let Some(resolved) = open_conversation(ingress, plan, &source).await else {
        return;
    };
    processing_feedback(http, discord, &resolved.event.feedback_target_id).await;
    trigger_typing(http, discord, &resolved.event.delivery_target_id).await;
    let receipt = submit_event_with_retry(&ingress.core, &resolved.event).await;
    match receipt {
        Ok(receipt) if receipt.duplicate => {
            clear_processing_feedback(http, discord, &resolved.event.feedback_target_id).await;
        }
        Ok(_) => {}
        Err(failure) => {
            warn!(error = %failure.message, "Discord event submission failed");
            terminal_feedback(
                http,
                discord,
                &resolved.event.feedback_target_id,
                Feedback::Failed,
            )
            .await;
        }
    }
}

/// Download, stage and submit a message with files. Its processing reaction
/// appears only once core has accepted the files; while they download the
/// sender sees typing.
async fn prepare_files(ingress: &Ingress, plan: MessagePlan) {
    let (discord, http) = (&ingress.config, &*ingress.http);
    let source = format!("{}:{}", plan.channel_id, plan.message_id);
    let refuse = async |refusal: Refusal| {
        explain(http, &source, &refusal.0).await;
        terminal_feedback(http, discord, &source, Feedback::Failed).await;
    };
    if let Err(refusal) = ingress.downloads.admit(&plan.attachments) {
        refuse(refusal).await;
        return;
    }
    // A message core already holds, such as one the gateway replayed, is
    // not downloaded again.
    if known_receipt(&ingress.core, &plan.message_id)
        .await
        .is_some()
    {
        return;
    }
    let typing = tokio::spawn(keep_typing(
        ingress.http.clone(),
        ingress.config.clone(),
        plan.channel_id.clone(),
    ));
    let typing = typing.abort_handle();
    let deadline = Instant::now() + PREPARATION_DEADLINE;
    let staged = match ingress.downloads.stage(http, &plan, deadline).await {
        Ok(staged) => staged,
        Err(refusal) => {
            typing.abort();
            refuse(refusal).await;
            return;
        }
    };
    let Some(mut resolved) = open_conversation(ingress, plan, &source).await else {
        typing.abort();
        return;
    };
    resolved.event.attachments = Some(staged.manifest.clone());
    let settled = submit_files(ingress, &resolved.event, deadline).await;
    typing.abort();
    // Core holds its own copy once it answers; the staged files go.
    drop(staged);
    match settled {
        Ok(receipt) if !receipt.duplicate => {
            processing_feedback(http, discord, &resolved.event.feedback_target_id).await;
        }
        Ok(_) => {}
        Err(refusal) => refuse(refusal).await,
    }
}

/// Create or adopt the message's conversation, retrying what may succeed.
async fn open_conversation(
    ingress: &Ingress,
    plan: MessagePlan,
    source: &str,
) -> Option<ResolvedMessage> {
    loop {
        match prepare_conversation(&ingress.participation, &ingress.http, plan.clone()).await {
            Ok(resolved) => return Some(resolved),
            Err(failure) if discord_failure_is_terminal(&failure) => {
                terminal_feedback(&ingress.http, &ingress.config, source, Feedback::Failed).await;
                warn!(error = %failure, "Discord conversation cannot be prepared");
                return None;
            }
            Err(failure) => {
                warn!(error = %failure, "waiting to prepare Discord conversation");
                sleep(Duration::from_millis(250)).await;
            }
        }
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
async fn known_receipt(core: &CoreClient, event_id: &str) -> Option<EventReceipt> {
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
/// slot for active work while they wait; once the deadline has passed, an
/// unanswered submission is settled by asking core rather than assumed lost.
async fn submit_files(
    ingress: &Ingress,
    event: &ExternalEvent,
    deadline: Instant,
) -> Result<EventReceipt, Refusal> {
    let mut delay = Duration::from_secs(1);
    loop {
        let attempt = {
            let _active = ingress.downloads.active().await;
            ingress.core.submit_event(event).await
        };
        let failure = match attempt {
            Ok(receipt) => return Ok(receipt),
            Err(failure) if !failure.is_transient() => return Err(refusal(&failure)),
            Err(failure) => failure,
        };
        warn!(error = %failure.message, event_id = %event.event_id, "waiting to submit Discord attachments");
        if Instant::now() + delay > deadline {
            return known_receipt(&ingress.core, &event.event_id)
                .await
                .ok_or_else(|| {
                    Refusal(
                        "Carillon could not accept these attachments in time, so this message was not accepted. Please send it again."
                            .to_owned(),
                    )
                });
        }
        sleep(delay).await;
        delay = (delay * 2).min(MAX_BACKOFF);
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

pub(crate) async fn submit_event_with_retry(
    core: &CoreClient,
    event: &ExternalEvent,
) -> Result<EventReceipt, InterfaceError> {
    loop {
        match core.submit_event(event).await {
            Err(failure) if failure.is_transient() => {
                warn!(error = %failure.message, event_id = %event.event_id, "waiting to submit Discord event");
                sleep(Duration::from_millis(250)).await;
            }
            outcome => return outcome,
        }
    }
}

#[cfg(test)]
mod tests;
