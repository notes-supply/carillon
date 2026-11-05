//! An answer's files on Discord: which message carries each, and their
//! bytes, read from core's copies.
//!
//! Discord takes at most 10 files and 25 MiB in all per message
//! (<https://docs.discord.com/developers/resources/message#create-message>
//! and error 30015 of
//! <https://docs.discord.com/developers/topics/opcodes-and-status-codes>).
//! The provider's registry entry declares the largest file it uploads, and
//! core keeps no larger answer file for it, so every file fits a message.

use std::path::Path;

use anyhow::{Context as _, Result, bail};
use carillon_installation::config::{MIB, Provider};
use carillon_protocol::model::AnswerFile;
use sha2::{Digest as _, Sha256};
use twilight_model::http::attachment::Attachment;

use super::discord_chunks;

const FILES_PER_MESSAGE: usize = 10;
/// A message's files, leaving room in its 25 MiB request for its text and
/// the request's framing.
const MESSAGE_FILE_BYTES: u64 = 25 * MIB - 64 * 1024;

/// Requires the provider's upload limit to fit in one message.
/// # Errors
/// Returns the largest limit one message carries when it is larger.
pub(crate) fn require_upload_limit(provider: &Provider) -> Result<()> {
    let most = MESSAGE_FILE_BYTES / MIB;
    let mib = provider.upload_mib();
    if mib > most {
        bail!(
            "max_upload_mib for {} is {mib}, but one Discord message carries at most {most}",
            provider.id
        );
    }
    Ok(())
}

/// One Discord message of a delivery: its text, possibly empty, and the
/// files it carries.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Outgoing<'a> {
    pub(crate) content: String,
    pub(crate) files: Vec<&'a AnswerFile>,
}

/// The messages that send `content` with `files`, in order. The text is
/// split at Discord's length limit; the first files go on the text's last
/// message, the rest after it, as many to a message as Discord takes.
pub(crate) fn compose<'a>(content: &str, files: &'a [AnswerFile]) -> Vec<Outgoing<'a>> {
    let mut messages = discord_chunks(content)
        .into_iter()
        .map(|content| Outgoing {
            content,
            files: Vec::new(),
        })
        .collect::<Vec<_>>();
    let mut bytes = 0;
    for file in files {
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
