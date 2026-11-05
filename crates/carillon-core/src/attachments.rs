//! Captured attachments: immutable copies of the files an interface staged
//! with an event, owned by core from acceptance on.
//!
//! ```text
//! <data_dir>/components/<interface>/incoming/<batch>/<i>   the interface's staging
//! <data_dir>/attachments/.partial-<capture>/<i>            a copy in progress
//! <data_dir>/attachments/<capture>/<i>                     a published capture
//! ```
//!
//! A capture is copied, hashed while copying, synced and published under a
//! fresh name before any turn refers to it, so an accepted reference never
//! points at an unpublished or unsynced file. Only reconciliation under
//! exclusive ownership removes what no committed turn references. The copy
//! never hardlinks or moves staged files: the interface keeps its staging
//! until it holds a receipt, and cannot change accepted bytes through it.
//! Core opens a whole batch before it starts the capture, which then reads
//! the open files, so an interface removing its staging cannot fail it.
//! An answer's capture leaves out each file that fails to copy, its ordinal
//! taken by the next, so one file changing or failing costs only itself.
//!
//! Read-only files guard against accidental edits, not against the owner.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Read as _, Write as _},
    os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context as _, Result, bail};
use carillon_installation::config::{attachment_staging, attachment_store};
use carillon_protocol::model::{StagedAttachments, TurnAttachment};
use rustix::fs::{CWD, Mode, OFlags, RenameFlags, ResolveFlags};
use sha2::{Digest as _, Sha256};

/// The copy buffer: a capture's memory does not grow with its files.
const CHUNK: usize = 1024 * 1024;
const PARTIAL: &str = ".partial-";

pub(crate) fn store(data_directory: &Path) -> PathBuf {
    attachment_store(data_directory)
}

/// A published capture: one directory holding a turn's files by ordinal.
#[derive(Clone, Debug)]
pub(crate) struct Capture {
    pub(crate) id: String,
    /// Lowercase hexadecimal SHA-256 of each file, in order.
    pub(crate) digests: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum CaptureError {
    /// The staged files are not what the event declares.
    #[error("{0}")]
    Invalid(String),
    #[error("the attachment filesystem is full")]
    Full,
    #[error("the capture was cancelled")]
    Cancelled,
    #[error(transparent)]
    Io(#[from] anyhow::Error),
}

impl From<io::Error> for CaptureError {
    fn from(error: io::Error) -> Self {
        if error.kind() == io::ErrorKind::StorageFull {
            Self::Full
        } else {
            Self::Io(error.into())
        }
    }
}

/// Stops a capture between chunks. A deadline or shutdown sets it; the copy
/// notices at its next chunk, so callers wait for the copy to return rather
/// than assume it stopped.
#[derive(Clone, Default)]
pub(crate) struct Cancel {
    own: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    /// Tests hold this to stall a copy between chunks, as a slow disk would.
    #[cfg(test)]
    pub(crate) stall: Arc<std::sync::Mutex<()>>,
    /// Tests hold this to stall a copy that passed its last check while it
    /// publishes, as a slow sync would; `publishing` shows it got there.
    #[cfg(test)]
    pub(crate) publication: Arc<std::sync::Mutex<()>>,
    #[cfg(test)]
    pub(crate) publishing: Arc<AtomicBool>,
}

impl Cancel {
    /// A new capture's signal: cancelled on its own, or by stopping every
    /// capture sharing this one's shutdown.
    pub(crate) fn fresh(&self) -> Self {
        Self {
            own: Arc::default(),
            ..self.clone()
        }
    }

    pub(crate) fn cancel(&self) {
        self.own.store(true, Ordering::SeqCst);
    }

    /// Cancel every capture made [`fresh`](Self::fresh) from this one.
    pub(crate) fn stop_all(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }

    pub(crate) fn stopped(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// Whether a deadline or shutdown asked the capture to stop.
    pub(crate) fn requested(&self) -> bool {
        self.own.load(Ordering::SeqCst) || self.shutdown.load(Ordering::SeqCst)
    }

    fn checkpoint(&self) -> bool {
        #[cfg(test)]
        drop(self.stall.lock());
        self.requested()
    }
}

/// A batch name: a single path component the interface chose.
pub(crate) fn valid_batch(batch: &str) -> bool {
    (1..=64).contains(&batch.len())
        && batch
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// Files opened for capture, each with its expected length: a staged batch,
/// or an answer's files. The capture reads these files, not their names,
/// which may be removed at any time, as an interface's staging is when it
/// restarts.
pub(crate) struct Sources(pub(crate) Vec<(fs::File, u64)>);

/// Open every file of the staged batch of `interface`, beneath the
/// interface's staging directory and without following any link. Each must
/// be a regular file of exactly the declared length.
pub(crate) fn open(
    data_directory: &Path,
    interface: &str,
    staged: &StagedAttachments,
) -> Result<Sources, CaptureError> {
    if !valid_batch(&staged.batch) {
        return Err(CaptureError::Invalid(
            "attachment batch names one staging directory".to_owned(),
        ));
    }
    let staging = attachment_staging(Path::new(""), interface);
    let root = rustix::fs::open(
        data_directory,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(io::Error::from)?;
    let mut sources = Vec::with_capacity(staged.files.len());
    for (ordinal, declared) in staged.files.iter().enumerate() {
        let relative = staging.join(&staged.batch).join(ordinal.to_string());
        // Every component below the data directory is resolved without
        // links and cannot leave it; a FIFO opens without waiting for a
        // writer and is then refused as not a regular file.
        let source = rustix::fs::openat2(
            &root,
            &relative,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::NOCTTY | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
        )
        .map_err(|error| {
            CaptureError::Invalid(format!(
                "staged attachment {ordinal} cannot be opened safely: {}",
                io::Error::from(error)
            ))
        })?;
        let source = fs::File::from(source);
        let metadata = source.metadata()?;
        if !metadata.file_type().is_file() {
            return Err(CaptureError::Invalid(format!(
                "staged attachment {ordinal} is not a regular file"
            )));
        }
        if metadata.len() != declared.size {
            return Err(CaptureError::Invalid(format!(
                "staged attachment {ordinal} has {} bytes, not the declared {}",
                metadata.len(),
                declared.size
            )));
        }
        sources.push((source, declared.size));
    }
    Ok(Sources(sources))
}

/// Copy opened staged files into a new published capture. Blocking: run it
/// off the async reactor.
pub(crate) fn capture(
    data_directory: &Path,
    Sources(sources): Sources,
    cancel: &Cancel,
) -> Result<Capture, CaptureError> {
    let partial = Partial::create(data_directory)?;
    let mut digests = Vec::with_capacity(sources.len());
    for (ordinal, (mut source, size)) in sources.into_iter().enumerate() {
        let destination = partial.path.join(ordinal.to_string());
        digests.push(copy_file(&mut source, &destination, size, cancel)?);
    }
    let id = partial.publish(cancel)?;
    Ok(Capture { id, digests })
}

/// An answer's capture: the files that copied, at consecutive ordinals, and
/// why each other one did not.
#[derive(Debug)]
pub(crate) struct Copies {
    /// The capture, published when any file copied.
    pub(crate) id: String,
    /// Each source's digest, or why it was left out, in order.
    pub(crate) files: Vec<Result<String, CaptureError>>,
}

/// Copy opened files into a new published capture, leaving out each that
/// fails to copy; with none copied, nothing is published. Only a
/// cancellation or a failure of the capture itself fails it. Blocking: run
/// it off the async reactor.
pub(crate) fn capture_each(
    data_directory: &Path,
    Sources(sources): Sources,
    cancel: &Cancel,
) -> Result<Copies, CaptureError> {
    let partial = Partial::create(data_directory)?;
    let mut files = Vec::with_capacity(sources.len());
    let mut copied = 0_usize;
    for (mut source, size) in sources {
        let destination = partial.path.join(copied.to_string());
        match copy_file(&mut source, &destination, size, cancel) {
            Ok(digest) => {
                copied += 1;
                files.push(Ok(digest));
            }
            Err(CaptureError::Cancelled) => return Err(CaptureError::Cancelled),
            Err(error) => {
                if let Err(removal) = fs::remove_file(&destination)
                    && removal.kind() != io::ErrorKind::NotFound
                {
                    return Err(removal.into());
                }
                files.push(Err(error));
            }
        }
    }
    let id = if copied == 0 {
        partial.id.clone()
    } else {
        partial.publish(cancel)?
    };
    Ok(Copies { id, files })
}

/// A capture being copied, removed unless it is published. A failed
/// removal is left for reconciliation: unpublished copies are never
/// referenced.
struct Partial {
    store: PathBuf,
    id: String,
    path: PathBuf,
    published: bool,
}

impl Partial {
    fn create(data_directory: &Path) -> Result<Self, CaptureError> {
        let store = store(data_directory);
        create_store(&store)?;
        let id = uuid::Uuid::now_v7().to_string();
        let path = store.join(format!("{PARTIAL}{id}"));
        fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self {
            store,
            id,
            path,
            published: false,
        })
    }

    /// Publishes the synced copies under the capture's name, unless a
    /// deadline or shutdown arrived during the last sync.
    fn publish(mut self, cancel: &Cancel) -> Result<String, CaptureError> {
        if cancel.checkpoint() {
            return Err(CaptureError::Cancelled);
        }
        #[cfg(test)]
        {
            cancel.publishing.store(true, Ordering::SeqCst);
            drop(cancel.publication.lock());
        }
        publish(&self.store, &self.path, &self.id)?;
        self.published = true;
        Ok(std::mem::take(&mut self.id))
    }
}

impl Drop for Partial {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

/// Copy exactly `size` bytes, hashing them on the way, then sync the copy.
fn copy_file(
    source: &mut fs::File,
    destination: &Path,
    size: u64,
    cancel: &Cancel,
) -> Result<String, CaptureError> {
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(destination)?;
    let mut buffer = vec![0; CHUNK];
    let mut digest = Sha256::new();
    let mut copied = 0_u64;
    loop {
        if cancel.checkpoint() {
            return Err(CaptureError::Cancelled);
        }
        let read = match source.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        };
        copied += read as u64;
        if copied > size {
            return Err(CaptureError::Invalid(
                "a staged attachment grew while it was being captured".to_owned(),
            ));
        }
        digest.update(&buffer[..read]);
        output.write_all(&buffer[..read])?;
    }
    if copied != size {
        return Err(CaptureError::Invalid(
            "a staged attachment shrank while it was being captured".to_owned(),
        ));
    }
    output.set_permissions(fs::Permissions::from_mode(0o440))?;
    output.sync_all()?;
    Ok(hex::encode(digest.finalize()))
}

/// Publish a synced copy under its final name, never replacing an existing
/// capture, and persist the store's entry.
fn publish(store: &Path, partial: &Path, id: &str) -> Result<(), CaptureError> {
    fs::File::open(partial)?.sync_all()?;
    fs::set_permissions(partial, fs::Permissions::from_mode(0o750))?;
    rustix::fs::renameat_with(CWD, partial, CWD, store.join(id), RenameFlags::NOREPLACE)
        .map_err(io::Error::from)?;
    fs::File::open(store)?.sync_all()?;
    Ok(())
}

/// Create the store on first use and persist its entry in the data directory.
fn create_store(store: &Path) -> io::Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(store) {
        Ok(()) => fs::File::open(store.parent().unwrap_or(store))?.sync_all(),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

/// Remove one capture that no committed turn references.
pub(crate) fn remove(data_directory: &Path, id: &str) -> io::Result<()> {
    let store = store(data_directory);
    fs::remove_dir_all(store.join(id))?;
    fs::File::open(store)?.sync_all()
}

/// What attachment reconciliation removed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Reconciled {
    /// Copies abandoned before publication.
    pub partial: usize,
    /// Published captures no committed turn references.
    pub unreferenced: usize,
}

/// Remove abandoned copies and published captures no committed turn
/// references. Call only under exclusive ownership of the installation's
/// state, after reading the committed references: age is never a reason.
pub(crate) fn reconcile(
    data_directory: &Path,
    referenced: &BTreeSet<String>,
) -> Result<Reconciled> {
    let store = store(data_directory);
    let mut reconciled = Reconciled::default();
    let entries = match fs::read_dir(&store) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(reconciled),
        Err(error) => return Err(error).context("read the attachment store"),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(PARTIAL) {
            remove_entry(&entry)?;
            reconciled.partial += 1;
        } else if !referenced.contains(&name) {
            remove_entry(&entry)?;
            reconciled.unreferenced += 1;
        }
    }
    if reconciled != Reconciled::default() {
        fs::File::open(&store)?.sync_all()?;
    }
    Ok(reconciled)
}

fn remove_entry(entry: &fs::DirEntry) -> io::Result<()> {
    if entry.file_type()?.is_dir() {
        fs::remove_dir_all(entry.path())
    } else {
        fs::remove_file(entry.path())
    }
}

/// A committed reference to one captured file of a turn.
#[derive(Clone, Debug)]
pub(crate) struct CapturedFile {
    pub(crate) capture: String,
    pub(crate) ordinal: u32,
    pub(crate) name: String,
    pub(crate) size: u64,
    pub(crate) content_type: Option<String>,
    pub(crate) sha256: String,
}

impl CapturedFile {
    /// The wire form: a path relative to the attachment root.
    pub(crate) fn wire(&self) -> TurnAttachment {
        TurnAttachment {
            path: format!("{}/{}", self.capture, self.ordinal),
            name: self.name.clone(),
            size: self.size,
            content_type: self.content_type.clone(),
            sha256: self.sha256.clone(),
        }
    }
}

/// Why a captured file cannot be used.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Unusable {
    /// It is missing, not a regular file, or not the bytes accepted.
    #[error("{0}")]
    Altered(String),
    /// It could not be read now; its bytes are unknown.
    #[error(transparent)]
    Unreadable(#[from] anyhow::Error),
}

/// Check that a referenced file is still the regular file core captured,
/// with the same length and digest, telling a file that is no longer the
/// accepted one from one that could not be read. Blocking: reads it whole.
pub(crate) fn check(data_directory: &Path, reference: &CapturedFile) -> Result<(), Unusable> {
    let path =
        file(data_directory, reference).map_err(|error| Unusable::Altered(error.to_string()))?;
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(Unusable::Altered(format!(
                "captured attachment {} is missing",
                path.display()
            )));
        }
        Err(error) => return Err(Unusable::Unreadable(error.into())),
    };
    if !metadata.file_type().is_file() || metadata.len() != reference.size {
        return Err(Unusable::Altered(format!(
            "captured attachment {} is not the file that was accepted",
            path.display()
        )));
    }
    if digest(&path).map_err(|error| Unusable::Unreadable(error.into()))? != reference.sha256 {
        return Err(Unusable::Altered(format!(
            "captured attachment {} was changed after it was accepted",
            path.display()
        )));
    }
    Ok(())
}

fn digest(path: &Path) -> io::Result<String> {
    let mut source = fs::File::open(path)?;
    let mut buffer = vec![0; CHUNK];
    let mut digest = Sha256::new();
    loop {
        match source.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => digest.update(&buffer[..read]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(hex::encode(digest.finalize()))
}

/// Validate a stopped installation's capture store against every committed
/// reference before it is archived or published by restore: each referenced
/// file present and intact, and nothing else in the store.
pub(crate) fn validate_store(data_directory: &Path, references: &[CapturedFile]) -> Result<()> {
    let mut expected: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    for reference in references {
        check(data_directory, reference)?;
        expected
            .entry(&reference.capture)
            .or_default()
            .insert(reference.ordinal.to_string());
    }
    let store = store(data_directory);
    let entries = match fs::read_dir(&store) {
        Ok(entries) => entries.collect::<io::Result<Vec<_>>>()?,
        Err(error) if error.kind() == io::ErrorKind::NotFound && expected.is_empty() => {
            return Ok(());
        }
        Err(error) => return Err(error).context("read the attachment store"),
    };
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(files) = expected.get(name.as_str()) else {
            bail!("the attachment store holds unexpected entry {name}");
        };
        if !entry.file_type()?.is_dir() {
            bail!("captured attachment {name} is not a directory");
        }
        for file in fs::read_dir(entry.path())? {
            let file = file?.file_name().to_string_lossy().into_owned();
            if !files.contains(&file) {
                bail!("captured attachment {name} holds unexpected file {file}");
            }
        }
    }
    Ok(())
}

/// Where a referenced file lives, after checking the stored identity is a
/// capture name and an ordinal rather than a path.
pub(crate) fn file(data_directory: &Path, reference: &CapturedFile) -> Result<PathBuf> {
    if uuid::Uuid::try_parse(&reference.capture).is_err() {
        bail!("invalid captured attachment identity");
    }
    Ok(store(data_directory)
        .join(&reference.capture)
        .join(reference.ordinal.to_string()))
}

/// Free bytes for this user on the store's filesystem.
pub(crate) fn available(data_directory: &Path) -> io::Result<u64> {
    let store = store(data_directory);
    let path = if store.exists() {
        store
    } else {
        data_directory.to_path_buf()
    };
    let stats = rustix::fs::statvfs(&path).map_err(io::Error::from)?;
    Ok(stats.f_bavail.saturating_mul(stats.f_frsize))
}

#[cfg(test)]
mod tests;
