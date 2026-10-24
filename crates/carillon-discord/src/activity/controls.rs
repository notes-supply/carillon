//! The control reactions on a job's message while its turn waits in the
//! runtime that holds it: offered while the runtime's snapshot offers them,
//! withdrawn once it does not. [`Reactions`] records which control emoji a
//! message may show, and removes them before the message's outcome.

use std::collections::BTreeSet;

use carillon_protocol::{
    model::{ActivityPhase, JobTurn, TurnState},
    turns::ControlAction,
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::Known;
use crate::delivery::{Reactions, control_emoji};

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

/// Keeps a job's message offering what its readings say, as the settings
/// serving name the controls, until the job is over or no longer followed,
/// then withdraws every control it may show. The message's controls are
/// taken up only to offer one: until then, whatever it may show is left to
/// its outcome to withdraw. A stopping provider leaves its reactions as they
/// are.
pub(super) fn offer(
    reactions: &Reactions,
    view: &watch::Receiver<Known>,
    stopping: &CancellationToken,
) -> impl Future<Output = ()> + Send + 'static {
    let (reactions, mut view, stopping) = (reactions.clone(), view.clone(), stopping.clone());
    let mut settings = reactions.settings().watch();
    async move {
        let mut controls = None;
        // The control emoji Discord last took in full.
        let mut shown = None;
        loop {
            let config = settings.borrow_and_update().clone();
            let wanted = {
                let known = view.borrow_and_update();
                if known.is_over() {
                    break;
                }
                let wanted = control_emoji(&config, &wanted(&known));
                if controls.is_none() && !wanted.is_empty() {
                    controls = known
                        .job()
                        .and_then(|job| job.feedback_target_id.as_deref())
                        .filter(|message| !message.is_empty())
                        .map(|message| reactions.controls(message, &config));
                }
                wanted
            };
            if let Some(controls) = &controls
                && shown.as_ref() != Some(&wanted)
            {
                shown = controls.reconcile(&wanted).await.then_some(wanted);
            }
            let changed = async {
                tokio::select! {
                    read = view.changed() => read.is_ok(),
                    _ = settings.changed() => true,
                }
            };
            match stopping.run_until_cancelled(changed).await {
                None => return,
                Some(false) => break,
                Some(true) => {}
            }
        }
        if let Some(controls) = &controls {
            controls.reconcile(&BTreeSet::new()).await;
        }
    }
}
