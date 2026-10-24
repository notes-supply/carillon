use carillon_protocol::model::{JOB_READ_LIMIT, JOB_TURN_LIMIT, Job, JobTurn, Origin};

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
    /// owns it, in the order their turns at work were accepted: at most
    /// [`JOB_READ_LIMIT`] after the job whose turn at work was `after`, and
    /// that turn's ID to read the next page from when more follow. A job has
    /// at most one turn at work, its newest, so every job at work is on one
    /// page. `None` when `after` is not one of the interface's turns.
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
                "SELECT id, origin_turn_id FROM turns
                 WHERE interface_id = ?1 AND state IN ('queued', 'running')
                   AND acceptance_sequence > ?2
                 ORDER BY acceptance_sequence LIMIT ?3",
            )?
            .query_map(
                params![interface_id, after, i64::try_from(JOB_READ_LIMIT + 1)?],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let next = (at_work.len() > JOB_READ_LIMIT).then(|| {
            at_work.truncate(JOB_READ_LIMIT);
            at_work[JOB_READ_LIMIT - 1].0.clone()
        });
        let origins = at_work
            .into_iter()
            .map(|(_, origin)| origin)
            .collect::<Vec<_>>();
        let jobs = jobs(&transaction, interface_id, &origins)?;
        Ok(Some(JobPage { jobs, next }))
    }

    /// The interface's jobs among `named`, by their origin, however they
    /// stand, in the order named. A name that is not the origin of one of
    /// the interface's jobs names nothing.
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

/// The latest turns of the interface's jobs that the JSON array `?2` names
/// by origin, at most `?3` of each, in the order named.
pub(super) const JOB_TURNS: &str = "SELECT o.id, o.runtime_id, o.accepted_at, o.target_id,
        t.id, t.state, t.accepted_at, t.finished_at, t.activity_json, d.state
    FROM json_each(?2) named
    JOIN turns o ON o.id = named.value
    JOIN turns t ON t.id IN (
      SELECT recent.id FROM turns recent WHERE recent.origin_turn_id = o.id
      ORDER BY recent.acceptance_sequence DESC LIMIT ?3)
    LEFT JOIN deliveries d ON d.turn_id = t.id
    WHERE o.interface_id = ?1 AND o.origin_turn_id = o.id
    ORDER BY named.key, t.acceptance_sequence";

/// The interface's jobs that `origins` begin, in that order, each with the
/// runtime that owns it and its [`JOB_TURN_LIMIT`] latest turns: one
/// statement over the `turns_origin` index reads at most one turn more of
/// each job than it lists, however long the job.
fn jobs(
    transaction: &Transaction<'_>,
    interface_id: &str,
    origins: &[String],
) -> Result<Vec<(String, Job)>> {
    let mut statement = transaction.prepare(JOB_TURNS)?;
    let mut rows = statement.query(params![
        interface_id,
        serde_json::to_string(origins)?,
        i64::try_from(JOB_TURN_LIMIT + 1)?,
    ])?;
    let mut jobs: Vec<(String, Job)> = Vec::new();
    while let Some(row) = rows.next()? {
        let origin: String = row.get(0)?;
        if jobs
            .last()
            .is_none_or(|(_, job)| job.origin.turn_id != origin)
        {
            jobs.push((
                row.get(1)?,
                Job {
                    origin: Origin {
                        turn_id: origin,
                        accepted_at: row.get(2)?,
                    },
                    target_id: row.get(3)?,
                    turns: Vec::new(),
                    earlier: false,
                },
            ));
        }
        if let Some((_, job)) = jobs.last_mut() {
            job.turns.push(JobTurn {
                turn_id: row.get(4)?,
                state: state_column(row, 5)?,
                accepted_at: row.get(6)?,
                finished_at: row.get(7)?,
                activity: optional_json_column(row, 8)?,
                delivery: optional_state_column(row, 9)?,
            });
        }
    }
    // The turn read beyond the limit only says that earlier ones exist.
    for (_, job) in &mut jobs {
        if job.turns.len() > JOB_TURN_LIMIT {
            job.turns.remove(0);
            job.earlier = true;
        }
    }
    Ok(jobs)
}
