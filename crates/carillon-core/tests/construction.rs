use anyhow::Context as _;
use carillon_core::{AgentOptions, Core, CoreOptions};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    sync::{Arc, Barrier},
};

fn options(root: &std::path::Path) -> CoreOptions {
    CoreOptions {
        data_directory: root.join("data"),
        agents: vec![AgentOptions {
            id: "test".into(),
            runtime: "test".into(),
            resources: Some(root.into()),
        }],
        runtimes: vec![],
        attachments: carillon_installation::config::AttachmentLimits::default(),
    }
}

#[test]
fn creation_is_private_and_never_adopts_existing_storage() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let config = options(root.path());
    let core = Core::create(config.clone())?;
    let path = config.data_directory.join("carillon.sqlite3");
    assert_eq!(
        fs::metadata(&config.data_directory)?.permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
    let prior = fs::read(&path)?;
    let refused = Core::create(config.clone())
        .err()
        .context("created over existing storage")?;
    assert_eq!(
        refused
            .root_cause()
            .downcast_ref::<std::io::Error>()
            .map(std::io::Error::kind),
        Some(std::io::ErrorKind::AlreadyExists),
        "{refused:#}"
    );
    assert_eq!(fs::read(&path)?, prior);
    Core::open(config.clone())?;
    drop(core);
    fs::remove_file(&path)?;
    let other = root.path().join("unrelated");
    fs::write(&other, "do not touch")?;
    symlink(&other, &path)?;
    let refused = Core::create(config)
        .err()
        .context("created over a symlink")?;
    assert_eq!(
        refused
            .root_cause()
            .downcast_ref::<std::io::Error>()
            .map(std::io::Error::kind),
        Some(std::io::ErrorKind::AlreadyExists),
        "{refused:#}"
    );
    assert_eq!(fs::read_to_string(other)?, "do not touch");
    Ok(())
}

#[test]
fn concurrent_creators_have_one_winner_and_public_parents_are_refused() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let config = options(root.path());
    let barrier = Arc::new(Barrier::new(8));
    let successes = std::thread::scope(|scope| {
        let tasks: [_; 8] = std::array::from_fn(|_| {
            let config = config.clone();
            let barrier = barrier.clone();
            scope.spawn(move || {
                barrier.wait();
                Core::create(config).is_ok()
            })
        });
        tasks
            .into_iter()
            .map(|task| task.join().unwrap_or(false))
            .filter(|ok| *ok)
            .count()
    });
    assert_eq!(successes, 1);
    let public = root.path().join("public");
    fs::create_dir(&public)?;
    fs::set_permissions(&public, fs::Permissions::from_mode(0o755))?;
    let mut config = config;
    config.data_directory = public.clone();
    let refused = Core::create(config)
        .err()
        .context("created under a public parent")?;
    assert!(
        format!("{refused:#}").contains("database directory must be private (0700)"),
        "{refused:#}"
    );
    assert!(!public.join("carillon.sqlite3").exists());
    assert_eq!(fs::metadata(public)?.permissions().mode() & 0o777, 0o755);
    Ok(())
}
