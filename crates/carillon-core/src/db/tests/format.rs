use super::*;

#[test]
fn open_refuses_missing_empty_and_obsolete_databases_without_repair() -> Result<()> {
    let directory = tempdir()?;
    let path = directory.path().join("state.sqlite3");
    for (contents, reason) in [
        (None, "unable to open database file"),
        (Some(Vec::new()), "not the current Carillon format"),
        (Some(b"not SQLite".to_vec()), "file is not a database"),
    ] {
        if let Some(bytes) = &contents {
            std::fs::write(&path, bytes)?;
        }
        crate::assert_refused(Database::open(&path), reason)?;
        assert_eq!(std::fs::read(&path).ok(), contents);
    }
    std::fs::remove_file(&path)?;
    rusqlite::Connection::open(&path)?.execute_batch("CREATE TABLE obsolete (id INTEGER)")?;
    let before = std::fs::read(&path)?;
    crate::assert_refused(Database::open(&path), "not the current Carillon format")?;
    assert_eq!(std::fs::read(&path)?, before);
    std::fs::remove_file(&path)?;
    Database::create(&path)?;
    Database::open(&path)?;
    Ok(())
}

#[test]
fn a_database_missing_a_current_index_is_refused_without_repair() -> Result<()> {
    let directory = tempdir()?;
    let path = directory.path().join("state.sqlite3");
    let database = Database::create(&path)?;
    accept(&database, "one", "conversation")?;
    drop(database);
    // An index is part of the format: a database an earlier release wrote
    // without it is refused, and nothing adds it.
    rusqlite::Connection::open(&path)?.execute_batch("DROP INDEX turns_queued")?;
    crate::assert_refused(Database::open(&path), "not the current Carillon format")?;
    let indexes: u32 = rusqlite::Connection::open(&path)?.query_row(
        "SELECT COUNT(*) FROM sqlite_schema WHERE name = 'turns_queued'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(indexes, 0, "a refused database was repaired");
    Ok(())
}

fn accept_in(database: &Database, id: &str, audience: &str, channel: &str) -> Result<()> {
    database.accept_event(
        "discord",
        "runtime",
        &ExternalEvent {
            channel_id: channel.to_owned(),
            ..event(id, audience)
        },
        Admission::Queued {
            providers: &json!([]),
            resource_snapshot_id: "resources",
        },
        None,
    )?;
    Ok(())
}

#[test]
fn a_conversation_keeps_the_channel_of_its_first_message_on_every_turn() -> Result<()> {
    let directory = tempdir()?;
    let database = Database::create(&directory.path().join("state.sqlite3"))?;
    accept_in(&database, "first", "conversation", "300")?;
    accept_in(&database, "second", "conversation", "elsewhere")?;
    accept_in(&database, "other", "other-conversation", "301")?;
    let mut handed = Vec::new();
    for _ in 0..3 {
        handed.push(hold(&database, "runtime")?.channel_id);
    }
    assert_eq!(handed, ["300", "300", "301"]);
    let opening = carillon_protocol::turns::TurnOpening {
        interface_id: "discord".to_owned(),
        agent_id: "agent".to_owned(),
        audience_id: "conversation".to_owned(),
        key: "nightly".to_owned(),
        label: None,
    };
    for duplicate in [false, true] {
        let crate::db::Opened::Receipt(opened) =
            database.open_turn("runtime", Some("runtime"), &opening)?
        else {
            bail!("no turn opened");
        };
        assert_eq!(opened.receipt.duplicate, duplicate);
        assert_eq!(opened.channel_id, "300");
    }
    let running = database.running_turns("runtime")?;
    let channels: Vec<(&str, &str)> = running
        .iter()
        .map(|(turn, _)| (turn.audience_id.as_str(), turn.channel_id.as_str()))
        .collect();
    assert_eq!(
        channels,
        [
            ("conversation", "300"),
            ("conversation", "300"),
            ("other-conversation", "301"),
            ("conversation", "300"),
        ]
    );
    Ok(())
}

/// The release before conversations had channels wrote no `conversations`
/// table. Its database is refused as it is, and nothing upgrades it.
#[test]
fn the_previous_releases_database_is_refused_and_left_as_it_was() -> Result<()> {
    let directory = tempdir()?;
    let path = directory.path().join("state.sqlite3");
    accept(&Database::create(&path)?, "one", "conversation")?;
    rusqlite::Connection::open(&path)?.execute_batch("DROP TABLE conversations")?;
    let before = std::fs::read(&path)?;
    crate::assert_refused(Database::open(&path), "not the current Carillon format")?;
    crate::assert_refused(
        Database::validate_backup(&path),
        "not the current Carillon format",
    )?;
    assert_eq!(std::fs::read(&path)?, before);
    Ok(())
}
