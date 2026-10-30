use carillon_protocol::model::{JOB_READ_LIMIT, Job, JobTurn, Origin};

use super::{
    Database, OptionalExtension, Result, Transaction, optional_json_column, optional_state_column,
    params, state_column,
};

/// A page of an interface's jobs at work, each with the ID of the runtime
/// that owns it, and where the next page begins when more follow.
#[derive(Debug)]
pub(crate) struct JobPage {
    pub(crate) jobs: Vec<(String, Job)>,
    pub(crate) next: Option<String>,
}

impl Database {
    /// A page of one interface's jobs at work, each with the runtime that
    /// owns it, in the order they were accepted: at most [`JOB_READ_LIMIT`]
    /// after the job `after`, and the ID of the last to read the next page
    /// from when more follow. `None` when `after` is not one of the
    /// interface's turns.
    pub(crate) fn jobs_at_work(
        &self,
        interface_id: &str,
        after: Option<&str>,
    ) -> Result<Option<JobPage>> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction()?;
        let after = match after {
            None => 0,
            Some(turn) => match transaction
                .query_row(
                    "SELECT acceptance_sequence FROM turns WHERE id = ?1 AND interface_id = ?2",
                    params![turn, interface_id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?
            {
                Some(sequence) => sequence,
                None => return Ok(None),
            },
        };
        let mut at_work = transaction
            .prepare(
                "SELECT id FROM turns
                 WHERE interface_id = ?1 AND state IN ('queued', 'running')
                   AND acceptance_sequence > ?2
                 ORDER BY acceptance_sequence LIMIT ?3",
            )?
            .query_map(
                params![interface_id, after, i64::try_from(JOB_READ_LIMIT + 1)?],
                |row| row.get::<_, String>(0),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let next = (at_work.len() > JOB_READ_LIMIT).then(|| {
            at_work.truncate(JOB_READ_LIMIT);
            at_work[JOB_READ_LIMIT - 1].clone()
        });
        let jobs = jobs(&transaction, interface_id, &at_work)?;
        Ok(Some(JobPage { jobs, next }))
    }

    /// The interface's jobs among `named`, however they stand, in the order
    /// named. A name that is not one of the interface's turns names nothing.
    pub(crate) fn named_jobs(
        &self,
        interface_id: &str,
        named: &[String],
    ) -> Result<Vec<(String, Job)>> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction()?;
        jobs(&transaction, interface_id, named)
    }
}

/// The interface's jobs, one turn each, that `turns` name, in that order,
/// each with the runtime that owns it.
fn jobs(
    transaction: &Transaction<'_>,
    interface_id: &str,
    turns: &[String],
) -> Result<Vec<(String, Job)>> {
    let mut statement = transaction.prepare(
        "SELECT t.id, t.runtime_id, t.accepted_at, t.target_id,
            NULLIF(t.feedback_target_id, ''), t.state, t.finished_at, t.activity_json, d.state
         FROM json_each(?2) named
         JOIN turns t ON t.id = named.value
         LEFT JOIN deliveries d ON d.turn_id = t.id
         WHERE t.interface_id = ?1
         ORDER BY named.key",
    )?;
    let jobs = statement
        .query_map(
            params![interface_id, serde_json::to_string(turns)?],
            |row| {
                let turn_id: String = row.get(0)?;
                let accepted_at = row.get(2)?;
                Ok((
                    row.get(1)?,
                    Job {
                        origin: Origin {
                            turn_id: turn_id.clone(),
                            accepted_at,
                        },
                        target_id: row.get(3)?,
                        feedback_target_id: row.get(4)?,
                        turns: vec![JobTurn {
                            turn_id,
                            state: state_column(row, 5)?,
                            accepted_at,
                            finished_at: row.get(6)?,
                            activity: optional_json_column(row, 7)?,
                            delivery: optional_state_column(row, 8)?,
                        }],
                        earlier: false,
                    },
                ))
            },
        )?
        .collect::<Result<_, _>>()?;
    Ok(jobs)
}
