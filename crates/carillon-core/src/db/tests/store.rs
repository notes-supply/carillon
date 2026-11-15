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
