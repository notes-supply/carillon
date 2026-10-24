use carillon_protocol::{
    model::{TurnActivity, TurnState},
    turns::Silence,
};

use super::{
    Database, OptionalExtension, Result, optional_state_column, params, report_json, state_column,
    timestamp, write_transaction,
};

/// How a turn a durable runtime holds ended, as core records it.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Settlement<'a> {
    /// Completed, its output delivered.
    Answered {
        output: &'a str,
        activity: Option<&'a TurnActivity>,
    },
    /// Interrupted, `notice` delivered and `error` kept for the operator.
    Failed {
        error: &'a str,
        notice: &'a str,
        activity: Option<&'a TurnActivity>,
    },
    /// Ended with nothing to send: steered completes, stopped interrupts.
    Silent(Silence),
}

impl Settlement<'_> {
    /// The turn state it ends in and the silence of its delivery, which
    /// together tell one kind of settlement from another.
    const fn ends(self) -> (TurnState, Option<Silence>) {
        match self {
            Self::Answered { .. } => (TurnState::Completed, None),
            Self::Failed { .. } => (TurnState::Interrupted, None),
            Self::Silent(Silence::Steered) => (TurnState::Completed, Some(Silence::Steered)),
            Self::Silent(Silence::Stopped) => (TurnState::Interrupted, Some(Silence::Stopped)),
        }
    }
}

/// What a settlement found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Settled {
    /// It is recorded: now, or earlier as the same kind.
    Recorded,
    /// The turn already ended another way.
    Superseded,
    /// The runtime holds no such turn.
    Unknown,
}

impl Database {
    /// Ends a queued or running turn of `runtime_id` as `settlement` says,
    /// with its delivery, in one transaction. A queued one is settled too:
    /// the runtime may hold a turn whose handing over core never recorded.
    pub(crate) fn settle_held(
        &self,
        runtime_id: &str,
        turn_id: &str,
        settlement: Settlement<'_>,
    ) -> Result<Settled> {
        let mut connection = self.connect()?;
        let transaction = write_transaction(&mut connection)?;
        let found = transaction
            .query_row(
                "SELECT t.state, d.silence FROM turns t LEFT JOIN deliveries d ON d.turn_id = t.id
                 WHERE t.id = ?1 AND t.runtime_id = ?2",
                params![turn_id, runtime_id],
                |row| {
                    Ok((
                        state_column::<TurnState>(row, 0)?,
                        optional_state_column::<Silence>(row, 1)?,
                    ))
                },
            )
            .optional()?;
        let (state, silence) = settlement.ends();
        let settled = match found {
            None => Settled::Unknown,
            Some((TurnState::Queued | TurnState::Running, _)) => {
                let (output, error, notice, activity) = match settlement {
                    Settlement::Answered { output, activity } => {
                        (Some(output), None, output, activity)
                    }
                    Settlement::Failed {
                        error,
                        notice,
                        activity,
                    } => (None, Some(error), notice, activity),
                    Settlement::Silent(Silence::Steered) => (None, None, "", None),
                    Settlement::Silent(Silence::Stopped) => (None, Some("stopped"), "", None),
                };
                let now = timestamp()?;
                transaction.execute(
                    "UPDATE turns SET state = ?2, output = ?3, error = ?4, activity_json = ?5,
                         finished_at = ?6
                     WHERE id = ?1 AND state IN ('queued', 'running')",
                    params![
                        turn_id,
                        state.as_str(),
                        output,
                        error,
                        report_json(activity)?,
                        now
                    ],
                )?;
                transaction.execute(
                    "INSERT INTO deliveries(turn_id, interface_id, target_id, feedback_target_id,
                         content, silence, state, updated_at)
                     SELECT id, interface_id, target_id, feedback_target_id, ?2, ?3, 'pending', ?4
                     FROM turns WHERE id = ?1",
                    params![turn_id, notice, silence.map(Silence::as_str), now],
                )?;
                Settled::Recorded
            }
            Some(ended) if ended == (state, silence) => Settled::Recorded,
            Some(_) => Settled::Superseded,
        };
        transaction.commit()?;
        Ok(settled)
    }
}
