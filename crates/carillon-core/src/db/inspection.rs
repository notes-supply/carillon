use carillon_protocol::model::{NATIVE_CONTEXT, TurnInspection};

use super::{
    CapturedFile, Database, OptionalExtension, Result, Value, answer_files, captured_files,
    json_column, optional_state_column, state_column,
};

impl Database {
    pub(crate) fn inspect_turn(&self, turn_id: &str) -> Result<Option<TurnInspection>> {
        let connection = self.connect()?;
        let inspection = connection
            .query_row(
                "SELECT t.id, t.event_id, t.audience_id, t.user_id, t.input, t.state, t.output,
                        t.provider_snapshot_json, t.resource_snapshot_id, d.state, d.native_ref, t.error,
                        t.native_json
                 FROM turns t LEFT JOIN deliveries d ON d.turn_id = t.id
                 WHERE t.id = ?1",
                [turn_id],
                |row| {
                    Ok(TurnInspection {
                        turn_id: row.get(0)?,
                        event_id: row.get(1)?,
                        audience_id: row.get(2)?,
                        user_id: row.get(3)?,
                        input: row.get(4)?,
                        context: json_column::<Value>(row, 12)?[NATIVE_CONTEXT]
                            .as_str()
                            .map(str::to_owned),
                        state: state_column(row, 5)?,
                        output: row.get(6)?,
                        providers: json_column(row, 7)?,
                        resource_snapshot_id: row.get(8)?,
                        delivery_state: optional_state_column(row, 9)?,
                        delivery_native_ref: row.get(10)?,
                        error: row.get(11)?,
                        attachments: Vec::new(),
                        answer_files: Vec::new(),
                    })
                },
            )
            .optional()?;
        let Some(mut inspection) = inspection else {
            return Ok(None);
        };
        inspection.attachments = captured_files(&connection, Some(turn_id))?
            .iter()
            .map(CapturedFile::wire)
            .collect();
        inspection.answer_files = answer_files(&connection, turn_id)?;
        Ok(Some(inspection))
    }
}
