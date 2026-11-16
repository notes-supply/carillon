//! Core's SQLite file. The path is private to this module, so the rest of
//! `db` reaches SQLite only through [`Database::read`] and
//! [`Database::write`].

use std::{
    collections::BTreeSet,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior};

use super::{OtherFormat, SCHEMA};

#[derive(Clone)]
pub(crate) struct Database {
    path: PathBuf,
    /// The interfaces whose people read answers from core's record.
    retained: BTreeSet<String>,
}

impl Database {
    pub(crate) fn create(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)
                .with_context(|| format!("create {}", parent.display()))?;
            let metadata = std::fs::symlink_metadata(parent)?;
            if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
                bail!(
                    "database directory must be private (0700): {}",
                    parent.display()
                );
            }
        }
        // Reserve the name atomically before SQLite opens it. Existing files,
        // including symlinks and incomplete prior creations, are never adopted.
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("create fresh database {}", path.display()))?;
        let database = Self::at(path);
        database.write(|transaction| Ok(transaction.execute_batch(SCHEMA)?))?;
        Ok(database)
    }

    pub(crate) fn open(path: &Path) -> Result<Self> {
        Self::validate_backup(path)?;
        Ok(Self::at(path))
    }

    fn at(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            retained: BTreeSet::new(),
        }
    }

    /// This storage, writing the answers of the `retained` interfaces as
    /// delivered.
    pub(crate) fn retaining(self, retained: BTreeSet<String>) -> Self {
        Self { retained, ..self }
    }

    pub(super) fn retains(&self, interface_id: &str) -> bool {
        self.retained.contains(interface_id)
    }

    fn connect(&self, flags: OpenFlags) -> Result<Connection> {
        let connection = Connection::open_with_flags(&self.path, flags)
            .with_context(|| format!("open {}", self.path.display()))?;
        connection.busy_timeout(Duration::from_secs(5))?;
        Ok(connection)
    }

    pub(super) fn read(&self) -> Result<Connection> {
        self.connect(OpenFlags::SQLITE_OPEN_READ_ONLY)
    }

    /// The one way to change what is stored. `work` commits when it returns
    /// `Ok`, and what it wrote is discarded when it fails.
    ///
    /// The transaction is immediate, so writers serialize up front under
    /// the busy timeout instead of failing later on a deferred
    /// read-to-write upgrade.
    pub(super) fn write<T>(&self, work: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let mut connection = self.connect(OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let value = work(&transaction)?;
        transaction.commit()?;
        Ok(value)
    }

    /// Validate without creating a missing database or repairing an old schema.
    pub(crate) fn validate_backup(path: &Path) -> Result<()> {
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let integrity: String =
            connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            bail!("SQLite integrity check failed: {integrity}");
        }
        if connection.prepare("PRAGMA foreign_key_check")?.exists([])? {
            bail!("SQLite foreign key check failed");
        }
        let current = Connection::open_in_memory()?;
        current.execute_batch(SCHEMA)?;
        let schema = |database: &Connection| -> Result<Vec<(String, String, String)>> {
            Ok(database.prepare(
                "SELECT type, name, sql FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name"
            )?.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        };
        if schema(&connection)? != schema(&current)? {
            return Err(OtherFormat.into());
        }
        Ok(())
    }
}
