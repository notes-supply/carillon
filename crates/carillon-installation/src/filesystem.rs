//! Filesystem durability for complete, locally generated trees and files.

use std::{fmt, fs, io::Write as _, os::unix::fs::PermissionsExt, path::Path};

/// Persist file contents and metadata, then each directory's entries bottom-up.
/// Callers must also sync the parent after publishing the tree with rename.
///
/// # Errors
///
/// Returns the first I/O error encountered.
pub fn sync_tree(path: &Path) -> std::io::Result<()> {
    if path.is_dir() {
        for entry in fs::read_dir(path)? {
            sync_tree(&entry?.path())?;
        }
    }
    fs::File::open(path)?.sync_all()
}

/// Persist a publication directory and the entries naming its ancestors.
///
/// # Errors
///
/// Returns the first I/O error encountered.
pub fn sync_ancestors(directory: &Path) -> std::io::Result<()> {
    for parent in directory.ancestors() {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

/// Where publishing a file or removing it stopped. Renames and removals are
/// visible at once but survive a system crash only after their directory is
/// synced, so a failed sync does not mean the change did not happen.
#[derive(Debug)]
pub enum PublishError {
    /// The change was not made: the previous file, or its absence, remains.
    NotPublished(std::io::Error),
    /// The change is visible, but syncing its directory failed, so a system
    /// crash may still bring back the previous state.
    Unsynced(std::io::Error),
}

impl fmt::Display for PublishError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotPublished(error) => write!(formatter, "not changed: {error}"),
            Self::Unsynced(error) => {
                write!(formatter, "changed, but not confirmed durable: {error}")
            }
        }
    }
}

impl std::error::Error for PublishError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NotPublished(error) | Self::Unsynced(error) => Some(error),
        }
    }
}

impl From<PublishError> for std::io::Error {
    fn from(error: PublishError) -> Self {
        let kind = match &error {
            PublishError::NotPublished(inner) | PublishError::Unsynced(inner) => inner.kind(),
        };
        Self::new(kind, error)
    }
}

/// Publish a complete file with the given mode via a synced temporary sibling
/// and rename, so readers never observe partial contents.
///
/// # Errors
///
/// Returns the first I/O error encountered. After an error the target may
/// already hold the new contents when only the final directory sync failed;
/// use [`publish`] when the caller must tell the two apart.
pub fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    Ok(publish(path, bytes, mode)?)
}

/// [`atomic_write`], reporting whether a failure happened before or after
/// the new contents were published.
///
/// # Errors
///
/// [`PublishError::NotPublished`] leaves the target unchanged;
/// [`PublishError::Unsynced`] leaves the new contents visible.
pub fn publish(path: &Path, bytes: &[u8], mode: u32) -> Result<(), PublishError> {
    let parent = parent(path);
    (|| {
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(mode))?;
        temporary.write_all(bytes)?;
        temporary.as_file().sync_all()?;
        fs::rename(temporary.path(), path)?;
        Ok(())
    })()
    .map_err(PublishError::NotPublished)?;
    sync_directory(parent)
}

/// Remove a file and sync its directory, so a system crash cannot bring it
/// back. A file that does not exist counts as removed, and its directory is
/// still synced, so repeating an unconfirmed removal confirms it.
///
/// # Errors
///
/// [`PublishError::NotPublished`] leaves the file in place;
/// [`PublishError::Unsynced`] leaves it removed but not durably.
pub fn remove(path: &Path) -> Result<(), PublishError> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(PublishError::NotPublished(error)),
    }
    sync_directory(parent(path))
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

fn sync_directory(directory: &Path) -> Result<(), PublishError> {
    fs::File::open(directory)
        .and_then(|directory| directory.sync_all())
        .map_err(PublishError::Unsynced)
}
