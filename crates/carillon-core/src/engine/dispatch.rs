//! Handing queued messages to the runtimes that hold them.

use anyhow::{Context as _, Result};
use carillon_protocol::model::RuntimeTurnRequest;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::{ALTERED_ATTACHMENT, Core, UNREADABLE_ATTACHMENT, WorkerStep, interrupted};
use crate::{
    attachments::Unusable,
    db::{QueuedTurn, Settlement},
};

/// Why a runtime does not hold a turn core handed it.
#[derive(Debug, thiserror::Error)]
pub enum InboxFailure {
    /// It may not have arrived, or the runtime asked for it later: it stays
    /// queued and is handed over again, in order. The runtime takes a turn
    /// it already holds as the same one.
    #[error("runtime provider did not take a turn: {0:#}")]
    Later(anyhow::Error),
    /// The runtime's inbox does not answer as core expects, such as a
    /// credential it refuses or a route it lacks: an installation fault for
    /// the operator, not this turn's. It stays queued as for `Later`.
    #[error("runtime provider's inbox is not installed as core expects: {0:#}")]
    Misinstalled(anyhow::Error),
    /// The runtime refused it, with an explanation safe to display when it
    /// gave one: the turn is interrupted.
    #[error("runtime provider refused the turn with {status}")]
    Refused {
        status: u16,
        explanation: Option<String>,
    },
}

/// Why a runtime's dispatcher hands nothing over for now: its queued
/// turns wait, in order, until it tries again.
#[derive(Debug, thiserror::Error)]
pub enum Stalled {
    /// The runtime did not take the turn, and did not refuse it.
    #[error(transparent)]
    Inbox(InboxFailure),
    /// Core's registry or storage failed.
    #[error("{0:#}")]
    Core(#[from] anyhow::Error),
}

impl Core {
    /// Hands a runtime its oldest queued message through `submit`,
    /// once its files are still the ones accepted. The turn stays queued
    /// until the runtime takes it, and then runs there until the runtime
    /// settles it; nothing here waits for its work. Once `stopping` is
    /// cancelled nothing more is handed over.
    /// # Errors
    /// Returns why the turn stays queued: the runtime did not take it, or
    /// registry or storage errors.
    pub async fn dispatch_next(
        &self,
        runtime_id: &str,
        stopping: &CancellationToken,
        submit: impl AsyncFnOnce(RuntimeTurnRequest) -> Result<(), InboxFailure>,
    ) -> Result<WorkerStep, Stalled> {
        if !self
            .runtime(runtime_id)
            .map_err(anyhow::Error::from)?
            .enabled
        {
            return Ok(WorkerStep::Backoff);
        }
        let Some(turn) = self.database.next_dispatch(runtime_id)? else {
            return Ok(WorkerStep::Idle);
        };
        if stopping.is_cancelled() {
            return Ok(WorkerStep::Idle);
        }
        if let Err(unusable) = self.check_attachments(&turn).await {
            let (error, explanation) = match &unusable {
                Unusable::Altered(message) => (message.clone(), ALTERED_ATTACHMENT),
                Unusable::Unreadable(error) => (format!("{error:#}"), UNREADABLE_ATTACHMENT),
            };
            warn!(turn_id = %turn.turn_id, %error, "captured attachment is not usable; turn not handed over");
            self.refuse(&turn, error, Some(explanation)).await?;
            return Ok(WorkerStep::Advanced);
        }
        match submit(self.runtime_request(&turn)).await {
            Ok(()) => {
                let held = turn.clone();
                self.blocking(move |core| core.database.dispatched(&held))
                    .await
                    .with_context(|| format!("record that turn {} is held", turn.turn_id))?;
                Ok(WorkerStep::Advanced)
            }
            Err(failure @ (InboxFailure::Later(_) | InboxFailure::Misinstalled(_))) => {
                Err(Stalled::Inbox(failure))
            }
            Err(InboxFailure::Refused {
                status,
                explanation,
            }) => {
                let error = explanation.as_ref().map_or_else(
                    || {
                        format!(
                            "runtime provider refused the turn with {status} and no explanation"
                        )
                    },
                    |explanation| format!("runtime refused the turn: {explanation}"),
                );
                warn!(turn_id = %turn.turn_id, %error, "turn did not complete");
                self.refuse(&turn, error, explanation.as_deref()).await?;
                Ok(WorkerStep::Advanced)
            }
        }
    }

    /// Interrupts a queued turn its runtime never took, telling its
    /// conversation why.
    async fn refuse(
        &self,
        turn: &QueuedTurn,
        error: String,
        explanation: Option<&str>,
    ) -> Result<()> {
        let (runtime, turn_id) = (turn.runtime_id.clone(), turn.turn_id.clone());
        let notice = interrupted(explanation);
        self.blocking(move |core| {
            let failed = Settlement::Failed {
                error: &error,
                notice: &notice,
                activity: None,
            };
            core.database.settle_held(&runtime, &turn_id, failed)
        })
        .await
        .with_context(|| format!("record refused turn {}", turn.turn_id))?;
        Ok(())
    }
}
