//! Filesystem durability for complete, locally generated trees and files.

use std::{fs, io::Write as _, os::unix::fs::PermissionsExt, path::Path};

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

/// Publish a complete file with the given mode via a synced temporary sibling
/// and rename, so readers never observe partial contents.
///
/// # Errors
///
/// Returns the first I/O error encountered; the target is left unchanged.
pub fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(mode))?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    fs::rename(temporary.path(), path)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}
