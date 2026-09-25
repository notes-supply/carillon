//! The lock serializing changes to one installation. Operator maintenance,
//! the release scripts, and the Discord provider's command-module changes
//! take the same `flock(2)` lock on `CARILLON_HOME/.maintenance.lock`.

use std::{
    fs,
    os::unix::fs::OpenOptionsExt as _,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result};
use fs2::FileExt as _;

/// Exclusive, non-blocking lock serializing maintenance of one installation.
/// The release scripts take the same lock unless a caller already holds it
/// for them and says so through [`MaintenanceLock::HELD_FOR`]. Dropping the
/// value, or ending the process, releases it.
#[derive(Debug)]
pub struct MaintenanceLock {
    _file: fs::File,
}

impl MaintenanceLock {
    /// Environment variable naming the installation whose lock the caller
    /// already holds on behalf of a child process.
    pub const HELD_FOR: &str = "CARILLON_MAINTENANCE_LOCK";

    /// The lock file of the installation at `home`.
    #[must_use]
    pub fn path(home: &Path) -> PathBuf {
        home.join(".maintenance.lock")
    }

    /// Take the lock, or report that another operation holds it.
    ///
    /// # Errors
    ///
    /// Returns an error when the lock is held or its file cannot be opened.
    pub fn acquire(home: &Path) -> Result<Self> {
        Self::try_acquire(home)?.with_context(|| {
            format!(
                "another Carillon maintenance operation is running for {}",
                home.display()
            )
        })
    }

    /// Take the lock, or return `None` while another operation holds it.
    ///
    /// # Errors
    ///
    /// Returns an error when the lock file cannot be opened or locked for a
    /// reason other than another holder.
    pub fn try_acquire(home: &Path) -> Result<Option<Self>> {
        let path = Self::path(home);
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("open maintenance lock {}", path.display()))?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(error) if error.kind() == fs2::lock_contended_error().kind() => Ok(None),
            Err(error) => {
                Err(error).with_context(|| format!("lock maintenance lock {}", path.display()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::MaintenanceLock;

    #[test]
    fn one_holder_at_a_time_until_released() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let held = MaintenanceLock::acquire(directory.path())?;
        assert!(MaintenanceLock::try_acquire(directory.path())?.is_none());
        let refusal = MaintenanceLock::acquire(directory.path())
            .err()
            .map(|error| error.to_string())
            .unwrap_or_default();
        assert!(refusal.starts_with("another Carillon maintenance operation is running"));
        drop(held);
        assert!(MaintenanceLock::try_acquire(directory.path())?.is_some());
        Ok(())
    }
}
