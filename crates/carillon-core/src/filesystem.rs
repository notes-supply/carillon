//! Filesystem durability for complete, locally generated trees.

use std::{fs, path::Path};

/// Persist file contents and metadata, then each directory's entries bottom-up.
/// Callers must also sync the parent after publishing the tree with rename.
pub(crate) fn sync_tree(path: &Path) -> std::io::Result<()> {
    if path.is_dir() {
        for entry in fs::read_dir(path)? {
            sync_tree(&entry?.path())?;
        }
    }
    fs::File::open(path)?.sync_all()
}

/// Persist a publication directory and the entries naming its ancestors.
pub(crate) fn sync_ancestors(directory: &Path) -> std::io::Result<()> {
    for parent in directory.ancestors() {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}
