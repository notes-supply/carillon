//! The files a sent message carries: plain names and their bytes, within
//! Discord's limits for one message.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use twilight_model::http::attachment::Attachment;

use super::OperationError;

/// The most files one sent message may carry.
pub(crate) const ATTACHMENT_COUNT: usize = 10;
/// The most bytes the files of one sent message may hold together.
pub(crate) const ATTACHMENT_BYTES: usize = 4 * 1024 * 1024;

/// A file to attach to a sent message: a plain filename and its bytes,
/// base64-encoded on the wire.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Upload {
    pub(super) filename: String,
    pub(super) data: String,
}

impl Upload {
    /// # Errors
    /// Rejects a name that is not a plain filename, or more bytes than one
    /// message may carry.
    pub(crate) fn new(filename: String, bytes: &[u8]) -> Result<Self, OperationError> {
        plain_filename(&filename)?;
        if bytes.len() > ATTACHMENT_BYTES {
            return Err(too_large());
        }
        Ok(Self {
            filename,
            data: STANDARD.encode(bytes),
        })
    }
}

pub(super) fn uploads(input: Vec<Upload>) -> Result<Vec<Attachment>, OperationError> {
    if input.len() > ATTACHMENT_COUNT {
        return Err(too_many());
    }
    let mut size = 0;
    input
        .into_iter()
        .enumerate()
        .map(|(index, Upload { filename, data })| {
            plain_filename(&filename)?;
            let bytes = STANDARD
                .decode(data)
                .map_err(|_| OperationError::invalid("Attachment data must be base64."))?;
            size += bytes.len();
            if size > ATTACHMENT_BYTES {
                return Err(too_large());
            }
            Ok(Attachment::from_bytes(filename, bytes, index as u64))
        })
        .collect()
}

fn plain_filename(filename: &str) -> Result<(), OperationError> {
    if filename.is_empty() || filename.contains(['/', '\\', '\0']) {
        return Err(OperationError::invalid(
            "Attachment filename must be a plain filename.",
        ));
    }
    Ok(())
}

pub(crate) fn too_many() -> OperationError {
    OperationError::invalid(&format!(
        "At most {ATTACHMENT_COUNT} attachments are supported."
    ))
}

pub(crate) fn too_large() -> OperationError {
    OperationError::invalid(&format!(
        "Attachments exceed the {} MiB per-message limit.",
        ATTACHMENT_BYTES >> 20
    ))
}
