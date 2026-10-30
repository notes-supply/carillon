//! Event identities core gives the turns runtimes open themselves, which no
//! interface may submit.

use tempfile::tempdir;

use super::{accept, installation};
use crate::application::http::*;

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
