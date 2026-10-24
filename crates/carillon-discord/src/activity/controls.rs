//! The control reactions on a job's message while its turn waits in the
//! runtime that holds it: offered while the runtime's snapshot offers them,
//! withdrawn once it does not. The reactions themselves belong to
//! [`Reactions`], which also withdraws them before the message's outcome.

use carillon_protocol::{
    model::{ActivityPhase, JobTurn, TurnState},
    turns::ControlAction,
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::Known;
use crate::delivery::Reactions;

/// The turn runs in its runtime, waiting behind other work there.
pub(super) fn waits(turn: &JobTurn) -> bool {
    turn.activity
        .as_ref()
        .is_some_and(|activity| activity.phase == ActivityPhase::Queued)
}

/// The controls the job's message offers now: its runtime's, while the
/// job's newest turn runs there and waits.
fn wanted(known: &Known) -> Vec<ControlAction> {
    known
        .job()
        .and_then(|job| job.at_work())
        .filter(|turn| turn.state == TurnState::Running && waits(turn))
        .and_then(|turn| turn.activity.as_ref())
        .map(|activity| activity.controls.clone())
        .unwrap_or_default()
}

/// Keeps a job's message offering what its readings say, until the job is
/// over or no longer followed, then withdraws what it offered. A stopping
/// provider leaves its reactions as they are.
pub(super) fn offer(
    reactions: &Reactions,
    view: &watch::Receiver<Known>,
    stopping: &CancellationToken,
) -> impl Future<Output = ()> + Send + 'static {
    let (reactions, mut view, stopping) = (reactions.clone(), view.clone(), stopping.clone());
    async move {
        let (mut message, mut offered) = (None, Vec::new());
        loop {
            let (wanted, over) = {
                let known = view.borrow_and_update();
                if message.is_none() {
                    message = known.job().and_then(|job| job.feedback_target_id.clone());
                }
                (wanted(&known), known.is_over())
            };
            if let Some(message) = &message
                && wanted != offered
            {
                reactions.offer_controls(message, &wanted).await;
                offered = wanted;
            }
            if over {
                break;
            }
            match stopping.run_until_cancelled(view.changed()).await {
                None => return,
                Some(Err(_)) => break,
                Some(Ok(())) => {}
            }
        }
        if let Some(message) = &message
            && !offered.is_empty()
        {
            reactions.offer_controls(message, &[]).await;
        }
    }
}
