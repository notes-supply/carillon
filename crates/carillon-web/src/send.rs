//! A message from the page to core, with its files.
//!
//! The page mints the event ID before its first attempt and sends it again
//! with every retry, so core answers a repeat with the first receipt and
//! nothing is submitted twice. Files stream into a staging batch of this
//! request's own, which goes when core has answered.

mod bound;

use std::{path::Path as FilePath, sync::Arc};

use anyhow::Context as _;
use axum::{
    Json,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header::CONTENT_TYPE},
};
use carillon_installation::config::{AttachmentLimits, MIB};
use carillon_interface::{Batch, InterfaceError, submit_event_with_retry};
use carillon_protocol::model::{
    EVENT_BODY_LIMIT, ExternalEvent, INTENT_COMMAND, NATIVE_INTENT, RuntimeCommand,
    StagedAttachments, StagedFile,
};
use multer::{Field, Multipart};
use serde::Serialize;
use serde_json::json;
use tokio::{fs::File, io::AsyncWriteExt as _};
use tracing::warn;

use crate::{
    api::{AWAY, from_core},
    browser::refusal,
    identity::{ConversationId, ConversationKey, EventId},
    send::bound::{BodyError, Meter, Metered},
    serve::Web,
};

#[derive(Serialize)]
pub(crate) struct Sent {
    turn_id: String,
    /// Core had this message already and submitted nothing new.
    duplicate: bool,
}

/// The most the provider reads of a message it has already refused, so a
/// browser still reads the refusal where it can, without a peer draining the
/// provider with a refused upload. A refused message is normally small; only
/// an oversized text or file leaves more than this, and that is the message
/// being refused.
const REFUSED_DRAIN: u64 = 2 * MIB;

fn malformed(error: &multer::Error) -> InterfaceError {
    if let multer::Error::StreamReadFailed(source) = error
        && let Some(BodyError::Overrun) = source.downcast_ref()
    {
        return refusal(
            StatusCode::PAYLOAD_TOO_LARGE,
            "A message's parts are larger than the web chat reads.",
        );
    }
    refusal(
        StatusCode::BAD_REQUEST,
        &format!("The message did not arrive whole: {error}"),
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
async fn read_text(part: &mut Field<'static>) -> Result<Option<String>, InterfaceError> {
    let mut text = Vec::new();
    while let Some(chunk) = part.chunk().await.map_err(|error| malformed(&error))? {
        if text.len() + chunk.len() > EVENT_BODY_LIMIT {
            return Ok(None);
        }
        text.extend_from_slice(&chunk);
    }
    let text = String::from_utf8_lossy(&text);
    Ok(Some(text.trim_prefix('\u{feff}').to_owned()))
}

/// Stages a `file` part at `path`, or `None` for one of more than
/// `max_bytes`.
async fn stage_file(
    part: &mut Field<'static>,
    max_bytes: u64,
    path: &FilePath,
) -> Result<Option<StagedFile>, InterfaceError> {
    let name = match part.file_name().map(as_chosen) {
        Some(name) if !name.trim().is_empty() => name.trim().to_owned(),
        _ => "file".to_owned(),
    };
    let content_type = part.content_type().map(|mime| mime.as_ref().to_owned());
    let mut file = File::create(path).await.context("stage a file")?;
    let mut size = 0_u64;
    while let Some(chunk) = part.chunk().await.map_err(|error| malformed(&error))? {
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

/// Reads to the end of `part`, up to `budget` bytes, and says how many it
/// read. A refused message is read this far so a browser still reaching the
/// end of its upload reads the refusal and not a broken connection. A stream
/// error ends the reading: the message is already refused.
async fn drain(part: &mut Field<'static>, budget: u64) -> u64 {
    let mut read = 0_u64;
    while read < budget {
        match part.chunk().await {
            Ok(Some(chunk)) => read += chunk.len() as u64,
            _ => break,
        }
    }
    read
}

/// Reads the request's parts: at most one `text`, and `file`s within core's
/// limits, in any order. A message `refused` before its parts were read
/// keeps none of them. Otherwise the first part that is something else or
/// past a limit refuses the message, and no later part is kept.
///
/// The parts of a refused message are read to their end, up to
/// [`REFUSED_DRAIN`] in all, so a browser answered while it uploads reports a
/// broken connection and not the answer. A form that cannot be read, a file
/// that cannot be staged, or a part that outruns [`bound::BETWEEN_PARTS`] is
/// answered at once.
async fn stage(
    limits: &AttachmentLimits,
    batch: &Batch,
    mut parts: Multipart<'static>,
    meter: &Meter,
    mut refused: Option<InterfaceError>,
) -> Result<(String, Vec<StagedFile>), InterfaceError> {
    let (mut text, mut files) = (None, Vec::new());
    let (mut total, mut drained) = (0_u64, 0_u64);
    loop {
        meter.parts();
        let Some(mut part) = parts
            .next_field()
            .await
            .map_err(|error| malformed(&error))?
        else {
            break;
        };
        meter.data();
        if refused.is_none() {
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
        // Consume whatever remains of this part, so the next boundary is read
        // within its window. For a part read whole this takes nothing; for a
        // refused one it reads the rest, up to the message's drain budget.
        if refused.is_some() {
            drained += drain(&mut part, REFUSED_DRAIN.saturating_sub(drained)).await;
            if drained >= REFUSED_DRAIN {
                break;
            }
        }
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
                error = format!("{failure:#}"),
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
    headers: HeaderMap,
    body: Body,
) -> Result<Json<Sent>, InterfaceError> {
    let boundary = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|content_type| multer::parse_boundary(content_type).ok())
        .ok_or_else(|| {
            refusal(
                StatusCode::BAD_REQUEST,
                "A message is sent as multipart form data.",
            )
        })?;
    let meter = Meter::new();
    let parts = Multipart::new(Metered::new(body, Arc::clone(&meter)), boundary);
    let key = ConversationKey {
        agent_id,
        audience_id,
    };
    let batch = web.staging.batch().context("stage a message's files")?;
    let limits = &web.registry.value.core.attachments;
    let (text, files) = stage(limits, &batch, parts, &meter, key.configured(&web).err()).await?;
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
