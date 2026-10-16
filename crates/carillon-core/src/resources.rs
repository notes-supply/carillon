//! Content-addressed snapshots of agent resources.
//!
//! A turn records the resource state it was accepted with, so later edits to
//! the live resource root cannot change what an already-queued turn runs
//! with. A snapshot is stored under its content address — a digest of every
//! selected file's path, bytes, and effective executability — and shared
//! between identical turns.

use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use rustix::fs::{Access, AtFlags, CWD, accessat};
use sha2::{Digest, Sha256};

#[derive(PartialEq, Eq)]
struct ResourceFile {
    bytes: Vec<u8>,
    executable: u32,
}

impl ResourceFile {
    fn read(path: &Path) -> Result<Self> {
        Ok(Self {
            bytes: fs::read(path)?,
            // Ownership changes on capture. Ask the OS about this owner's
            // effective access (including ACLs), rather than copying bit slots.
            executable: match accessat(CWD, path, Access::EXEC_OK, AtFlags::EACCESS) {
                Ok(()) => 0o110,
                Err(rustix::io::Errno::ACCESS) => 0,
                Err(error) => return Err(error.into()),
            },
        })
    }
}

/// Captures the current resource root into the snapshot store and returns the
/// content address identifying it. An agent without resources captures the
/// empty snapshot.
pub(crate) fn materialize(source: Option<&Path>, store: &Path) -> Result<String> {
    let files = match source {
        Some(source) => selected_resource_files(source)?,
        None => BTreeMap::new(),
    };

    let id = content_address(&files);
    let destination = store.join(&id);
    if destination.exists() {
        if selected_resource_files(&destination)? != files {
            bail!(
                "resource snapshot {} does not match its content address",
                destination.display()
            );
        }
    } else {
        // The store is an immediate child of the already-created core data directory.
        match fs::create_dir(store) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let temporary = store.join(format!(".{}", uuid::Uuid::now_v7()));
        fs::create_dir(&temporary)?;
        let result = (|| -> Result<()> {
            for (relative, file) in &files {
                let path = temporary.join(relative);
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&path, &file.bytes)?;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o440 | file.executable))?;
            }
            set_snapshot_directory_permissions(&temporary)?;
            carillon_installation::filesystem::sync_tree(&temporary)?;
            match fs::rename(&temporary, &destination) {
                Ok(()) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::DirectoryNotEmpty
                    ) =>
                {
                    // Another admission published the same content while we captured it.
                    // Never hide a damaged winner or an unrelated publication failure.
                    if !fs::symlink_metadata(&destination)?.is_dir()
                        || selected_resource_files(&destination)? != files
                    {
                        bail!(
                            "resource snapshot {} does not match its content address",
                            destination.display()
                        );
                    }
                    fs::remove_dir_all(&temporary)?;
                }
                Err(error) => return Err(error.into()),
            }
            Ok(())
        })();
        if let Err(error) = result {
            let _ = fs::remove_dir_all(&temporary);
            return Err(error);
        }
    }
    // Also sync on reuse: a previous attempt may have renamed successfully but
    // failed to persist the directory entry. No accepted turn may outrun this.
    fs::File::open(store)?.sync_all()?;
    fs::File::open(store.parent().context("snapshot store has no parent")?)?.sync_all()?;
    Ok(id)
}

/// Check an archived core-owned snapshot without publishing or repairing it.
pub(crate) fn validate(id: &str, store: &Path) -> Result<()> {
    if id.len() != 64
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("invalid resource snapshot content address");
    }
    let root = store.join(id);
    if !fs::symlink_metadata(&root)?.file_type().is_dir() {
        bail!(
            "resource snapshot root is not a directory: {}",
            root.display()
        );
    }
    if content_address(&selected_resource_files(&root)?) != id {
        bail!("resource snapshot {id} does not match its content address");
    }
    Ok(())
}

/// Where a snapshot lives in the given store. Resolved at claim time, so a
/// relocated store still points at the right content.
pub(crate) fn root(store: &Path, id: &str) -> PathBuf {
    store.join(id)
}

fn selected_resource_files(source: &Path) -> Result<BTreeMap<String, ResourceFile>> {
    if !fs::metadata(source)
        .with_context(|| format!("read agent resource root {}", source.display()))?
        .is_dir()
    {
        bail!(
            "agent resource root is not a directory: {}",
            source.display()
        );
    }
    let mut files = BTreeMap::new();
    for name in ["SYSTEM.md", "AGENTS.md", "settings.json"] {
        let path = source.join(name);
        if let Some(metadata) = resource_metadata(&path)? {
            if !metadata.file_type().is_file() {
                bail!("agent resource is not a regular file: {}", path.display());
            }
            files.insert(name.to_owned(), ResourceFile::read(&path)?);
        }
    }
    collect_resource_tree(source, &source.join("extensions"), "extensions", &mut files)?;
    collect_resource_tree(source, &source.join("skills"), "skills", &mut files)?;
    Ok(files)
}

fn collect_resource_tree(
    source_root: &Path,
    directory: &Path,
    prefix: &str,
    files: &mut BTreeMap<String, ResourceFile>,
) -> Result<()> {
    let Some(metadata) = resource_metadata(directory)? else {
        return Ok(());
    };
    if !metadata.is_dir() {
        bail!("agent resource is not a directory: {}", directory.display());
    }
    for entry in fs::read_dir(directory)
        .with_context(|| format!("read resource directory {}", directory.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            bail!(
                "resource symlinks are not supported: {}",
                entry.path().display()
            );
        }
        if file_type.is_dir() {
            collect_resource_tree(source_root, &entry.path(), prefix, files)?;
        } else if file_type.is_file() {
            let relative = entry
                .path()
                .strip_prefix(source_root)?
                .to_string_lossy()
                .into_owned();
            if !relative.starts_with(prefix) {
                bail!("resource escaped selected directory: {relative}");
            }
            files.insert(relative, ResourceFile::read(&entry.path())?);
        } else {
            bail!(
                "resource is not a regular file or directory: {}",
                entry.path().display()
            );
        }
    }
    Ok(())
}

fn resource_metadata(path: &Path) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.is_symlink() {
                bail!("resource symlinks are not supported: {}", path.display());
            }
            Ok(Some(metadata))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn content_address(files: &BTreeMap<String, ResourceFile>) -> String {
    let mut digest = Sha256::new();
    for (path, file) in files {
        digest.update((path.len() as u64).to_le_bytes());
        digest.update(path.as_bytes());
        digest.update(file.executable.to_le_bytes());
        digest.update((file.bytes.len() as u64).to_le_bytes());
        digest.update(&file.bytes);
    }
    hex::encode(digest.finalize())
}

fn set_snapshot_directory_permissions(directory: &Path) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            set_snapshot_directory_permissions(&entry.path())?;
        }
    }
    fs::set_permissions(directory, fs::Permissions::from_mode(0o750))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt, process::Command};

    use anyhow::Context;
    use tempfile::tempdir;

    use super::{materialize, root};

    #[test]
    fn concurrent_publication_reuses_the_winner_and_removes_staging() -> anyhow::Result<()> {
        let source = tempdir()?;
        let parent = tempdir()?;
        fs::write(source.path().join("SYSTEM.md"), vec![b'x'; 4 * 1024 * 1024])?;
        let store = parent.path().join("snapshots");
        let barrier = std::sync::Barrier::new(8);
        let ids = std::thread::scope(|scope| -> anyhow::Result<Vec<String>> {
            let tasks: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        materialize(Some(source.path()), &store)
                    })
                })
                .collect();
            tasks
                .into_iter()
                .map(|task| {
                    task.join()
                        .map_err(|_| anyhow::anyhow!("capture thread panicked"))?
                })
                .collect()
        })?;
        assert!(ids.iter().all(|id| id == &ids[0]));
        assert_eq!(fs::read_dir(&store)?.count(), 1);
        super::validate(&ids[0], &store)?;
        let captured = store.join(&ids[0]).join("SYSTEM.md");
        fs::set_permissions(&captured, fs::Permissions::from_mode(0o600))?;
        fs::write(captured, "damaged")?;
        let refused = materialize(Some(source.path()), &store)
            .err()
            .context("reused a damaged snapshot")?;
        assert!(
            format!("{refused:#}").contains("does not match its content address"),
            "{refused:#}"
        );
        Ok(())
    }

    #[test]
    fn snapshots_refuse_unsupported_entries_instead_of_omitting_them() -> anyhow::Result<()> {
        let source = tempdir()?;
        let parent = tempdir()?;
        let store = parent.path().join("snapshots");
        fs::create_dir(source.path().join("skills"))?;
        let socket = source.path().join("skills/live.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket)?;
        let error = materialize(Some(source.path()), &store)
            .err()
            .context("socket was omitted")?;
        assert!(
            error
                .to_string()
                .contains("not a regular file or directory")
        );
        assert!(!store.exists());
        fs::remove_file(socket)?;
        let snapshot = materialize(Some(source.path()), &store)?;
        super::validate(&snapshot, &store)?;
        Ok(())
    }

    #[test]
    fn snapshots_preserve_effective_owner_execution_not_other_bit_slots() -> anyhow::Result<()> {
        let source = tempdir()?;
        let parent = tempdir()?;
        let store = parent.path().join("snapshots");
        let helper = source.path().join("skills/helper");
        fs::create_dir(source.path().join("skills"))?;
        fs::write(&helper, b"#!/bin/sh\nprintf native-helper")?;
        let mut executable_snapshot = None;
        let mut ordinary_snapshot = None;
        for mode in [0o450, 0o405, 0o455, 0o500, 0o550, 0o555] {
            fs::set_permissions(&helper, fs::Permissions::from_mode(mode))?;
            let live = Command::new(&helper).output();
            let snapshot = materialize(Some(source.path()), &store)?;
            let captured = Command::new(root(&store, &snapshot).join("skills/helper")).output();
            assert_eq!(live.is_ok(), captured.is_ok(), "mode {mode:o}");
            let previous = if mode & 0o100 != 0 {
                &mut executable_snapshot
            } else {
                &mut ordinary_snapshot
            };
            if let Some(id) = previous {
                assert_eq!(
                    id, &snapshot,
                    "equivalent effective permissions changed capture"
                );
            } else {
                *previous = Some(snapshot.clone());
            }
            if let Ok(output) = captured {
                assert!(output.status.success());
                assert_eq!(output.stdout, b"native-helper");
            }
            super::validate(&snapshot, &store)?;
            assert_eq!(materialize(Some(source.path()), &store)?, snapshot);
        }
        assert_ne!(executable_snapshot, ordinary_snapshot);
        Ok(())
    }

    #[test]
    fn snapshots_reject_symlinked_subtree_roots_and_dangling_resources() -> anyhow::Result<()> {
        use std::os::unix::fs::symlink;

        let external = tempdir()?;
        fs::write(
            external.path().join("private.txt"),
            "outside snapshot selection",
        )?;
        for name in [
            "extensions",
            "skills",
            "SYSTEM.md",
            "AGENTS.md",
            "settings.json",
        ] {
            for destination in [
                external.path().to_path_buf(),
                external.path().join("absent"),
            ] {
                let source = tempdir()?;
                let parent = tempdir()?;
                let store = parent.path().join("snapshots");
                let link = source.path().join(name);
                symlink(destination, &link)?;
                let error = materialize(Some(source.path()), &store)
                    .err()
                    .context("symlink was accepted")?;
                assert!(
                    error
                        .to_string()
                        .contains("resource symlinks are not supported")
                );
                assert!(!store.exists(), "rejected resource published a snapshot");
                fs::remove_file(link)?;
                // Missing optional resources are still valid; refusal leaves retry usable.
                let snapshot = materialize(Some(source.path()), &store)?;
                assert!(fs::read_dir(root(&store, &snapshot))?.next().is_none());
            }
        }
        Ok(())
    }

    #[test]
    fn snapshots_preserve_native_helpers_and_address_executable_changes() -> anyhow::Result<()> {
        let source = tempdir()?;
        let parent = tempdir()?;
        let store = parent.path().join("resource-snapshots");
        for relative in ["skills/example/scripts/helper", "extensions/example/helper"] {
            let helper = source.path().join(relative);
            fs::create_dir_all(helper.parent().context("missing parent")?)?;
            fs::write(&helper, b"#!/bin/sh\nprintf native-helper")?;
            fs::set_permissions(&helper, fs::Permissions::from_mode(0o750))?;
        }
        let executable = materialize(Some(source.path()), &store)?;
        for relative in ["skills/example/scripts/helper", "extensions/example/helper"] {
            let helper = root(&store, &executable).join(relative);
            let output = Command::new(&helper).output()?;
            assert!(output.status.success());
            assert_eq!(output.stdout, b"native-helper");
            assert_eq!(fs::metadata(&helper)?.permissions().mode() & 0o777, 0o550);
            fs::set_permissions(
                source.path().join(relative),
                fs::Permissions::from_mode(0o640),
            )?;
        }
        let ordinary = materialize(Some(source.path()), &store)?;
        assert_ne!(executable, ordinary);
        assert_eq!(materialize(Some(source.path()), &store)?, ordinary);
        for relative in ["skills/example/scripts/helper", "extensions/example/helper"] {
            // Source chmod affects future turns only; already accepted helpers still run.
            assert!(
                Command::new(root(&store, &executable).join(relative))
                    .output()?
                    .status
                    .success()
            );
            assert_eq!(
                Command::new(root(&store, &ordinary).join(relative))
                    .output()
                    .err()
                    .context("non-executable helper ran")?
                    .kind(),
                std::io::ErrorKind::PermissionDenied
            );
        }
        // Comparison must check executable metadata too, not silently reuse tampered content.
        fs::set_permissions(
            root(&store, &ordinary).join("skills/example/scripts/helper"),
            fs::Permissions::from_mode(0o550),
        )?;
        let refused = materialize(Some(source.path()), &store)
            .err()
            .context("reused a damaged snapshot")?;
        assert!(
            format!("{refused:#}").contains("does not match its content address"),
            "{refused:#}"
        );
        let refused = super::validate(&ordinary, &store)
            .err()
            .context("validated a tampered snapshot")?;
        assert!(
            format!("{refused:#}").contains("does not match its content address"),
            "{refused:#}"
        );
        Ok(())
    }

    #[test]
    fn materialized_resources_do_not_change_with_the_source() -> anyhow::Result<()> {
        let source = tempdir()?;
        let store = tempdir()?;
        fs::write(source.path().join("SYSTEM.md"), "first")?;
        fs::create_dir(source.path().join("skills"))?;
        let first = materialize(Some(source.path()), store.path())?;
        fs::write(source.path().join("SYSTEM.md"), "second")?;
        let second = materialize(Some(source.path()), store.path())?;
        assert_ne!(first, second);
        assert_eq!(
            fs::read_to_string(root(store.path(), &first).join("SYSTEM.md"))?,
            "first"
        );
        Ok(())
    }

    #[test]
    fn an_agent_without_resources_captures_the_empty_snapshot() -> anyhow::Result<()> {
        let store = tempdir()?;
        let empty = materialize(None, store.path())?;
        assert_eq!(materialize(None, store.path())?, empty);
        assert!(fs::read_dir(root(store.path(), &empty))?.next().is_none());
        super::validate(&empty, store.path())?;
        let source = tempdir()?;
        assert_eq!(materialize(Some(source.path()), store.path())?, empty);
        Ok(())
    }
}
