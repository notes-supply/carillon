//! The files an agent attaches to its answer. Core keeps them the way it
//! keeps a message's attachments, before it records the answer: copied once
//! into a published capture, hashed while copying, read-only. Its interface
//! sends them from there, however often it tries.
//!
//! A file core cannot keep never fails the answer. The answer's text gains a
//! line that names the file and why, and the rest are kept.

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
};

use carillon_installation::config::MIB;
use rustix::fs::{Mode, OFlags};
use tracing::warn;

use crate::{
    Core, CoreError,
    attachments::{self, CaptureError, Copies, Sources},
    intake::{Intake, Reservation, lock},
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

/// Why core keeps no copy of a file an answer names.
#[derive(Debug, thiserror::Error)]
enum Unkept {
    #[error("there is no such file")]
    Missing,
    #[error("Carillon may not read it")]
    Forbidden,
    #[error("it could not be opened: {0}")]
    Unopened(io::Error),
    #[error("it is not a regular file")]
    NotAFile,
    #[error("it has {} MiB, over the {limit} MiB Carillon keeps of one file", .size.div_ceil(MIB))]
    TooLarge { size: u64, limit: u64 },
    #[error("it has {} MiB, over the {limit} MiB this channel uploads of one file", .size.div_ceil(MIB))]
    Unsendable { size: u64, limit: u64 },
    #[error("the answer's files would have more than {0} MiB together")]
    TooMuch(u64),
    #[error("an answer keeps at most {0} files")]
    TooMany(u32),
    #[error("{0}")]
    NoRoom(String),
    #[error("the disk filled up while Carillon copied it")]
    DiskFull,
    #[error("it changed while Carillon copied it")]
    Changed,
    #[error("Carillon could not copy it: {0:#}")]
    Uncopied(anyhow::Error),
}

impl From<CaptureError> for Unkept {
    fn from(error: CaptureError) -> Self {
        match error {
            CaptureError::Full => Self::DiskFull,
            CaptureError::Invalid(_) => Self::Changed,
            CaptureError::Io(error) => Self::Uncopied(error),
            cancelled @ CaptureError::Cancelled => Self::Uncopied(cancelled.into()),
        }
    }
}

impl From<io::Error> for Unkept {
    fn from(error: io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::NotFound => Self::Missing,
            io::ErrorKind::PermissionDenied => Self::Forbidden,
            _ => Self::Unopened(error),
        }
    }
}

/// A turn whose answer's files core is keeping, until dropped: the same
/// outcome again waits for it rather than copying again.
struct Keeping {
    intake: Arc<Intake>,
    turn: String,
}

impl Drop for Keeping {
    fn drop(&mut self) {
        lock(&self.intake.keeping).remove(&self.turn);
    }
}

/// What core made of an answer's files.
pub(crate) struct Kept {
    pub(crate) files: Vec<KeptFile>,
    /// A line for each file core could not keep.
    notes: Vec<String>,
    keeping: Keeping,
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

    fn keep(&mut self, capture: &str, path: &Path, size: u64, sha256: String) {
        self.files.push(KeptFile {
            capture: capture.to_owned(),
            name: path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            source: path.display().to_string(),
            size,
            sha256,
        });
    }

    fn refuse(&mut self, path: &Path, reason: &Unkept) {
        self.notes
            .push(format!("Could not attach {}: {reason}.", path.display()));
    }

    /// Ends what keeping the files held once the answer's settlement is
    /// decided: removes the capture if no committed turn references it, as
    /// when the same outcome was recorded meanwhile, and releases the turn
    /// and its bytes. A failed removal is left for startup reconciliation.
    pub(crate) fn settled(self, core: &Core) {
        let Self {
            files,
            keeping,
            reservation,
            ..
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
        drop((keeping, reservation));
    }
}

impl Core {
    /// Copies the files an answer names into one new capture, in order,
    /// within the attachment limits and the largest file the turn's
    /// interface uploads, unless the turn `runtime_id` holds already ended.
    /// Blocking: run it off the async reactor.
    /// # Errors
    /// Returns unavailable while the turn's files are being kept or core
    /// stops, so the runtime asks again; or storage errors.
    pub(crate) fn keep_answer_files(
        &self,
        runtime_id: &str,
        turn_id: &str,
        paths: &[PathBuf],
    ) -> Result<Kept, CoreError> {
        if !lock(&self.intake.keeping).insert(turn_id.to_owned()) {
            return Err(CoreError::Unavailable(
                "Carillon is still keeping this answer's files".to_owned(),
            ));
        }
        let mut kept = Kept {
            files: Vec::new(),
            notes: Vec::new(),
            keeping: Keeping {
                intake: self.intake.clone(),
                turn: turn_id.to_owned(),
            },
            reservation: None,
        };
        let Some(interface) = self.database.unsettled_interface(runtime_id, turn_id)? else {
            return Ok(kept);
        };
        let upload = self.options.uploads.get(&interface).copied();
        let limits = &self.options.attachments;
        let (mut accepted, mut sources) = (Vec::new(), Vec::new());
        let mut total = 0_u64;
        for (index, path) in paths.iter().enumerate() {
            let refused = if index >= limits.max_files as usize {
                Unkept::TooMany(limits.max_files)
            } else {
                match open(path) {
                    Err(reason) => reason,
                    Ok((_, size))
                        if let Some(limit) =
                            upload.filter(|limit| size > limit.saturating_mul(MIB)) =>
                    {
                        Unkept::Unsendable { size, limit }
                    }
                    Ok((_, size)) if size > limits.max_file_bytes() => Unkept::TooLarge {
                        size,
                        limit: limits.max_file_mib,
                    },
                    Ok((_, size)) if total + size > limits.max_event_bytes() => {
                        Unkept::TooMuch(limits.max_event_mib)
                    }
                    Ok((file, size)) => {
                        total += size;
                        accepted.push((path.as_path(), size));
                        sources.push((file, size));
                        continue;
                    }
                }
            };
            kept.refuse(path, &refused);
        }
        if accepted.is_empty() {
            return Ok(kept);
        }
        let refused = match self.intake.reserve(self, total) {
            Ok(reservation) => {
                kept.reservation = Some(reservation);
                let cancel = self.intake.signals.fresh();
                match attachments::capture_each(
                    &self.options.data_directory,
                    Sources(sources),
                    &cancel,
                ) {
                    Ok(Copies { id, files }) => {
                        for ((path, size), copied) in accepted.into_iter().zip(files) {
                            match copied {
                                Ok(sha256) => kept.keep(&id, path, size, sha256),
                                Err(error) => kept.refuse(path, &error.into()),
                            }
                        }
                        return Ok(kept);
                    }
                    Err(CaptureError::Cancelled) => {
                        return Err(CoreError::Unavailable(
                            "Carillon is shutting down".to_owned(),
                        ));
                    }
                    Err(error) => error.into(),
                }
            }
            Err(CoreError::InsufficientStorage(reason)) => Unkept::NoRoom(reason),
            Err(error) => return Err(error),
        };
        for (path, _) in accepted {
            kept.refuse(path, &refused);
        }
        Ok(kept)
    }
}

/// Opens a file as its owner would, following links. A FIFO opens without
/// waiting for a writer and is then refused as not a regular file.
fn open(path: &Path) -> Result<(fs::File, u64), Unkept> {
    let file = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(fs::File::from)
    .map_err(io::Error::from)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(Unkept::NotAFile);
    }
    Ok((file, metadata.len()))
}
