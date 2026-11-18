//! An open observation: the snapshots core streams of one view.

use std::{num::NonZeroUsize, time::Duration};

use bytes::Bytes;
use carillon_protocol::observation::{KEEP_ALIVE, SNAPSHOT_EVENT, SNAPSHOT_LIMIT, Snapshot};
use reqwest::Response;
use sse_core::{SseDecoder, SseEvent};
use tokio::time::Instant;

use crate::InterfaceError;

/// How long core has to accept an observation and send its first snapshot,
/// from the moment it is asked.
pub(crate) const OPEN_TIMEOUT: Duration = Duration::from_secs(10);

/// A stream silent this long has stopped: three keep-alives were missed.
const IDLE_TIMEOUT: Duration = KEEP_ALIVE.saturating_mul(3);

const DECODER_LIMIT: NonZeroUsize = NonZeroUsize::new(SNAPSHOT_LIMIT).expect("a limit above zero");

/// The snapshots core streams of one view, from
/// [`CoreClient::observe`](crate::CoreClient::observe). Each is everything
/// the view shows: replace what you had with it and merge nothing.
///
/// An observation does not repair itself. Once [`Self::next`] has returned
/// `Ok(None)` or an error it is over, and a caller that still wants the
/// view opens another, whose first snapshot is complete.
pub struct Observation {
    /// `None` once the observation is over.
    response: Option<Response>,
    events: SseDecoder,
    /// What arrived and is not decoded yet.
    unread: Bytes,
    /// When waiting for core fails: by when the first snapshot is due,
    /// then how long the stream may stay silent.
    quiet_until: Instant,
    started: bool,
}

impl Observation {
    /// `response` is core's answer to an observation whose first snapshot
    /// is due by `first_due`.
    pub(crate) fn new(response: Response, first_due: Instant) -> Self {
        Self {
            response: Some(response),
            events: SseDecoder::with_limit(DECODER_LIMIT),
            unread: Bytes::new(),
            quiet_until: first_due,
            started: false,
        }
    }

    /// The next snapshot, or `None` once the observation is over: core
    /// ended the stream, as it does when it stops, or an earlier call
    /// returned an error. Waiting may be abandoned and taken up again:
    /// nothing received is lost.
    ///
    /// # Errors
    ///
    /// Returns a transient failure when the connection is lost, when the
    /// first snapshot does not come in time, or when the stream stays
    /// silent for three keep-alives. Returns a failure that is not
    /// transient when core sends what is not a snapshot, or an event
    /// larger than [`SNAPSHOT_LIMIT`]. An error is returned once: every
    /// call after it returns `Ok(None)`.
    pub async fn next(&mut self) -> Result<Option<Snapshot>, InterfaceError> {
        let outcome = self.read().await;
        if !matches!(outcome, Ok(Some(_))) {
            self.response = None;
        }
        outcome
    }

    async fn read(&mut self) -> Result<Option<Snapshot>, InterfaceError> {
        let Some(response) = &mut self.response else {
            return Ok(None);
        };
        loop {
            match self.events.next(&mut self.unread) {
                Some(Ok(SseEvent::Message(event))) if event.event == SNAPSHOT_EVENT => {
                    let snapshot = serde_json::from_str(&event.data).map_err(|error| {
                        InterfaceError::protocol(format!("decode a snapshot from core: {error}"))
                    })?;
                    self.started = true;
                    self.quiet_until = Instant::now() + IDLE_TIMEOUT;
                    return Ok(Some(snapshot));
                }
                Some(Ok(SseEvent::Message(event))) => {
                    return Err(InterfaceError::protocol(format!(
                        "core sent an event named {:?} on an observation",
                        event.event
                    )));
                }
                Some(Err(error)) => {
                    return Err(InterfaceError::protocol(format!(
                        "core sent an event over {SNAPSHOT_LIMIT} bytes on an observation: {error}"
                    )));
                }
                Some(Ok(SseEvent::Retry(_))) => continue,
                None => {}
            }
            let chunk = tokio::time::timeout_at(self.quiet_until, response.chunk())
                .await
                .map_err(|_| {
                    InterfaceError::transient(if self.started {
                        format!("core sent nothing on an observation for {IDLE_TIMEOUT:?}")
                    } else {
                        format!("core sent no first snapshot within {OPEN_TIMEOUT:?}")
                    })
                })?
                .map_err(|error| {
                    InterfaceError::transient(format!(
                        "read an observation: {:#}",
                        anyhow::Error::new(error)
                    ))
                })?;
            let Some(chunk) = chunk else {
                return Ok(None);
            };
            self.unread = chunk;
            if self.started {
                self.quiet_until = Instant::now() + IDLE_TIMEOUT;
            }
        }
    }
}

#[cfg(test)]
mod tests;
