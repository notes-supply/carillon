//! Content-addressed snapshots of agent resources.
//!
//! A turn records the resource state it was accepted with, so later edits to
//! the live resource root cannot change what an already-queued turn runs
//! with. Snapshots are stored under their content fingerprint and shared
//! between identical turns.

use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt, path::Path};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::model::{ResourceSnapshot, RuntimeResources};

#[derive(PartialEq, Eq)]
struct ResourceFile {
    bytes: Vec<u8>,
    executable: u32,
}

impl ResourceFile {
    fn read(path: &Path) -> Result<Self> {
        Ok(Self {
            bytes: fs::read(path)?,
            executable: fs::metadata(path)?.permissions().mode() & 0o111,
        })
    }
}

/// Captures the current resource root into the snapshot store and returns the
/// fingerprints identifying it.
pub fn materialize(source: &Path, store: &Path) -> Result<ResourceSnapshot> {
    let files = selected_resource_files(source)?;

    let snapshot = describe(&files);
    let destination = store.join(&snapshot.id);
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
            crate::filesystem::sync_tree(&temporary)?;
            fs::rename(&temporary, &destination)?;
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
    Ok(snapshot)
}

fn describe(files: &BTreeMap<String, ResourceFile>) -> ResourceSnapshot {
    ResourceSnapshot {
        id: hash_entries(files, ""),
        system_sha256: hash_bytes(files.get("SYSTEM.md").map_or(&[], |file| &file.bytes)),
        context_sha256: hash_bytes(files.get("AGENTS.md").map_or(&[], |file| &file.bytes)),
        settings_sha256: hash_bytes(files.get("settings.json").map_or(&[], |file| &file.bytes)),
        extensions_sha256: hash_entries(files, "extensions/"),
        skills_sha256: hash_entries(files, "skills/"),
    }
}

/// Check archived core-owned resources without publishing or repairing them.
pub fn validate(snapshot: &ResourceSnapshot, store: &Path) -> Result<()> {
    if snapshot.id.len() != 64
        || !snapshot
            .id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("invalid resource snapshot content address");
    }
    let root = store.join(&snapshot.id);
    if !fs::symlink_metadata(&root)?.file_type().is_dir() {
        bail!(
            "resource snapshot root is not a directory: {}",
            root.display()
        );
    }
    if describe(&selected_resource_files(&root)?) != *snapshot {
        bail!(
            "resource snapshot {} does not match its recorded hashes",
            snapshot.id
        );
    }
    Ok(())
}

/// The runtime's view of a snapshot: its on-disk location in the given store
/// plus the fingerprints, resolved at claim time so a relocated store still
/// points at the right content.
pub fn runtime_view(snapshot: &ResourceSnapshot, store: &Path) -> RuntimeResources {
    RuntimeResources {
        root: store.join(&snapshot.id).display().to_string(),
        system_sha256: snapshot.system_sha256.clone(),
        context_sha256: snapshot.context_sha256.clone(),
        settings_sha256: snapshot.settings_sha256.clone(),
        extensions_sha256: snapshot.extensions_sha256.clone(),
        skills_sha256: snapshot.skills_sha256.clone(),
    }
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

fn hash_entries(files: &BTreeMap<String, ResourceFile>, prefix: &str) -> String {
    let mut digest = Sha256::new();
    for (path, file) in files.range(prefix.to_owned()..) {
        if !path.starts_with(prefix) {
            break;
        }
        let relative = path.strip_prefix(prefix).unwrap_or(path).as_bytes();
        digest.update((relative.len() as u64).to_le_bytes());
        digest.update(relative);
        digest.update(file.executable.to_le_bytes());
        digest.update((file.bytes.len() as u64).to_le_bytes());
        digest.update(&file.bytes);
    }
    hex::encode(digest.finalize())
}

fn hash_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
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

    use super::{materialize, runtime_view};

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
                let error = materialize(source.path(), &store)
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
                let snapshot = materialize(source.path(), &store)?;
                assert!(fs::read_dir(store.join(snapshot.id))?.next().is_none());
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
        let executable = materialize(source.path(), &store)?;
        for relative in ["skills/example/scripts/helper", "extensions/example/helper"] {
            let helper = store.join(&executable.id).join(relative);
            let output = Command::new(&helper).output()?;
            assert!(output.status.success());
            assert_eq!(output.stdout, b"native-helper");
            assert_eq!(fs::metadata(&helper)?.permissions().mode() & 0o777, 0o550);
            fs::set_permissions(
                source.path().join(relative),
                fs::Permissions::from_mode(0o640),
            )?;
        }
        let ordinary = materialize(source.path(), &store)?;
        assert_ne!(executable.id, ordinary.id);
        assert_ne!(executable.skills_sha256, ordinary.skills_sha256);
        assert_ne!(executable.extensions_sha256, ordinary.extensions_sha256);
        assert_eq!(materialize(source.path(), &store)?.id, ordinary.id);
        for relative in ["skills/example/scripts/helper", "extensions/example/helper"] {
            // Source chmod affects future turns only; already accepted helpers still run.
            assert!(
                Command::new(store.join(&executable.id).join(relative))
                    .output()?
                    .status
                    .success()
            );
            assert_eq!(
                Command::new(store.join(&ordinary.id).join(relative))
                    .output()
                    .err()
                    .context("non-executable helper ran")?
                    .kind(),
                std::io::ErrorKind::PermissionDenied
            );
        }
        // Comparison must check executable metadata too, not silently reuse tampered content.
        fs::set_permissions(
            store
                .join(&ordinary.id)
                .join("skills/example/scripts/helper"),
            fs::Permissions::from_mode(0o550),
        )?;
        assert!(materialize(source.path(), &store).is_err());
        Ok(())
    }

    #[test]
    fn materialized_resources_do_not_change_with_the_source() -> anyhow::Result<()> {
        let source = tempdir()?;
        let store = tempdir()?;
        fs::write(source.path().join("SYSTEM.md"), "first")?;
        fs::create_dir(source.path().join("skills"))?;
        let first = materialize(source.path(), store.path())?;
        fs::write(source.path().join("SYSTEM.md"), "second")?;
        let second = materialize(source.path(), store.path())?;
        assert_ne!(first.system_sha256, second.system_sha256);
        assert_eq!(first.skills_sha256, second.skills_sha256);
        assert_ne!(first.id, second.id);
        assert_eq!(
            fs::read_to_string(store.path().join(&first.id).join("SYSTEM.md"))?,
            "first"
        );
        let relocated = tempdir()?;
        assert_eq!(
            runtime_view(&first, relocated.path()).root,
            relocated.path().join(&first.id).display().to_string()
        );
        Ok(())
    }
}
