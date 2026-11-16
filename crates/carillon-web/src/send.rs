//! A message from the page to core, with its files.
//!
//! The page mints the event ID before its first attempt and sends it again
//! with every retry, so core answers a repeat with the first receipt and
//! nothing is submitted twice. Files stream into a staging batch of this
//! request's own, which goes when core has answered.

use std::{path::Path as FilePath, sync::Arc};

use anyhow::Context as _;
use axum::{
    Json,
    extract::{
        Multipart, Path, State,
        multipart::{Field, MultipartError, MultipartRejection},
    },
    http::StatusCode,
};
use carillon_installation::config::{AttachmentLimits, MIB};
use carillon_interface::{Batch, InterfaceError, submit_event_with_retry};
use carillon_protocol::model::{
    EVENT_BODY_LIMIT, ExternalEvent, INTENT_COMMAND, NATIVE_INTENT, RuntimeCommand,
    StagedAttachments, StagedFile,
};
use serde::Serialize;
use serde_json::json;
use tokio::{fs::File, io::AsyncWriteExt as _};
use tracing::warn;

use crate::{
    api::{AWAY, from_core},
    browser::refusal,
    identity::{ConversationId, ConversationKey, EventId},
    serve::Web,
};

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

fn event_too_large() -> InterfaceError {
    refusal(
        StatusCode::PAYLOAD_TOO_LARGE,
        &format!(
            "A message takes about {} MiB of text at most.",
            EVENT_BODY_LIMIT as u64 / MIB
        ),
    )
}

/// A `text` part, or `None` for one longer than any event core reads. A
/// leading byte-order mark is not part of what the person wrote.
async fn read_text(part: &mut Field<'_>) -> Result<Option<String>, InterfaceError> {
    let mut text = Vec::new();
    while let Some(chunk) = part.chunk().await.map_err(malformed)? {
        if text.len() + chunk.len() > EVENT_BODY_LIMIT {
            return Ok(None);
        }
        text.extend_from_slice(&chunk);
    }
    let text = String::from_utf8_lossy(&text);
    Ok(Some(
        text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned(),
    ))
}

/// Stages a `file` part at `path`, or `None` for one of more than
/// `max_bytes`.
async fn stage_file(
    part: &mut Field<'_>,
    max_bytes: u64,
    path: &FilePath,
) -> Result<Option<StagedFile>, InterfaceError> {
    let name = match part.file_name().map(as_chosen) {
        Some(name) if !name.trim().is_empty() => name.trim().to_owned(),
        _ => "file".to_owned(),
    };
    let content_type = part.content_type().map(str::to_owned);
    let mut file = File::create(path).await.context("stage a file")?;
    let mut size = 0_u64;
    while let Some(chunk) = part.chunk().await.map_err(malformed)? {
        size += chunk.len() as u64;
        if size > max_bytes {
            return Ok(None);
        }
        file.write_all(&chunk).await.context("stage a file")?;
    }
    file.flush().await.context("stage a file")?;
    Ok(Some(StagedFile {
        name,
        size,
        content_type,
    }))
}

/// Reads the request's parts: at most one `text`, and `file`s within
/// core's limits, in any order. A message `refused` before its parts were
/// read keeps none of them. Otherwise the first part that is something
/// else or past a limit refuses the message, and no later part is kept.
/// The later parts are still read before the refusal is given: a browser
/// that is answered while it uploads reports a broken connection and not
/// the answer. A form that cannot be read, or a file that cannot be
/// staged, is answered at once.
async fn stage(
    limits: &AttachmentLimits,
    batch: &Batch,
    mut parts: Multipart,
    mut refused: Option<InterfaceError>,
) -> Result<(String, Vec<StagedFile>), InterfaceError> {
    let (mut text, mut files) = (None, Vec::new());
    let mut total = 0_u64;
    while let Some(mut part) = parts.next_field().await.map_err(malformed)? {
        if refused.is_some() {
            continue;
        }
        refused = match part.name() {
            Some("text") if text.is_none() => {
                text = read_text(&mut part).await?;
                text.is_none().then(event_too_large)
            }
            Some("text") => Some(refusal(
                StatusCode::BAD_REQUEST,
                "A message has one `text` part.",
            )),
            Some("file") if files.len() >= limits.max_files as usize => Some(refusal(
                StatusCode::PAYLOAD_TOO_LARGE,
                &format!("A message carries at most {} files.", limits.max_files),
            )),
            Some("file") => {
                let max_bytes = limits
                    .max_file_bytes()
                    .min(limits.max_event_bytes() - total);
                match stage_file(&mut part, max_bytes, &batch.file(files.len())).await? {
                    Some(file) => {
                        total += file.size;
                        files.push(file);
                        None
                    }
                    None => Some(refusal(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        &format!(
                            "A file may have at most {} MiB, and a message's files {} MiB \
                             together.",
                            limits.max_file_mib, limits.max_event_mib
                        ),
                    )),
                }
            }
            _ => Some(refusal(
                StatusCode::BAD_REQUEST,
                "A message's parts are `text` and `file`.",
            )),
        };
    }
    match refused {
        Some(refused) => Err(refused),
        None => Ok((text.unwrap_or_default(), files)),
    }
}

/// Whether `text` invokes one of `commands`: `/` and a command's name, and
/// then nothing or its arguments.
fn invokes(text: &str, commands: &[RuntimeCommand]) -> bool {
    text.split(char::is_whitespace)
        .next()
        .and_then(|first| first.strip_prefix('/'))
        .is_some_and(|name| commands.iter().any(|command| command.name == name))
}

/// Whether the message invokes one of the commands the conversation's
/// runtime offers. Only the runtime knows its commands, so a text that
/// begins with `/` asks it, once. A runtime that does not say leaves the
/// text a message: the person's words reach the agent either way, and no
/// message waits for a list.
async fn is_command(web: &Web, key: &ConversationKey, text: &str) -> bool {
    if !text.starts_with('/') {
        return false;
    }
    match web.core.commands(&key.query()).await {
        Ok(offered) => invokes(text, &offered.commands),
        Err(failure) => {
            warn!(
                status = %failure.status,
                error = %failure.message,
                "the runtime's commands could not be read, so a text is sent as a message"
            );
            false
        }
    }
}

/// `PUT /api/conversations/{agent_id}/{audience_id}/messages/{event_id}`:
/// submits the message and answers once core has it.
pub(crate) async fn send(
    State(web): State<Arc<Web>>,
    Path((agent_id, audience_id, event_id)): Path<(String, ConversationId, EventId)>,
    parts: Result<Multipart, MultipartRejection>,
) -> Result<Json<Sent>, InterfaceError> {
    let parts = parts.map_err(|rejection| {
        refusal(
            rejection.status(),
            "A message is sent as multipart form data.",
        )
    })?;
    let key = ConversationKey {
        agent_id,
        audience_id,
    };
    let batch = web.staging.batch().context("stage a message's files")?;
    let limits = &web.registry.value.core.attachments;
    let (text, files) = stage(limits, &batch, parts, key.configured(&web).err()).await?;
    if text.trim().is_empty() && files.is_empty() {
        return Err(refusal(
            StatusCode::BAD_REQUEST,
            "A message needs text or a file.",
        ));
    }
    let command = is_command(&web, &key, &text).await;
    let audience = key.audience_id.as_str();
    let event = ExternalEvent {
        event_id: event_id.as_str().to_owned(),
        user_id: web.user.clone(),
        audience_id: audience.to_owned(),
        channel_id: audience.to_owned(),
        delivery_target_id: audience.to_owned(),
        feedback_target_id: event_id.as_str().to_owned(),
        agent_id: key.agent_id,
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
    // JSON escapes the text, so text read within the limit can make an
    // event past it.
    let event_bytes = serde_json::to_vec(&event).context("write a message as an event")?;
    if event_bytes.len() > EVENT_BODY_LIMIT {
        return Err(event_too_large());
    }
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
        Some(Err(refused)) => Err(from_core(refused)),
        None => Err(refusal(StatusCode::SERVICE_UNAVAILABLE, AWAY)),
    }
}

#[cfg(test)]
mod tests;
