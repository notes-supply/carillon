//! Native message preparation, feedback and durable submission.
use crate::{
    InterfaceError,
    client::CoreClient,
    config::DiscordConfig,
    conversations::{
        IncomingMessage, Participation, admit_message, discord_parent_channel, prepare_conversation,
    },
    delivery::{
        Feedback, clear_processing_feedback, processing_feedback, start_typing, terminal_feedback,
    },
    error::discord_failure_is_terminal,
};
use anyhow::Result;
use carillon_protocol::model::{EventReceipt, ExternalEvent};
use serde_json::Value;
use std::{sync::Arc, time::Duration};
use tokio::{sync::mpsc, time::sleep};
use tracing::warn;
use twilight_http::Client as DiscordHttp;

#[derive(Clone, Debug)]
pub(crate) struct QueuedMessage {
    pub(crate) request: IncomingMessage,
    pub(crate) native: Value,
}

pub(crate) async fn message_worker(
    core: CoreClient,
    config: DiscordConfig,
    participation: Arc<Participation>,
    http: Arc<DiscordHttp>,
    mut messages: mpsc::Receiver<QueuedMessage>,
) {
    'messages: while let Some(mut inbound) = messages.recv().await {
        let discord = &config;
        inbound.request.parent_channel_id = loop {
            match discord_parent_channel(
                &http,
                &config,
                &participation,
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
                    terminal_feedback(&http, discord, &source, Feedback::Failed).await;
                    warn!(error = %failure, "Discord message route is unavailable");
                    continue 'messages;
                }
                Err(failure) => {
                    warn!(error = %failure, "waiting to resolve Discord message route");
                    sleep(Duration::from_millis(250)).await;
                }
            }
        };
        let Ok(Some(plan)) =
            admit_message(discord, &participation, inbound.request, inbound.native)
        else {
            continue;
        };
        let source = format!("{}:{}", plan.channel_id, plan.message_id);
        let resolved = loop {
            match prepare_conversation(&participation, &http, plan.clone()).await {
                Ok(resolved) => break resolved,
                Err(failure) if discord_failure_is_terminal(&failure) => {
                    terminal_feedback(&http, discord, &source, Feedback::Failed).await;
                    warn!(error = %failure, "Discord conversation cannot be prepared");
                    continue 'messages;
                }
                Err(failure) => {
                    warn!(error = %failure, "waiting to prepare Discord conversation");
                    sleep(Duration::from_millis(250)).await;
                }
            }
        };
        processing_feedback(&http, discord, &resolved.event.feedback_target_id).await;
        let typing = start_typing(http.clone(), discord, &resolved.event.delivery_target_id).await;
        let receipt = submit_event_with_retry(&core, &resolved.event).await;
        if let Some(task) = typing {
            task.abort();
        }
        match receipt {
            Ok(receipt) if receipt.duplicate => {
                clear_processing_feedback(&http, discord, &resolved.event.feedback_target_id).await;
            }
            Ok(_) => {}
            Err(failure) => {
                warn!(error = %failure.message, "Discord event submission failed");
                terminal_feedback(
                    &http,
                    discord,
                    &resolved.event.feedback_target_id,
                    Feedback::Failed,
                )
                .await;
            }
        }
    }
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
