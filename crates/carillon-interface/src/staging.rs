use anyhow::{Context as _, Result};
use std::{
    fs::DirBuilder,
    io,
    os::unix::fs::DirBuilderExt as _,
    path::{Path, PathBuf},
};
use tracing::warn;
use uuid::Uuid;

pub struct Staging {
    directory: PathBuf,
}

impl Staging {
    /// Prepare `directory`, removing what a previous instance left there.
    /// Call only once this instance owns the provider's state.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory cannot be cleared or created.
    pub fn open(directory: PathBuf) -> Result<Self> {
        match std::fs::remove_dir_all(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("clear {}", directory.display()));
            }
        }
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)
            .with_context(|| format!("create {}", directory.display()))?;
        Ok(Self { directory })
    }

    /// A batch under a name no other request has: core may still be reading
    /// the files of an earlier attempt at the same event.
    ///
    /// # Errors
    ///
    /// Returns an error when its directory cannot be created.
    pub fn batch(&self) -> io::Result<Batch> {
        let name = Uuid::now_v7().simple().to_string();
        let directory = self.directory.join(&name);
        DirBuilder::new().mode(0o700).create(&directory)?;
        Ok(Batch { directory, name })
    }
}

/// The files of one event. Dropping it removes them, on the thread that
/// drops it.
pub struct Batch {
    directory: PathBuf,
    name: String,
}

impl Batch {
    /// The name the event gives core for these files.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Where the event's file `ordinal` belongs, counting from zero.
    #[must_use]
    pub fn file(&self, ordinal: usize) -> PathBuf {
        self.directory.join(ordinal.to_string())
    }
}

impl Drop for Batch {
    fn drop(&mut self) {
        if let Err(error) = remove(&self.directory) {
            warn!(%error, "staged attachments could not be removed; the provider removes them at startup");
        }
    }
}

fn remove(directory: &Path) -> io::Result<()> {
    match std::fs::remove_dir_all(directory) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}
