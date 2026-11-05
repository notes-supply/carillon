//! The files committed turns reference: a message's captured attachments
//! and an answer's kept files, each by capture and ordinal.

use carillon_protocol::model::AnswerFile;
use rusqlite::{Connection, Row, types::Type};

use super::{Database, Result};
use crate::attachments::CapturedFile;

impl Database {
    /// Every committed attachment reference, for validating the store before
    /// archiving or restoring.
    pub(crate) fn captured_files(&self) -> Result<Vec<CapturedFile>> {
        let connection =
            Connection::open_with_flags(&self.path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        captured_files(&connection, None)
    }

    /// The captures committed turns and answers reference.
    pub(crate) fn captures(&self) -> Result<std::collections::BTreeSet<String>> {
        let connection = self.connect()?;
        let mut statement = connection.prepare(
            "SELECT capture_id FROM turn_attachments UNION SELECT capture_id FROM answer_files",
        )?;
        let captures = statement
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        Ok(captures)
    }

    pub(crate) fn capture_referenced(&self, capture: &str) -> Result<bool> {
        Ok(self.connect()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM turn_attachments WHERE capture_id = ?1)
                 OR EXISTS(SELECT 1 FROM answer_files WHERE capture_id = ?1)",
            [capture],
            |row| row.get(0),
        )?)
    }

    /// Bytes kept by every committed capture.
    pub(crate) fn captured_bytes(&self) -> Result<u64> {
        let bytes: i64 = self.connect()?.query_row(
            "SELECT (SELECT COALESCE(SUM(size), 0) FROM turn_attachments)
                  + (SELECT COALESCE(SUM(size), 0) FROM answer_files)",
            [],
            |row| row.get(0),
        )?;
        Ok(u64::try_from(bytes)?)
    }
}

/// A turn's captured files in message order, or every committed reference,
/// answers' files included.
pub(super) fn captured_files(
    connection: &Connection,
    turn_id: Option<&str>,
) -> Result<Vec<CapturedFile>> {
    let mut statement = connection.prepare(
        "SELECT capture_id, ordinal, name, size, content_type, sha256 FROM turn_attachments
         WHERE ?1 IS NULL OR turn_id = ?1
         UNION ALL
         SELECT capture_id, ordinal, name, size, NULL, sha256 FROM answer_files WHERE ?1 IS NULL
         ORDER BY capture_id, ordinal",
    )?;
    let files = statement.query_map([turn_id], |row| {
        Ok(CapturedFile {
            capture: row.get(0)?,
            ordinal: row.get(1)?,
            name: row.get(2)?,
            size: size_column(row, 3)?,
            content_type: row.get(4)?,
            sha256: row.get(5)?,
        })
    })?;
    files.collect::<Result<_, _>>().map_err(Into::into)
}

/// The files kept with a turn's answer, in the order the agent attached them.
pub(super) fn answer_files(connection: &Connection, turn_id: &str) -> Result<Vec<AnswerFile>> {
    let mut statement = connection.prepare(
        "SELECT capture_id, ordinal, name, source, size, sha256 FROM answer_files
         WHERE turn_id = ?1 ORDER BY ordinal",
    )?;
    let files = statement.query_map([turn_id], |row| {
        Ok(AnswerFile {
            path: format!("{}/{}", row.get::<_, String>(0)?, row.get::<_, u32>(1)?),
            name: row.get(2)?,
            source: row.get(3)?,
            size: size_column(row, 4)?,
            sha256: row.get(5)?,
        })
    })?;
    files.collect::<Result<_, _>>().map_err(Into::into)
}

fn size_column(row: &Row<'_>, index: usize) -> rusqlite::Result<u64> {
    u64::try_from(row.get::<_, i64>(index)?).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(index, Type::Integer, Box::new(error))
    })
}
