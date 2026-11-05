//! The files an agent attaches to its answer. Core keeps them the way it
//! keeps a message's attachments, before it records the answer: copied once
//! into a published capture, hashed while copying, read-only. Its interface
//! sends them from there, however often it tries.
//!
//! A file core cannot keep never fails the answer. The answer's text gains a
//! line that names the file and why, and the rest are kept.

use std::{
    fmt::Display,
    fs, io,
    path::{Path, PathBuf},
};

use carillon_installation::config::MIB;
use rustix::fs::{Mode, OFlags};
use tracing::warn;

use crate::{
    Core, CoreError,
    attachments::{self, Capture, CaptureError, Sources},
    intake::Reservation,
};

/// A file kept with an answer; its ordinal in the answer is its ordinal in
/// the capture.
#[derive(Debug)]
pub(crate) struct KeptFile {
    pub(crate) capture: String,
    pub(crate) name: String,
    /// Where the agent had it.
    pub(crate) source: String,
    pub(crate) size: u64,
    pub(crate) sha256: String,
}

/// What core made of an answer's files.
#[derive(Default)]
pub(crate) struct Kept {
    pub(crate) files: Vec<KeptFile>,
    /// A line for each file core could not keep.
    notes: Vec<String>,
    /// Holds the copies' bytes against the storage budget until the answer
    /// that references them is recorded.
    reservation: Option<Reservation>,
}

impl Kept {
    /// The text delivered for `output`: the answer, then what was not kept.
    pub(crate) fn content(&self, output: &str) -> String {
        if self.notes.is_empty() {
            return output.to_owned();
        }
        format!("{output}\n\n{}", self.notes.join("\n"))
    }

    fn refuse(&mut self, paths: &[(&Path, u64)], reason: &str) {
        for (path, _) in paths {
            self.notes.push(not_attached(path, reason));
        }
    }

    /// Ends what keeping the files held once the answer's settlement is
    /// decided: removes the capture if no committed turn references it, as
    /// when the same outcome was recorded meanwhile, and releases its bytes
    /// from the budget. A failed removal is left for startup reconciliation.
    pub(crate) fn settled(self, core: &Core) {
        let Self {
            files, reservation, ..
        } = self;
        if let Some(capture) = files.first().map(|file| file.capture.as_str()) {
            let removed = core
                .database
                .capture_referenced(capture)
                .and_then(|referenced| {
                    if !referenced {
                        attachments::remove(&core.options.data_directory, capture)?;
                    }
                    Ok(())
                });
            if let Err(error) = removed {
                warn!(capture, %error, "an answer's capture is left for startup reconciliation");
            }
        }
        drop(reservation);
    }
}

impl Core {
    /// Copies the files an answer names into one new capture, in order,
    /// within the attachment limits. Blocking: run it off the async reactor.
    /// # Errors
    /// Returns unavailable when core is stopping, so the runtime asks again.
    pub(crate) fn keep_answer_files(&self, paths: &[PathBuf]) -> Result<Kept, CoreError> {
        let limits = &self.options.attachments;
        let mut kept = Kept::default();
        let (mut accepted, mut sources) = (Vec::new(), Vec::new());
        let mut total = 0_u64;
        for (index, path) in paths.iter().enumerate() {
            let refused = if index >= limits.max_files as usize {
                format!("an answer keeps at most {} files", limits.max_files)
            } else {
                match open(path) {
                    Err(reason) => reason,
                    Ok((_, size)) if size > limits.max_file_bytes() => format!(
                        "it has {} MiB, over the {} MiB Carillon keeps of one file",
                        size.div_ceil(MIB),
                        limits.max_file_mib
                    ),
                    Ok((_, size)) if total + size > limits.max_event_bytes() => format!(
                        "the answer's files would have more than {} MiB together",
                        limits.max_event_mib
                    ),
                    Ok((file, size)) => {
                        total += size;
                        accepted.push((path.as_path(), size));
                        sources.push((file, size));
                        continue;
                    }
                }
            };
            kept.notes.push(not_attached(path, refused));
        }
        if accepted.is_empty() {
            return Ok(kept);
        }
        match self.intake.reserve(self, total) {
            Ok(reservation) => kept.reservation = Some(reservation),
            Err(CoreError::InsufficientStorage(reason)) => {
                kept.refuse(&accepted, &reason);
                return Ok(kept);
            }
            Err(error) => return Err(error),
        }
        let cancel = self.intake.signals.fresh();
        match attachments::capture(&self.options.data_directory, Sources(sources), &cancel) {
            Ok(Capture { id, digests }) => {
                kept.files = accepted
                    .iter()
                    .zip(digests)
                    .map(|((path, size), sha256)| KeptFile {
                        capture: id.clone(),
                        name: path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned(),
                        source: path.display().to_string(),
                        size: *size,
                        sha256,
                    })
                    .collect();
            }
            Err(CaptureError::Cancelled) => {
                return Err(CoreError::Unavailable(
                    "Carillon is shutting down".to_owned(),
                ));
            }
            Err(CaptureError::Full) => {
                kept.refuse(&accepted, "the disk filled up while Carillon copied it");
            }
            Err(CaptureError::Invalid(_)) => {
                kept.refuse(&accepted, "it changed while Carillon copied it");
            }
            Err(CaptureError::Io(error)) => {
                kept.refuse(&accepted, &format!("Carillon could not copy it: {error:#}"));
            }
        }
        Ok(kept)
    }
}

/// Opens a file as its owner would, following links. A FIFO opens without
/// waiting for a writer and is then refused as not a regular file.
fn open(path: &Path) -> Result<(fs::File, u64), String> {
    let file = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(fs::File::from)
    .map_err(|error| unreadable(&io::Error::from(error)))?;
    let metadata = file.metadata().map_err(|error| unreadable(&error))?;
    if !metadata.is_file() {
        return Err("it is not a regular file".to_owned());
    }
    Ok((file, metadata.len()))
}

fn unreadable(error: &io::Error) -> String {
    match error.kind() {
        io::ErrorKind::NotFound => "there is no such file".to_owned(),
        io::ErrorKind::PermissionDenied => "Carillon may not read it".to_owned(),
        _ => format!("it could not be opened: {error}"),
    }
}

fn not_attached(path: &Path, reason: impl Display) -> String {
    format!("Could not attach {}: {reason}.", path.display())
}
