//! `GET /v1/observe`: what the calling interface's view shows, as a stream
//! of complete snapshots. The first is sent at once, and another whenever
//! what the view shows has changed. A reader replaces what it had with each
//! one. There is no event ID and no replay: a reader that loses the stream
//! opens another and starts from its first snapshot.
//!
//! The stream is the response body and nothing else. No task and no queue
//! stands behind it, so a snapshot is composed only when the reader takes
//! the one before: a slow reader gets the latest state when it reads again,
//! never a backlog, and a reader that goes away leaves nothing behind.

use std::{convert::Infallible, future, time::Duration};

use anyhow::Result;
use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode, Uri},
    response::sse::{Event, KeepAlive, Sse},
};
use carillon_protocol::observation::{
    KEEP_ALIVE, Opened, Progress, SNAPSHOT_EVENT, Snapshot, View,
};
use futures_util::{Stream, StreamExt as _, stream};
use tokio::{sync::watch, time::Instant};
use tracing::warn;

use super::{ApiError, AppState, activity::activities, interface_provider};
use crate::db::{Bounded, HeldTurn, TooLarge};

/// How long after a snapshot that shows a running turn core composes the
/// next one, which asks the turn's runtime again.
const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// The least time between two compositions on one stream. However many
/// changes are committed, a stream costs two compositions a second, and a
/// runtime two calls a second for each running turn the stream shows.
const COMPOSITION_GAP: Duration = Duration::from_millis(500);

/// One reader's stream of a view.
struct Observation {
    state: AppState,
    interface_id: String,
    view: View,
    changes: watch::Receiver<()>,
    sent: String,
    running: bool,
    composed: Instant,
}

/// Authenticates the caller before it reads the view, so a caller that is
/// refused learns nothing of what core would accept. The first snapshot is
/// composed before the answer begins, so a view core cannot show is refused
/// with a status: 400 for a view that is not one, 413 for one whose
/// snapshot is over [`SNAPSHOT_LIMIT`](carillon_protocol::observation::SNAPSHOT_LIMIT).
pub(super) async fn observe(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let interface_id = interface_provider(&headers, &state.config())?.id.clone();
    let Query(view) = Query::<View>::try_from_uri(&uri)
        .map_err(|rejection| ApiError::new(StatusCode::BAD_REQUEST, rejection.body_text()))?;
    let mut observation = Observation {
        // Subscribed before the first composition: a change committed
        // while it is composed is then still heard.
        changes: state.core.changes(),
        state,
        interface_id,
        view,
        sent: String::new(),
        running: false,
        composed: Instant::now(),
    };
    observation.sent = observation.compose().await.map_err(|error| {
        if error.is::<TooLarge>() {
            ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, error.to_string())
        } else {
            error.into()
        }
    })?;
    let first = future::ready(Ok(event(&observation.sent)));
    let later = stream::unfold(observation, Observation::next);
    Ok(
        Sse::new(stream::once(first).chain(later))
            .keep_alive(KeepAlive::new().interval(KEEP_ALIVE)),
    )
}

fn event(snapshot: &str) -> Event {
    Event::default().event(SNAPSHOT_EVENT).data(snapshot)
}

impl Observation {
    /// The view's snapshot as it is now, serialized.
    async fn compose(&mut self) -> Result<String> {
        let config = self.state.config();
        let sample = async |held: &[HeldTurn]| {
            let asked = held
                .iter()
                .map(|turn| (turn.runtime_id.as_str(), turn.turn_id.as_str()));
            activities(&self.state.client, &config, &self.interface_id, asked).await
        };
        let snapshot = self
            .state
            .core
            .snapshot(&self.interface_id, &self.view, sample)
            .await?;
        let mut serialized = Bounded::new(Vec::new());
        serialized.add(&snapshot)?;
        self.composed = Instant::now();
        self.running = shows_a_running_turn(&snapshot);
        Ok(String::from_utf8(serialized.into_inner())?)
    }

    /// The next snapshot to send, once what the view shows differs from
    /// what was last sent. `None` ends the stream: core is stopping, or a
    /// snapshot could not be composed. Nothing here waits to try again, so
    /// a reader of a stream that ended opens another at once.
    async fn next(mut self) -> Option<(Result<Event, Infallible>, Self)> {
        let stopping = self.state.stopping.clone();
        let changed = stopping.run_until_cancelled(self.changed()).await??;
        Some((Ok(changed), self))
    }

    async fn changed(&mut self) -> Option<Event> {
        loop {
            let earliest = self.composed + COMPOSITION_GAP;
            tokio::select! {
                heard = self.changes.changed() => {
                    heard.ok()?;
                    tokio::time::sleep_until(earliest).await;
                }
                () = tokio::time::sleep_until(self.composed + SAMPLE_INTERVAL), if self.running => {}
            }
            // Marked seen before the composition, never after: a change
            // committed while it runs then starts another.
            self.changes.mark_unchanged();
            match self.compose().await {
                Ok(snapshot) if snapshot == self.sent => {}
                Ok(snapshot) => {
                    self.sent = snapshot;
                    return Some(event(&self.sent));
                }
                Err(error) => {
                    warn!(
                        interface_id = self.interface_id,
                        error = format!("{error:#}"),
                        "an observation ended: its snapshot could not be composed"
                    );
                    return None;
                }
            }
        }
    }
}

fn shows_a_running_turn(snapshot: &Snapshot) -> bool {
    let Some(Opened::Recorded { turns, .. }) = &snapshot.open else {
        return false;
    };
    turns
        .iter()
        .any(|turn| matches!(turn.progress, Progress::Running { .. }))
}
