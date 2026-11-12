use crate::{CoreClient, InterfaceError};
use carillon_protocol::model::{EventReceipt, EventStatus, ExternalEvent};
use std::time::Duration;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::warn;

const RETRY: Duration = Duration::from_millis(250);
const ASK_AGAIN: Duration = Duration::from_secs(2);

/// What core settles for one of this provider's events: its receipt once
/// accepted, `None` when core does not hold it. A capture still running is
/// waited for, and an unreachable core is waited out; a refusal to answer
/// is returned.
///
/// # Errors
///
/// Returns core's refusal to say what became of the event.
pub async fn settled(
    core: &CoreClient,
    event_id: &str,
) -> Result<Option<EventReceipt>, InterfaceError> {
    loop {
        match core.event_status(event_id).await {
            Ok(EventStatus::Accepted(receipt)) => return Ok(Some(receipt)),
            Ok(EventStatus::Unknown) => return Ok(None),
            Ok(EventStatus::Capturing) => {}
            Err(failure) if failure.is_transient() => {
                warn!(error = %failure.message, %event_id, "waiting to ask core about an event");
            }
            Err(failure) => return Err(failure),
        }
        sleep(ASK_AGAIN).await;
    }
}

/// Submit `event` until core accepts or refuses it. No attempt starts once
/// the provider is `stopping`, whether or not this call made one: `None`
/// then, as core has not answered. An attempt under way runs to its end,
/// within the client's own bound.
pub async fn submit_event_with_retry(
    core: &CoreClient,
    stopping: &CancellationToken,
    event: &ExternalEvent,
) -> Option<Result<EventReceipt, InterfaceError>> {
    loop {
        if stopping.is_cancelled() {
            return None;
        }
        match core.submit_event(event).await {
            Err(failure) if failure.is_transient() => {
                warn!(error = %failure.message, event_id = %event.event_id, "waiting to submit an event");
                stopping.run_until_cancelled(sleep(RETRY)).await;
            }
            outcome => return Some(outcome),
        }
    }
}
