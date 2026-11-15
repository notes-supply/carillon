//! Core's change signal, as its dispatchers hear it.

use super::{
    live::{HeldRuntime, ok, serving, workers_are},
    *,
};
use crate::InboxFailure;

#[tokio::test]
async fn a_turn_queued_while_its_dispatcher_waits_is_handed_over() -> Result<()> {
    let directory = tempdir()?;
    let runtime = HeldRuntime::start().await?;
    let (_fixture, state) = serving(directory.path(), &runtime)?;
    let stopping = CancellationToken::new();
    let supervisor = tokio::spawn(workers::supervise(
        state.clone(),
        state.workers.clone(),
        stopping.clone(),
    ));
    workers_are(&state, Dispatcher::Dispatching).await?;
    for (settled, input) in (1..).zip(["first", "second"]) {
        // Long enough for the dispatcher to look, find nothing and wait, so
        // that only core's signal brings it back.
        tokio::time::sleep(Duration::from_millis(50)).await;
        ok(live::submit(&state, input, "conversation").await)?;
        runtime.drain(&state, (0, 0, settled)).await?;
    }
    assert_eq!(runtime.given(), ["first", "second"]);
    stopping.cancel();
    supervisor.await?;
    Ok(())
}

#[tokio::test]
async fn dispatchers_hear_of_queued_turns_and_hand_over_nothing_once_stopping() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let mut changes = state.core.changes();
    let mut heard = || {
        let seen = changes.has_changed().ok();
        changes.mark_unchanged();
        seen
    };
    let queued = accept(&state, "one", "conversation").await?;
    assert_eq!(heard(), Some(true));
    // Stopping, it hands nothing over: the turn stays queued.
    let stopping = CancellationToken::new();
    stopping.cancel();
    let step = state
        .core
        .dispatch_next("test-runtime", &stopping, async |_| {
            unreachable!("a turn was handed over once stopping")
        })
        .await?;
    assert_eq!(step, WorkerStep::Idle);
    assert_eq!(
        state
            .core
            .inspect_turn(&queued.turn_id)?
            .context("missing turn")?
            .state,
        TurnState::Queued
    );
    // A look that hands nothing over changes nothing, so no dispatcher
    // wakes itself.
    assert_eq!(heard(), Some(false));
    hand_next(&state.core, "test-runtime").await?;
    assert_eq!(heard(), Some(true));
    assert!(hand_next(&state.core, "test-runtime").await?.is_none());
    assert_eq!(heard(), Some(false));
    // Runtimes changing do, so a runtime enabled again is handed its turns.
    state.core.set_runtimes(runtime_facts(&state.config))?;
    assert_eq!(heard(), Some(true));
    Ok(())
}

#[tokio::test]
async fn a_turn_its_runtime_refuses_is_heard_of() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let refused = accept(&state, "refused", "conversation").await?;
    let changes = state.core.changes();
    let refuse = async |_| {
        Err(InboxFailure::Refused {
            status: 422,
            explanation: None,
        })
    };
    state
        .core
        .dispatch_next("test-runtime", &CancellationToken::new(), refuse)
        .await?;
    assert!(changes.has_changed()?);
    let turn = state.core.inspect_turn(&refused.turn_id)?.context("turn")?;
    assert_eq!(turn.state, TurnState::Interrupted);
    Ok(())
}
