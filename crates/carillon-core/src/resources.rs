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

/// Captures the current resource root into the snapshot store and returns the
/// fingerprints identifying it.
pub fn materialize(source: &Path, store: &Path) -> Result<ResourceSnapshot> {
    let files = selected_resource_files(source)?;

    let system_sha256 = hash_bytes(files.get("SYSTEM.md").map_or(&[], Vec::as_slice));
    let context_sha256 = hash_bytes(files.get("AGENTS.md").map_or(&[], Vec::as_slice));
    let settings_sha256 = hash_bytes(files.get("settings.json").map_or(&[], Vec::as_slice));
    let extensions_sha256 = hash_entries(&files, "extensions/");
    let skills_sha256 = hash_entries(&files, "skills/");
    let fingerprint = hash_bytes(
        format!(
            "{system_sha256}{context_sha256}{settings_sha256}{extensions_sha256}{skills_sha256}"
        )
        .as_bytes(),
    );
    let destination = store.join(&fingerprint);
    if destination.exists() {
        if selected_resource_files(&destination)? != files {
            bail!(
                "resource snapshot {} does not match its content address",
                destination.display()
            );
        }
    } else {
        fs::create_dir_all(store)?;
        let temporary = store.join(format!(".{}", uuid::Uuid::now_v7()));
        fs::create_dir(&temporary)?;
        for (relative, bytes) in &files {
            let path = temporary.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&path, bytes)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o440))?;
        }
        set_snapshot_directory_permissions(&temporary)?;
        fs::rename(temporary, &destination)?;
    }
    Ok(ResourceSnapshot {
        id: fingerprint,
        system_sha256,
        context_sha256,
        settings_sha256,
        extensions_sha256,
        skills_sha256,
    })
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

fn selected_resource_files(source: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut files = BTreeMap::new();
    for name in ["SYSTEM.md", "AGENTS.md", "settings.json"] {
        let path = source.join(name);
        if path.exists() {
            let metadata = fs::symlink_metadata(&path)?;
            if !metadata.file_type().is_file() {
                bail!("agent resource is not a regular file: {}", path.display());
            }
            files.insert(name.to_owned(), fs::read(&path)?);
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
    files: &mut BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    if !directory.exists() {
        return Ok(());
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
            files.insert(relative, fs::read(entry.path())?);
        }
    }
    Ok(())
}

fn hash_entries(files: &BTreeMap<String, Vec<u8>>, prefix: &str) -> String {
    let mut digest = Sha256::new();
    for (path, bytes) in files.range(prefix.to_owned()..) {
        if !path.starts_with(prefix) {
            break;
        }
        digest.update(path.strip_prefix(prefix).unwrap_or(path).as_bytes());
        digest.update([0]);
        digest.update(bytes);
        digest.update([0]);
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
    use std::fs;

    use tempfile::tempdir;

    use super::{materialize, runtime_view};

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
