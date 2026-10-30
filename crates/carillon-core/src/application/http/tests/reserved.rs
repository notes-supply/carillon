//! Event identities core gives the turns it and runtimes record themselves,
//! which no interface may submit.

use tempfile::tempdir;

use super::{accept, completed, installation, run_next_turn};
use crate::application::http::*;

/// An interface may name its events anything but the identities core gives
/// its own follow-up observations; otherwise one sent after a parent's
/// receipt would make the parent's completion fail, losing its answer.
#[tokio::test]
async fn an_interface_cannot_take_a_follow_up_identity_from_a_parent_turn() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let parent = accept(&state, "parent", "conversation").await?;
    let internal = format!("follow-up:{}", parent.turn_id);
    let claimed = accept(&state, &internal, "conversation").await;
    let step = run_next_turn(&state, "test-runtime", async |_, _, request| {
        Ok(RuntimeTurnResponse {
            follow_up: Some(json!({"after": 1})),
            ..completed(request)
        })
    })
    .await;
    // The parent completed with its answer, and its observation is queued.
    step?;
    let inspection = state
        .core
        .inspect_turn(&parent.turn_id)?
        .context("missing parent")?;
    assert_eq!(inspection.state, TurnState::Completed);
    assert_eq!(inspection.output.as_deref(), Some("reply to parent"));
    let observation = state
        .core
        .database
        .event_receipt("test-interface", &internal)?
        .context("missing observation")?;
    assert_eq!(observation.state, TurnState::Queued);
    assert_ne!(observation.turn_id, parent.turn_id);
    // The interface's event was refused at admission, before anything was recorded.
    let refusal = claimed
        .err()
        .context("an interface took a follow-up identity")?;
    assert!(refusal.to_string().contains("reserved"), "{refusal}");
    Ok(())
}

#[tokio::test]
async fn an_interface_cannot_submit_a_runtimes_event() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let refusal = accept(&state, "runtime:test-runtime:nightly", "conversation")
        .await
        .err()
        .context("accepted a runtime's event identity")?;
    assert_eq!(
        refusal.to_string(),
        "event identities starting runtime: are reserved for core"
    );
    assert!(state.core.recent_turns(None, 10)?.is_empty());
    Ok(())
}
