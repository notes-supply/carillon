//! A message from the page to core, with its files.
//!
//! The page mints the event ID before its first attempt and sends it again
//! with every retry, so core answers a repeat with the first receipt and
//! nothing is submitted twice. Files stream into a staging batch of this
//! request's own, which goes when core has answered.

use std::sync::Arc;

use anyhow::Context as _;
use axum::{
    Json,
    extract::{
        Multipart, Path, Query, State,
        multipart::{Field, MultipartError, MultipartRejection},
    },
    http::StatusCode,
};
use carillon_installation::config::AttachmentLimits;
use carillon_interface::{Batch, InterfaceError, submit_event_with_retry};
use carillon_protocol::model::{
    ExternalEvent, INTENT_COMMAND, NATIVE_INTENT, StagedAttachments, StagedFile,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::{fs::File, io::AsyncWriteExt as _};

use crate::{
    browser::refusal,
    identity::{ConversationId, EventId, agent_of},
    serve::Web,
};

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

fn malformed(error: &MultipartError) -> InterfaceError {
    refusal(
        error.status(),
        &format!("The message did not arrive whole: {}", error.body_text()),
    )
}

/// What the page sent, with its files staged in the batch.
struct Staged {
    text: String,
    files: Vec<StagedFile>,
}

/// Reads the request's parts: `text`, then any number of `file`. A part
/// past core's limits is not kept, and the rest of the body is still read
/// before the refusal is given: a browser that is answered while it
/// uploads reports a broken connection and not the answer.
async fn stage(
    limits: &AttachmentLimits,
    batch: &Batch,
    mut parts: Multipart,
) -> Result<Staged, InterfaceError> {
    let mut staged = Staged {
        text: String::new(),
        files: Vec::new(),
    };
    let mut total = 0_u64;
    let mut refused = None;
    while let Some(mut part) = parts
        .next_field()
        .await
        .map_err(|error| malformed(&error))?
    {
        match part.name() {
            Some("text") => {
                staged.text = part.text().await.map_err(|error| malformed(&error))?;
            }
            Some("file") => {
                if staged.files.len() >= limits.max_files as usize {
                    refused.get_or_insert_with(|| {
                        too_large(format!(
                            "A message carries at most {} files.",
                            limits.max_files
                        ))
                    });
                }
                let name = part.file_name().unwrap_or_default().trim().to_owned();
                let content_type = part.content_type().map(str::to_owned);
                let mut file = match refused {
                    None => Some(
                        File::create(batch.file(staged.files.len()))
                            .await
                            .context("stage a file")?,
                    ),
                    Some(_) => None,
                };
                let mut size = 0_u64;
                while let Some(chunk) = chunk(&mut part).await? {
                    size += chunk.len() as u64;
                    if size > limits.max_file_bytes() || total + size > limits.max_event_bytes() {
                        refused.get_or_insert_with(|| {
                            too_large(format!(
                                "A file may have at most {} MiB, and a message's files {} MiB \
                                 together.",
                                limits.max_file_mib, limits.max_event_mib
                            ))
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
                staged.files.push(StagedFile {
                    name: if name.is_empty() {
                        "file".to_owned()
                    } else {
                        name
                    },
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
    match refused {
        Some(refused) => Err(refused),
        None => Ok(staged),
    }
}

async fn chunk(part: &mut Field<'_>) -> Result<Option<axum::body::Bytes>, InterfaceError> {
    part.chunk().await.map_err(|error| malformed(&error))
}

fn too_large(message: String) -> InterfaceError {
    InterfaceError {
        status: StatusCode::PAYLOAD_TOO_LARGE,
        message,
    }
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
    let agent_id = agent_of(&web, id, agent).await?;
    let batch = web.staging.batch().context("stage a message's files")?;
    let Staged { text, files } = stage(&web.registry.value.core.attachments, &batch, parts).await?;
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
        audience_id: id.to_string(),
        channel_id: id.to_string(),
        delivery_target_id: id.to_string(),
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
        outcome
    });
    match submission.await.context("submit a message")? {
        Some(Ok(receipt)) => Ok(Json(Sent {
            turn_id: receipt.turn_id,
            duplicate: receipt.duplicate,
        })),
        Some(Err(refused)) => Err(refused),
        None => Err(refusal(
            StatusCode::SERVICE_UNAVAILABLE,
            "Carillon's web chat is restarting. Send the message again.",
        )),
    }
}

#[cfg(test)]
mod tests;
