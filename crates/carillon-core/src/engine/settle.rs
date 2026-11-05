//! What runtimes tell core of the turns they hold, and the turns an
//! interface may control there.

use anyhow::Result;
use carillon_protocol::{
    model::{OpenedTurn, TurnState},
    turns::{RunningTurn, Silence, TurnOpening, TurnOutcome},
};

use super::{Core, CoreError, RuntimeProvider, interrupted, kept_report};
use crate::db::{Opened, RUNTIME_EVENT_PREFIX, Settled, Settlement};

impl Core {
    /// Settles a turn `runtime_id` holds as it says the turn ended, with the
    /// delivery that tells its conversation, once. An answer's files are
    /// copied first, unless the turn already ended; blocking while they are.
    /// # Errors
    /// Returns unavailable while core stops, or storage errors.
    pub fn settle(
        &self,
        runtime_id: &str,
        turn_id: &str,
        outcome: TurnOutcome,
    ) -> Result<Settled, CoreError> {
        let settle = |settlement| self.database.settle_held(runtime_id, turn_id, settlement);
        Ok(match outcome {
            TurnOutcome::Answered {
                output,
                activity,
                files,
            } => {
                let activity = kept_report(turn_id, activity);
                let kept = if files.is_empty() {
                    None
                } else {
                    Some(self.keep_answer_files(runtime_id, turn_id, &files)?)
                };
                let settled = settle(Settlement::Answered {
                    output: &output,
                    content: &kept
                        .as_ref()
                        .map_or_else(|| output.clone(), |kept| kept.content(&output)),
                    activity: activity.as_ref(),
                    files: kept.as_ref().map_or(&[], |kept| &kept.files),
                });
                if let Some(kept) = kept {
                    kept.settled(self);
                }
                settled?
            }
            TurnOutcome::Failed {
                explanation,
                activity,
            } => {
                let explanation = explanation.filter(|explanation| !explanation.trim().is_empty());
                let error = explanation.as_ref().map_or_else(
                    || "runtime provider failed the turn with no explanation".to_owned(),
                    |explanation| format!("runtime reported failure: {explanation}"),
                );
                let activity = kept_report(turn_id, activity);
                settle(Settlement::Failed {
                    error: &error,
                    notice: &interrupted(explanation.as_deref()),
                    activity: activity.as_ref(),
                })?
            }
            TurnOutcome::Steered {} => settle(Settlement::Silent(Silence::Steered))?,
            TurnOutcome::Stopped {} => settle(Settlement::Silent(Silence::Stopped))?,
        })
    }

    /// Returns the receipt of the turn the same key opened in the same
    /// conversation, or else opens a running turn `runtime_id` will settle,
    /// in an existing conversation of its own whose agent runs there now;
    /// either with the conversation's channel.
    /// # Errors
    /// Returns a conversation that is not the runtime's, or a key another
    /// conversation used, as a conflict, or storage errors.
    pub(crate) fn open_turn(
        &self,
        runtime_id: &str,
        opening: &TurnOpening,
    ) -> Result<OpenedTurn, CoreError> {
        let agent_runtime = self
            .agent(&opening.agent_id)
            .ok()
            .map(|agent| agent.runtime.as_str());
        match self
            .database
            .open_turn(runtime_id, agent_runtime, opening)?
        {
            Opened::Receipt(opened) => Ok(opened),
            Opened::Elsewhere => Err(CoreError::Conflict(format!(
                "the key {} opened a turn in another conversation",
                opening.key
            ))),
            Opened::Moved => Err(CoreError::Conflict(format!(
                "{} does not run on {runtime_id}",
                opening.agent_id
            ))),
            Opened::NoConversation => Err(CoreError::Conflict(
                "a runtime opens turns only in an existing conversation".to_owned(),
            )),
            Opened::OwnedBy(owner) => Err(CoreError::Conflict(format!(
                "the conversation runs on {owner}, not {runtime_id}"
            ))),
        }
    }

    /// The turns core records running on `runtime_id`, oldest first: each
    /// with the request core hands the runtime, and the key of a turn it
    /// opened.
    /// # Errors
    /// Returns storage errors.
    pub(crate) fn running_turns(&self, runtime_id: &str) -> Result<Vec<RunningTurn>> {
        let opened = format!("{RUNTIME_EVENT_PREFIX}{runtime_id}:");
        Ok(self
            .database
            .running_turns(runtime_id)?
            .into_iter()
            .map(|(turn, event_id)| RunningTurn {
                key: event_id.strip_prefix(&opened).map(str::to_owned),
                request: self.runtime_request(&turn),
            })
            .collect())
    }

    /// The runtime holding the running turn of an interface's event, and the
    /// turn's ID: `None` when the interface submitted no such event.
    /// # Errors
    /// Returns a turn that is not running on an enabled runtime as a
    /// conflict, or storage errors.
    pub(crate) fn controlled(
        &self,
        interface_id: &str,
        event_id: &str,
    ) -> Result<Option<(RuntimeProvider, String)>, CoreError> {
        let Some((turn_id, state, runtime_id)) =
            self.database.event_turn(interface_id, event_id)?
        else {
            return Ok(None);
        };
        if state != TurnState::Running {
            return Err(CoreError::Conflict(
                "that message's turn is not running".to_owned(),
            ));
        }
        Ok(Some((self.enabled_runtime(&runtime_id)?, turn_id)))
    }
}
