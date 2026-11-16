//! `GET /api/stream`: what the page's view shows, as a stream of complete
//! snapshots, and the page's one source of what the server knows. Each
//! event is one message. The first is a snapshot or `unavailable`, and
//! another snapshot follows whenever what the view shows has changed.
//!
//! How a stream ends says what the page does next. After `unavailable` it
//! waits and opens another, and after `signed_out` it asks for the
//! password. A stream that ends with neither ended because its day did or
//! because the provider is stopping, and the page opens the next at once.
//! Nothing here opens core's stream a second time: the page owns the only
//! reconnect loop.
//!
//! The stream is the response body and nothing else, as core's own is. No
//! task and no queue stands behind it, so core is read only when the page
//! takes the message before: a slow page gets the latest state when it
//! reads again, never a backlog, and a page that goes away leaves nothing
//! behind.

use std::sync::Arc;

use axum::{
    Extension,
    extract::{Query, State},
    response::sse::{Event, Sse},
};
use carillon_interface::{InterfaceError, Observation};
use carillon_protocol::observation::{KEEP_ALIVE, Snapshot, View};
use futures_util::{Stream, stream};
use serde::{Deserialize, Serialize};
use tokio::time::{Instant, sleep_until};
use tracing::warn;

use crate::{
    browser::{Key, now},
    files::{Day, Links},
    identity::{ConversationKey, Malformed},
    serve::Web,
    snapshot::ChatSnapshot,
};

/// One event's data.
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Message<'a> {
    /// Everything the view shows. The page replaces what it had.
    Snapshot { snapshot: &'a ChatSnapshot },
    /// The provider is here and core is not. The stream ends.
    Unavailable,
    /// The password changed, so the token is void. The stream ends.
    SignedOut,
}

/// The view the page asks for: one core accepts, whose open conversation
/// is one the page could have made.
#[derive(Deserialize)]
#[serde(try_from = "View")]
pub(crate) struct Viewed {
    view: View,
    open: Option<ConversationKey>,
}

impl TryFrom<View> for Viewed {
    type Error = Malformed;

    fn try_from(view: View) -> Result<Self, Malformed> {
        let open = match &view.open {
            Some(open) => {
                Some(ConversationKey::of(&open.conversation).ok_or(Malformed::Conversation)?)
            }
            None => None,
        };
        Ok(Self { view, open })
    }
}

/// How far one page's stream has come.
enum Stage {
    /// Core has not been asked yet.
    Opening,
    Open(Box<Observation>),
    /// Its last message is sent.
    Over,
}

/// What a stream waited for.
enum Heard {
    /// Nothing, for as long as a connection may stay silent.
    Quiet,
    Core(Result<Option<Snapshot>, InterfaceError>),
}

/// One page's stream of its view.
struct Shown {
    web: Arc<Web>,
    /// The key the page's token was checked against. Links are signed with
    /// it, and the stream ends once it is no longer the password's.
    key: Key,
    viewed: Viewed,
    /// The day the stream opened on, which its links are minted for.
    day: Day,
    day_ends: Instant,
    stage: Stage,
    /// The snapshot last sent, as it was written.
    sent: String,
    quiet_until: Instant,
}

/// Answers at once and asks core only when the page reads, so a failure of
/// core is a message of the stream and never a status.
pub(crate) async fn stream(
    State(web): State<Arc<Web>>,
    Extension(key): Extension<Key>,
    Query(viewed): Query<Viewed>,
) -> Sse<impl Stream<Item = serde_json::Result<Event>>> {
    let (day, left) = Day::at(now());
    let opened = Instant::now();
    Sse::new(stream::unfold(
        Shown {
            web,
            key,
            viewed,
            day,
            day_ends: opened + left,
            stage: Stage::Opening,
            sent: String::new(),
            quiet_until: opened + KEEP_ALIVE,
        },
        Shown::next,
    ))
}

impl Shown {
    async fn next(mut self) -> Option<(serde_json::Result<Event>, Self)> {
        let event = self.event().await?;
        if event.is_err() {
            self.stage = Stage::Over;
        }
        Some((event, self))
    }

    /// The next event to send, or `None` once the stream is over. Before
    /// each one the password is read again, so a new one ends every stream
    /// within a keep-alive.
    async fn event(&mut self) -> Option<serde_json::Result<Event>> {
        loop {
            let heard = match &mut self.stage {
                Stage::Over => return None,
                // The day's end is not waited for here: a stream that ends
                // with its day has shown a snapshot.
                Stage::Opening => {
                    Heard::Core(match self.web.core.observe(&self.viewed.view).await {
                        Ok(mut observation) => {
                            let first = observation.next().await;
                            self.stage = Stage::Open(Box::new(observation));
                            first
                        }
                        Err(failure) => Err(failure),
                    })
                }
                Stage::Open(observation) => tokio::select! {
                    biased;
                    () = self.web.stopping.cancelled() => return None,
                    () = sleep_until(self.day_ends) => return None,
                    () = sleep_until(self.quiet_until) => Heard::Quiet,
                    next = observation.next() => Heard::Core(next),
                },
            };
            if !self.key.is_current(&self.web.browser.password_file) {
                return Some(self.last(&Message::SignedOut));
            }
            let snapshot = match heard {
                Heard::Quiet => {
                    self.quiet_until = Instant::now() + KEEP_ALIVE;
                    return Some(Ok(Event::default().comment("keep-alive")));
                }
                Heard::Core(Ok(Some(snapshot))) => snapshot,
                Heard::Core(Ok(None)) => {
                    warn!("core ended the stream of the page's view");
                    return Some(self.last(&Message::Unavailable));
                }
                Heard::Core(Err(failure)) => {
                    warn!(
                        status = %failure.status,
                        error = %failure.message,
                        "core does not show the page its view"
                    );
                    return Some(self.last(&Message::Unavailable));
                }
            };
            let links = Links {
                web: &self.web,
                key: &self.key,
                day: self.day,
            };
            let snapshot = ChatSnapshot::of(&links, self.viewed.open.as_ref(), snapshot).await;
            let data = match written(&Message::Snapshot {
                snapshot: &snapshot,
            }) {
                Ok(data) => data,
                Err(error) => return Some(Err(error)),
            };
            // Two of core's snapshots can show the page the same: they
            // differ in what stays behind.
            if data != self.sent {
                self.sent = data;
                self.quiet_until = Instant::now() + KEEP_ALIVE;
                return Some(Ok(Event::default().data(&self.sent)));
            }
        }
    }

    /// `message` as the stream's last event.
    fn last(&mut self, message: &Message<'_>) -> serde_json::Result<Event> {
        self.stage = Stage::Over;
        Ok(Event::default().data(written(message)?))
    }
}

fn written(message: &Message<'_>) -> serde_json::Result<String> {
    serde_json::to_string(message)
}

#[cfg(test)]
mod tests;
