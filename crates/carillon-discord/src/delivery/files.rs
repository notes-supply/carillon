//! An answer's files on Discord: which message carries each, and their
//! bytes, read from core's copies.
//!
//! Discord takes files of up to 20 MiB by default, from everyone, at most 10
//! and 25 MiB in all per message
//! (<https://docs.discord.com/developers/reference#uploading-files>,
//! <https://docs.discord.com/developers/resources/message#create-message>,
//! and error 30015 of
//! <https://docs.discord.com/developers/topics/opcodes-and-status-codes>).
//! A server's boosts can raise the limit for one file, but only interactions
//! report a server's limit in bytes (`attachment_size_limit`), and a
//! message's request stays within 25 MiB, so the default is the limit here.

use std::path::Path;

use anyhow::{Context as _, Result, bail};
use carillon_installation::config::MIB;
use carillon_protocol::model::AnswerFile;
use sha2::{Digest as _, Sha256};
use twilight_model::http::attachment::Attachment;

use super::discord_chunks;

const FILE_LIMIT: u64 = 20 * MIB;
const FILES_PER_MESSAGE: usize = 10;
/// A message's files, leaving room in its 25 MiB request for its text and
/// the request's framing.
const MESSAGE_FILE_BYTES: u64 = 25 * MIB - 64 * 1024;

/// One Discord message of a delivery: its text, possibly empty, and the
/// files it carries.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Outgoing<'a> {
    pub(crate) content: String,
    pub(crate) files: Vec<&'a AnswerFile>,
}

/// The messages that send `content` with `files`, in order. The text is
/// split at Discord's length limit, with a line for each file over Discord's
/// upload limit; the first files go on the text's last message, the rest
/// after it, as many to a message as Discord takes.
pub(crate) fn compose<'a>(content: &str, files: &'a [AnswerFile]) -> Vec<Outgoing<'a>> {
    let (fit, oversized): (Vec<_>, Vec<_>) = files.iter().partition(|file| file.size <= FILE_LIMIT);
    let notes = oversized
        .iter()
        .map(|file| {
            format!(
                "Could not send {}: it has {} MiB, over the {} MiB Discord takes of one file. It is at {}.",
                file.name,
                file.size.div_ceil(MIB),
                FILE_LIMIT / MIB,
                file.source
            )
        })
        .collect::<Vec<_>>();
    let content = if notes.is_empty() {
        content.to_owned()
    } else {
        format!("{content}\n\n{}", notes.join("\n"))
    };
    let mut messages = discord_chunks(&content)
        .into_iter()
        .map(|content| Outgoing {
            content,
            files: Vec::new(),
        })
        .collect::<Vec<_>>();
    let mut bytes = 0;
    for file in fit {
        match messages.last_mut() {
            Some(last)
                if last.files.len() < FILES_PER_MESSAGE
                    && bytes + file.size <= MESSAGE_FILE_BYTES =>
            {
                last.files.push(file);
                bytes += file.size;
            }
            _ => {
                messages.push(Outgoing {
                    content: String::new(),
                    files: vec![file],
                });
                bytes = file.size;
            }
        }
    }
    messages
}

/// Reads each file from core's copy in `store`, checked against the digest
/// core recorded, as a message attachment.
pub(crate) async fn load(store: &Path, files: &[&AnswerFile]) -> Result<Vec<Attachment>> {
    let mut loaded = Vec::with_capacity(files.len());
    for (id, file) in (0_u64..).zip(files) {
        let bytes = tokio::fs::read(store.join(&file.path))
            .await
            .with_context(|| format!("read Carillon's copy of {}", file.name))?;
        if hex::encode(Sha256::digest(&bytes)) != file.sha256 {
            bail!("Carillon's copy of {} changed after it was kept", file.name);
        }
        loaded.push(Attachment::from_bytes(filename(&file.name), bytes, id));
    }
    Ok(loaded)
}

/// The name Discord shows: letters, digits, `.`, `-` and `_`, which is all
/// twilight sends.
fn filename(name: &str) -> String {
    let safe = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    if safe.is_empty() {
        "file".to_owned()
    } else {
        safe
    }
}
