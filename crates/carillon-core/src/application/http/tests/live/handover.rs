//! Turns queued one after another for a dispatcher its supervisor started,
//! each handed to the runtime.

use super::*;

#[tokio::test]
async fn turns_queued_for_a_running_dispatcher_are_handed_over() -> Result<()> {
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
    for (settled, input) in (1..).zip(["first", "second", "third"]) {
        ok(submit(&state, input, "conversation").await)?;
        runtime.drain(&state, (0, 0, settled)).await?;
    }
    assert_eq!(runtime.given(), ["first", "second", "third"]);
    stopping.cancel();
    supervisor.await?;
    Ok(())
}
