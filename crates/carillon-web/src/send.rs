//! A message from the page to core, with its files.
//!
//! The page mints the event ID before its first attempt and sends it again
//! with every retry, so core answers a repeat with the first receipt and
//! nothing is submitted twice. Files stream into a staging batch of this
//! request's own, which goes when core has answered.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError, Weak},
    time::Duration,
};

use anyhow::Context as _;
use axum::{
    Json,
    extract::{
        Multipart, Path, Query, State,
        multipart::{MultipartError, MultipartRejection},
    },
    http::StatusCode,
};
use carillon_installation::config::AttachmentLimits;
use carillon_interface::{Batch, InterfaceError, submit_event_with_retry};
use carillon_protocol::{
    conversations::ConversationSummary,
    model::{ExternalEvent, INTENT_COMMAND, NATIVE_INTENT, StagedAttachments, StagedFile},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::{
    fs::File,
    io::AsyncWriteExt as _,
    sync::{Mutex as Turn, OwnedMutexGuard},
    time::sleep,
};
use tracing::warn;

use crate::{
    api::{AWAY, from_core},
    browser::refusal,
    identity::{ConversationId, EventId, agent_of},
    serve::Web,
};

/// One lock for each conversation with a send under way. Core keys a
/// conversation by its agent too, so it would take two first messages
/// that name different agents, and the conversation would then have both.
/// A send holds its conversation's lock from its look at core to core's
/// receipt, so the second finds what the first began. Other conversations
/// are not held up.
#[derive(Default)]
pub(crate) struct Sends(Mutex<HashMap<String, Weak<Turn<()>>>>);

impl Sends {
    async fn turn(&self, id: &ConversationId) -> OwnedMutexGuard<()> {
        let turn = {
            let mut waiting = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            waiting.retain(|_, turn| turn.strong_count() > 0);
            let held = waiting.entry(id.as_str().to_owned()).or_default();
            let turn = held.upgrade().unwrap_or_default();
            *held = Arc::downgrade(&turn);
            turn
        };
        turn.lock_owned().await
    }
}

const RETRY: Duration = Duration::from_millis(250);

/// What core has of conversation `id`, asked until core answers. Core is
/// away for a moment at every restart, and the submission that follows
/// waits for it the same way.
async fn found(web: &Web, id: &ConversationId) -> Result<Vec<ConversationSummary>, InterfaceError> {
    loop {
        match web.core.conversations_of(id.as_str()).await {
            Err(failure) if failure.is_transient() && !web.stopping.is_cancelled() => {
                warn!(error = %failure.message, "waiting to read a conversation from core");
                web.stopping.run_until_cancelled(sleep(RETRY)).await;
            }
            found => return found.map_err(from_core),
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct Sending {
    /// The agent of a conversation's first message.
    agent: Option<String>,
    /// The text invokes one of the runtime's commands.
    #[serde(default)]
    command: bool,
}

#[derive(Serialize)]
pub(crate) struct Sent {
    turn_id: String,
    /// Core had this message already and submitted nothing new.
    duplicate: bool,
}

#[expect(clippy::needless_pass_by_value, reason = "as `map_err` passes it")]
fn malformed(error: MultipartError) -> InterfaceError {
    refusal(
        error.status(),
        &format!("The message did not arrive whole: {}", error.body_text()),
    )
}

/// A file's name as the person's file has it. A browser writes `"`, CR and
/// LF in a form's file name as `%22`, `%0D` and `%0A`, as the HTML standard
/// has it, and nothing else that way, so only those three are read back.
fn as_chosen(name: &str) -> String {
    name.replace("%22", "\"")
        .replace("%0D", "\r")
        .replace("%0A", "\n")
}

/// Reads the request's parts: `text`, then any number of `file`. A part
/// past core's limits is not kept, and the rest of the body is still read
/// before the refusal is given: a browser that is answered while it
/// uploads reports a broken connection and not the answer.
async fn stage(
    limits: &AttachmentLimits,
    batch: &Batch,
    mut parts: Multipart,
) -> Result<(String, Vec<StagedFile>), InterfaceError> {
    let (mut text, mut files) = (String::new(), Vec::new());
    let mut total = 0_u64;
    let mut refused = None;
    while let Some(mut part) = parts.next_field().await.map_err(malformed)? {
        match part.name() {
            Some("text") => text = part.text().await.map_err(malformed)?,
            Some("file") => {
                if files.len() >= limits.max_files as usize {
                    refused.get_or_insert_with(|| {
                        refusal(
                            StatusCode::PAYLOAD_TOO_LARGE,
                            &format!("A message carries at most {} files.", limits.max_files),
                        )
                    });
                }
                let name = match part.file_name().map(as_chosen) {
                    Some(name) if !name.trim().is_empty() => name.trim().to_owned(),
                    _ => "file".to_owned(),
                };
                let content_type = part.content_type().map(str::to_owned);
                let mut file = match refused {
                    None => Some(
                        File::create(batch.file(files.len()))
                            .await
                            .context("stage a file")?,
                    ),
                    Some(_) => None,
                };
                let mut size = 0_u64;
                while let Some(chunk) = part.chunk().await.map_err(malformed)? {
                    size += chunk.len() as u64;
                    if size > limits.max_file_bytes() || total + size > limits.max_event_bytes() {
                        refused.get_or_insert_with(|| {
                            refusal(
                                StatusCode::PAYLOAD_TOO_LARGE,
                                &format!(
                                    "A file may have at most {} MiB, and a message's files {} \
                                     MiB together.",
                                    limits.max_file_mib, limits.max_event_mib
                                ),
                            )
                        });
                        file = None;
                    }
                    if let Some(file) = &mut file {
                        file.write_all(&chunk).await.context("stage a file")?;
                    }
                }
                if let Some(mut file) = file {
                    file.flush().await.context("stage a file")?;
                }
                total += size;
                files.push(StagedFile {
                    name,
                    size,
                    content_type,
                });
            }
            _ => {
                return Err(refusal(
                    StatusCode::BAD_REQUEST,
                    "A message's parts are `text` and `file`.",
                ));
            }
        }
    }
    refused.map_or(Ok((text, files)), Err)
}

/// `PUT /api/conversations/{id}/messages/{event_id}[?agent=][&command=true]`:
/// submits the message and answers once core has it.
pub(crate) async fn send(
    State(web): State<Arc<Web>>,
    Path((id, event_id)): Path<(ConversationId, EventId)>,
    Query(Sending { agent, command }): Query<Sending>,
    parts: Result<Multipart, MultipartRejection>,
) -> Result<Json<Sent>, InterfaceError> {
    let parts = parts.map_err(|rejection| {
        refusal(
            rejection.status(),
            "A message is sent as multipart form data.",
        )
    })?;
    let turn = web.sends.turn(&id).await;
    let agent_id = agent_of(&web, found(&web, &id).await?, agent)?;
    let batch = web.staging.batch().context("stage a message's files")?;
    let (text, files) = stage(&web.registry.value.core.attachments, &batch, parts).await?;
    if text.trim().is_empty() && files.is_empty() {
        return Err(refusal(
            StatusCode::BAD_REQUEST,
            "A message needs text or a file.",
        ));
    }
    let event = ExternalEvent {
        event_id: event_id.as_str().to_owned(),
        agent_id,
        user_id: web.user.clone(),
        audience_id: id.as_str().to_owned(),
        channel_id: id.as_str().to_owned(),
        delivery_target_id: id.as_str().to_owned(),
        feedback_target_id: event_id.as_str().to_owned(),
        content: text,
        native: if command {
            json!({ NATIVE_INTENT: INTENT_COMMAND })
        } else {
            json!({})
        },
        attachments: (!files.is_empty()).then(|| StagedAttachments {
            batch: batch.name().to_owned(),
            files,
        }),
    };
    // Core copies the files before it answers, which can take minutes. The
    // submission runs in a task of its own, so a browser that leaves does
    // not end it and take the staged files from under core.
    let submission = tokio::spawn(async move {
        let outcome = submit_event_with_retry(&web.core, &web.stopping, &event).await;
        let _ = tokio::task::spawn_blocking(move || drop(batch)).await;
        drop(turn);
        outcome
    });
    match submission.await.context("submit a message")? {
        Some(Ok(receipt)) => Ok(Json(Sent {
            turn_id: receipt.turn_id,
            duplicate: receipt.duplicate,
        })),
        Some(Err(refused)) => Err(from_core(refused)),
        None => Err(refusal(StatusCode::SERVICE_UNAVAILABLE, AWAY)),
    }
}

#[cfg(test)]
mod tests;
