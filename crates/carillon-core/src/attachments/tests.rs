//! Capture store behaviour against real files.
use std::{
    collections::BTreeSet,
    fs,
    os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink},
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result};
use carillon_protocol::model::{StagedAttachments, StagedFile};

use super::{Cancel, CaptureError, CapturedFile, check, reconcile, store, validate_store};

fn capture(
    data: &Path,
    interface: &str,
    staged: &StagedAttachments,
    cancel: &Cancel,
) -> Result<super::Capture, CaptureError> {
    super::capture(data, super::open(data, interface, staged)?, cancel)
}

fn staging(data: &Path) -> PathBuf {
    data.join("components/interface/incoming")
}

fn stage(data: &Path, batch: &str, files: &[&[u8]]) -> Result<StagedAttachments> {
    let directory = staging(data).join(batch);
    fs::create_dir_all(&directory)?;
    for (ordinal, bytes) in files.iter().enumerate() {
        fs::write(directory.join(ordinal.to_string()), bytes)?;
    }
    Ok(StagedAttachments {
        batch: batch.to_owned(),
        files: files
            .iter()
            .enumerate()
            .map(|(ordinal, bytes)| StagedFile {
                name: format!("file {ordinal}.txt"),
                size: bytes.len() as u64,
                content_type: Some("text/plain".to_owned()),
            })
            .collect(),
    })
}

fn captured(capture: &super::Capture, staged: &StagedAttachments) -> Vec<CapturedFile> {
    staged
        .files
        .iter()
        .zip(&capture.digests)
        .enumerate()
        .map(|(ordinal, (file, digest))| CapturedFile {
            capture: capture.id.clone(),
            ordinal: u32::try_from(ordinal).unwrap_or(u32::MAX),
            name: file.name.clone(),
            size: file.size,
            content_type: file.content_type.clone(),
            sha256: digest.clone(),
        })
        .collect()
}

fn store_entries(data: &Path) -> Result<Vec<String>> {
    let Ok(entries) = fs::read_dir(store(data)) else {
        return Ok(Vec::new());
    };
    let mut names = entries
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
        .collect::<Result<Vec<_>, _>>()?;
    names.sort();
    Ok(names)
}

#[test]
fn captures_are_exact_read_only_copies_independent_of_staging() -> Result<()> {
    let data = tempfile::tempdir()?;
    let staged = stage(data.path(), "batch-1", &[b"first file", b"", b"third"])?;
    let capture = capture(data.path(), "interface", &staged, &Cancel::default())?;
    assert_eq!(
        store_entries(data.path())?,
        std::slice::from_ref(&capture.id)
    );
    assert_eq!(
        capture.digests[0],
        hex::encode(<sha2::Sha256 as sha2::Digest>::digest(b"first file"))
    );
    let published = store(data.path()).join(&capture.id);
    assert_eq!(
        fs::metadata(&published)?.permissions().mode() & 0o777,
        0o750
    );
    for (ordinal, expected) in [b"first file".as_slice(), b"", b"third"].iter().enumerate() {
        let file = published.join(ordinal.to_string());
        let source = staging(data.path())
            .join("batch-1")
            .join(ordinal.to_string());
        assert_eq!(fs::read(&file)?, *expected);
        let metadata = fs::metadata(&file)?;
        assert_eq!(metadata.permissions().mode() & 0o777, 0o440);
        // A copy, not a link to bytes the interface can still change.
        assert_eq!(metadata.nlink(), 1);
        assert_ne!(metadata.ino(), fs::metadata(&source)?.ino());
    }
    // Staging is the interface's until it holds a receipt.
    assert_eq!(
        fs::read(staging(data.path()).join("batch-1/0"))?,
        b"first file"
    );
    fs::write(staging(data.path()).join("batch-1/0"), b"changed later")?;
    for file in captured(&capture, &staged) {
        check(data.path(), &file)?;
    }
    Ok(())
}

#[test]
fn staged_sources_are_confined_regular_files_of_the_declared_length() -> Result<()> {
    let outside = tempfile::tempdir()?;
    fs::write(outside.path().join("secret"), b"outside")?;
    fs::create_dir(outside.path().join("batch"))?;
    fs::write(outside.path().join("batch/0"), b"outside")?;
    // A link at every component below the data directory is refused, even
    // when it points at a file with the declared length.
    for segment in ["components", "interface", "incoming", "batch", "0"] {
        let data = tempfile::tempdir()?;
        let staged = stage(data.path(), "batch", &[b"outside"])?;
        let path = match segment {
            "components" => data.path().join("components"),
            "interface" => data.path().join("components/interface"),
            "incoming" => staging(data.path()),
            "batch" => staging(data.path()).join("batch"),
            _ => staging(data.path()).join("batch/0"),
        };
        let target = tempfile::tempdir()?;
        let moved = target.path().join("moved");
        fs::rename(&path, &moved)?;
        symlink(&moved, &path)?;
        let error = capture(data.path(), "interface", &staged, &Cancel::default())
            .err()
            .context("followed a link")?;
        assert!(
            matches!(error, CaptureError::Invalid(_)),
            "{segment}: {error}"
        );
        assert!(store_entries(data.path())?.is_empty(), "{segment}");
    }

    let data = tempfile::tempdir()?;
    let mut staged = stage(data.path(), "batch", &[b"12345"])?;
    for batch in ["..", "../batch", "batch/0", ".", "", "a.b"] {
        staged.batch = batch.to_owned();
        assert!(matches!(
            capture(data.path(), "interface", &staged, &Cancel::default()),
            Err(CaptureError::Invalid(_))
        ));
    }
    staged.batch = "batch".to_owned();
    for size in [4, 6] {
        staged.files[0].size = size;
        assert!(matches!(
            capture(data.path(), "interface", &staged, &Cancel::default()),
            Err(CaptureError::Invalid(_))
        ));
    }
    staged.files[0].size = 5;
    // Another interface's staging is not this interface's.
    assert!(matches!(
        capture(data.path(), "other", &staged, &Cancel::default()),
        Err(CaptureError::Invalid(_))
    ));
    // A FIFO is refused at once rather than read until a writer appears;
    // directories are refused too.
    let fifo = staging(data.path()).join("batch/0");
    fs::remove_file(&fifo)?;
    rustix::fs::mknodat(
        rustix::fs::CWD,
        &fifo,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        0,
    )?;
    staged.files[0].size = 0;
    assert!(matches!(
        capture(data.path(), "interface", &staged, &Cancel::default()),
        Err(CaptureError::Invalid(_))
    ));
    fs::remove_file(&fifo)?;
    fs::create_dir(&fifo)?;
    assert!(matches!(
        capture(data.path(), "interface", &staged, &Cancel::default()),
        Err(CaptureError::Invalid(_))
    ));
    // Nothing partial or published remains after any refusal.
    assert!(store_entries(data.path())?.is_empty());
    Ok(())
}

#[test]
fn a_cancelled_capture_is_refused_and_leaves_nothing() -> Result<()> {
    let data = tempfile::tempdir()?;
    let staged = stage(data.path(), "batch", &[&vec![7; 3 * super::CHUNK]])?;
    let cancel = Cancel::default();
    cancel.cancel();
    assert!(matches!(
        capture(data.path(), "interface", &staged, &cancel),
        Err(CaptureError::Cancelled)
    ));
    assert!(store_entries(data.path())?.is_empty());
    Ok(())
}

#[test]
fn reconciliation_removes_only_unreferenced_and_partial_captures() -> Result<()> {
    let data = tempfile::tempdir()?;
    let kept = capture(
        data.path(),
        "interface",
        &stage(data.path(), "kept", &[b"kept"])?,
        &Cancel::default(),
    )?;
    let orphan = capture(
        data.path(),
        "interface",
        &stage(data.path(), "orphan", &[b"orphan"])?,
        &Cancel::default(),
    )?;
    // A copy that crashed before publication.
    let partial = store(data.path()).join(".partial-0190a6e0-0000-7000-8000-000000000000");
    fs::create_dir(&partial)?;
    fs::write(partial.join("0"), b"half")?;
    let referenced = BTreeSet::from([kept.id.clone()]);
    let reconciled = reconcile(data.path(), &referenced)?;
    assert_eq!(
        reconciled,
        super::Reconciled {
            partial: 1,
            unreferenced: 1
        }
    );
    assert_eq!(store_entries(data.path())?, std::slice::from_ref(&kept.id));
    assert!(!store(data.path()).join(&orphan.id).exists());
    assert_eq!(
        reconcile(data.path(), &referenced)?,
        super::Reconciled::default()
    );
    // A missing store is nothing to reconcile.
    let empty = tempfile::tempdir()?;
    assert_eq!(
        reconcile(empty.path(), &referenced)?,
        super::Reconciled::default()
    );
    Ok(())
}

#[test]
fn verification_and_store_validation_detect_altered_missing_and_unexpected_files() -> Result<()> {
    let data = tempfile::tempdir()?;
    let staged = stage(data.path(), "batch", &[b"original", b"second"])?;
    let capture = capture(data.path(), "interface", &staged, &Cancel::default())?;
    let files = captured(&capture, &staged);
    validate_store(data.path(), &files)?;
    let first = store(data.path()).join(&capture.id).join("0");

    // Same length, different bytes: only the digest notices.
    fs::set_permissions(&first, fs::Permissions::from_mode(0o640))?;
    fs::write(&first, b"altered!")?;
    let refused = check(data.path(), &files[0])
        .err()
        .context("accepted altered bytes")?;
    assert!(
        format!("{refused:#}").contains("was changed after it was accepted"),
        "{refused:#}"
    );
    let refused = validate_store(data.path(), &files)
        .err()
        .context("accepted an altered store")?;
    assert!(
        format!("{refused:#}").contains("was changed after it was accepted"),
        "{refused:#}"
    );
    fs::write(&first, b"original")?;
    check(data.path(), &files[0])?;

    fs::remove_file(&first)?;
    let refused = check(data.path(), &files[0])
        .err()
        .context("accepted a missing capture")?;
    assert!(format!("{refused:#}").contains("is missing"), "{refused:#}");
    fs::write(&first, b"original")?;

    let extra = store(data.path()).join(&capture.id).join("2");
    fs::write(&extra, b"unexpected")?;
    let refused = validate_store(data.path(), &files)
        .err()
        .context("accepted an unexpected file")?;
    assert!(
        format!("{refused:#}").contains("holds unexpected file"),
        "{refused:#}"
    );
    fs::remove_file(&extra)?;
    let stray = store(data.path()).join("stray");
    fs::create_dir(&stray)?;
    let refused = validate_store(data.path(), &files)
        .err()
        .context("accepted a stray entry")?;
    assert!(
        format!("{refused:#}").contains("holds unexpected entry stray"),
        "{refused:#}"
    );
    fs::remove_dir(&stray)?;
    validate_store(data.path(), &files)?;

    // A stored identity is a capture name, never a path.
    let mut escaping = files[0].clone();
    "../outside".clone_into(&mut escaping.capture);
    let refused = check(data.path(), &escaping)
        .err()
        .context("accepted an escaping capture name")?;
    assert!(
        format!("{refused:#}").contains("invalid captured attachment identity"),
        "{refused:#}"
    );
    Ok(())
}
