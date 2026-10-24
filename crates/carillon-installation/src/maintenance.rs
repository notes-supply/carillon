//! The locks serializing changes to one installation and to the owner's
//! user units. Operator maintenance, the release scripts, and the Discord
//! provider's module changes take the same `flock(2)` lock on
//! `CARILLON_HOME/.maintenance.lock`. Every installation of an owner shares
//! the owner's Carillon user units, so whatever reads or changes which
//! installation they run also takes `.carillon-units.lock` beside them,
//! always after its installation's lock. Core holds [`ServiceLock`] over its
//! state while it serves; what needs core stopped takes it too.

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
    _lock: Locked,
}

impl MaintenanceLock {
    /// Environment variable naming the installation whose lock the caller
    /// already holds on behalf of a child process.
    pub const HELD_FOR: &str = "CARILLON_MAINTENANCE_LOCK";

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
        Ok(try_lock(&Self::path(home))?.map(|lock| Self { _lock: lock }))
    }

    /// Take the lock for a short change, unless [`Self::HELD_FOR`] says the
    /// caller already holds it for this installation, in which case nothing
    /// is taken and `None` returned.
    ///
    /// # Errors
    ///
    /// Returns an error when another operation holds the lock or its file
    /// cannot be opened.
    pub fn acquire_unless_held(home: &Path) -> Result<Option<Self>> {
        if held_for(home, std::env::var_os(Self::HELD_FOR).as_deref()) {
            return Ok(None);
        }
        Self::acquire(home).map(Some)
    }
}

/// Whether `held`, the value of [`MaintenanceLock::HELD_FOR`], names the
/// installation at `home`.
fn held_for(home: &Path, held: Option<&std::ffi::OsStr>) -> bool {
    held.is_some_and(|held| {
        let held = Path::new(held);
        match (held.canonicalize(), home.canonicalize()) {
            (Ok(held), Ok(home)) => held == home,
            _ => held == home,
        }
    })
}

/// Exclusive, non-blocking lock over the owner's Carillon user units in
/// `units`, the owner's `systemd/user` directory. Taken after the
/// installation's [`MaintenanceLock`]; the release scripts take the same
/// lock unless a caller already holds it for them and says so through
/// [`UnitsLock::HELD_FOR`].
#[derive(Debug)]
pub struct UnitsLock {
    _lock: Locked,
}

impl UnitsLock {
    /// Environment variable naming the unit directory whose lock the caller
    /// already holds on behalf of a child process.
    pub const HELD_FOR: &str = "CARILLON_UNITS_LOCK";

    #[must_use]
    pub fn path(units: &Path) -> PathBuf {
        units.join(".carillon-units.lock")
    }

    /// Take the lock, or report that another operation holds it.
    ///
    /// # Errors
    ///
    /// Returns an error when the lock is held or its file cannot be opened.
    pub fn acquire(units: &Path) -> Result<Self> {
        let lock = try_lock(&Self::path(units))?.with_context(|| {
            format!(
                "another Carillon operation is changing the user units in {}",
                units.display()
            )
        })?;
        Ok(Self { _lock: lock })
    }
}

/// Exclusive lock over an installation's durable state directory. Core
/// holds one while serving; backup takes it to prove core is stopped and to
/// keep it from starting meanwhile.
#[derive(Debug)]
pub struct ServiceLock {
    _lock: Locked,
}

impl ServiceLock {
    #[must_use]
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("service.lock")
    }

    /// Take the lock, or report that core owns the state.
    ///
    /// # Errors
    ///
    /// Returns an error when the lock is held or its file cannot be opened.
    pub fn acquire(data_dir: &Path) -> Result<Self> {
        Self::try_acquire(data_dir)?.with_context(|| {
            format!(
                "Carillon core already owns {}",
                Self::path(data_dir).display()
            )
        })
    }

    /// Take the lock, or return `None` while core or another holder has it.
    ///
    /// # Errors
    ///
    /// Returns an error when the state directory or lock file cannot be
    /// created, or the lock fails for a reason other than another holder.
    pub fn try_acquire(data_dir: &Path) -> Result<Option<Self>> {
        fs::create_dir_all(data_dir).with_context(|| format!("create {}", data_dir.display()))?;
        Ok(try_lock(&Self::path(data_dir))?.map(|lock| Self { _lock: lock }))
    }
}

/// A file holding an exclusive `flock(2)` lock until dropped. The lock is
/// its open file description's, which a child process spawned meanwhile
/// shares until its exec closes its copy, so closing this descriptor alone
/// may leave the lock held; dropping unlocks the description first.
#[derive(Debug)]
struct Locked(fs::File);

impl Drop for Locked {
    fn drop(&mut self) {
        // A failed unlock leaves the close to release it, as before.
        let _ = self.0.unlock();
    }
}

/// The exclusively locked file at `path`, or `None` while another holds it.
fn try_lock(path: &Path) -> Result<Option<Locked>> {
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("open lock {}", path.display()))?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(Locked(file))),
        Err(error) if error.kind() == fs2::lock_contended_error().kind() => Ok(None),
        Err(error) => Err(error).with_context(|| format!("lock {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use anyhow::Context as _;

    use super::MaintenanceLock;

    #[test]
    fn only_the_installation_named_holds_its_lock_for_a_child() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let home = directory.path();
        assert!(super::held_for(home, Some(home.as_os_str())));
        // The same installation, however the caller spelt it.
        assert!(super::held_for(home, Some(home.join(".").as_os_str())));
        assert!(!super::held_for(home, Some(OsStr::new("/elsewhere"))));
        assert!(!super::held_for(home, None));
        Ok(())
    }

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

    /// The lock belongs to the open file description, which a child process
    /// spawned meanwhile shares until its exec closes its copy. Dropping the
    /// holder releases the lock even while such a copy remains.
    #[test]
    fn dropping_the_holder_releases_the_lock_a_pending_child_shares() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let held = super::try_lock(&MaintenanceLock::path(directory.path()))?
            .context("the lock was not free")?;
        let child = held.0.try_clone()?;
        drop(held);
        assert!(MaintenanceLock::try_acquire(directory.path())?.is_some());
        drop(child);
        Ok(())
    }
}
