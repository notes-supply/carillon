use super::*;

#[test]
fn a_read_connection_refuses_to_write() -> Result<()> {
    let directory = tempdir()?;
    let database = Database::create(&directory.path().join("state.sqlite3"))?;
    accept(&database, "one", "conversation")?;
    let reader = database.read()?;
    crate::assert_refused(
        reader.execute("UPDATE turns SET state = 'completed'", []),
        "attempt to write a readonly database",
    )?;
    crate::assert_refused(
        reader.execute_batch("CREATE TABLE more (id INTEGER)"),
        "attempt to write a readonly database",
    )?;
    assert_eq!(database.turn_counts()?.pending, 1);
    Ok(())
}

#[test]
fn a_read_sees_what_is_committed_while_a_write_is_open() -> Result<()> {
    let directory = tempdir()?;
    let database = Database::create(&directory.path().join("state.sqlite3"))?;
    accept(&database, "one", "conversation")?;
    database.write(|transaction| {
        transaction.execute("UPDATE turns SET state = 'completed'", [])?;
        let written: u32 = transaction.query_row(
            "SELECT COUNT(*) FROM turns WHERE state = 'completed'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(written, 1);
        assert_eq!(database.turn_counts()?.pending, 1);
        Ok(())
    })?;
    assert_eq!(database.turn_counts()?.completed, 1);
    Ok(())
}

#[test]
fn a_failed_write_changes_nothing() -> Result<()> {
    let directory = tempdir()?;
    let database = Database::create(&directory.path().join("state.sqlite3"))?;
    accept(&database, "one", "conversation")?;
    let failed: Result<()> = database.write(|transaction| {
        transaction.execute("UPDATE turns SET state = 'completed'", [])?;
        bail!("the work failed");
    });
    crate::assert_refused(failed, "the work failed")?;
    assert_eq!(database.turn_counts()?.pending, 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reader_woken_by_a_change_finds_storage_free() -> Result<()> {
    let directory = tempdir()?;
    let database = Database::create(&directory.path().join("state.sqlite3"))?;
    let mut changes = database.changes();
    let storage = database.clone();
    let reader = tokio::spawn(async move {
        changes.changed().await?;
        // With no busy timeout, a writer that is still closing refuses the
        // read where it would otherwise make the reader wait.
        let connection = storage.read()?;
        connection.busy_timeout(std::time::Duration::ZERO)?;
        let turns: u32 =
            connection.query_row("SELECT COUNT(*) FROM turns", [], |row| row.get(0))?;
        anyhow::Ok(turns)
    });
    accept(&database, "one", "conversation")?;
    assert_eq!(reader.await??, 1);
    Ok(())
}
