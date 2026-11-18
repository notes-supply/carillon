//! `GET /api/stream`: what the page's view shows, as a stream of complete
//! snapshots, and the page's one source of what the server knows. Each
//! event is one message. The first is a snapshot, `unavailable` or
//! `signed_out`, and another snapshot follows whenever what the view shows
//! has changed.
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
use carillon_interface::Observation;
use carillon_protocol::observation::{KEEP_ALIVE, View};
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
    /// Core has not been asked yet, and only core ends the wait for its
    /// first answer.
    Unshown,
    /// A snapshot is shown, so the day's end or the provider stopping may
    /// end the stream.
    Showing(Box<Observation>),
    /// Its last message is sent.
    Over,
}

/// What a stream has made to send. Only [`Shown::send`] makes it an event.
enum Ready {
    /// Nothing, for as long as a connection may stay silent.
    Quiet,
    /// A snapshot that differs from the one last sent, as written.
    Snapshot(serde_json::Result<String>),
    /// Core cannot be reached, refused the view or ended its stream.
    Unavailable,
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
            stage: Stage::Unshown,
            sent: String::new(),
            quiet_until: opened + KEEP_ALIVE,
        },
        Shown::next,
    ))
}

impl Shown {
    async fn next(mut self) -> Option<(serde_json::Result<Event>, Self)> {
        let ready = self.ready().await?;
        let event = self.send(ready)?;
        Some((event, self))
    }

    /// What to send next, or `None` once the stream is over.
    async fn ready(&mut self) -> Option<Ready> {
        loop {
            let heard = match &mut self.stage {
                Stage::Over => return None,
                Stage::Unshown => match self.web.core.observe(&self.viewed.view).await {
                    Ok(mut observation) => {
                        let first = observation.next().await;
                        self.stage = Stage::Showing(Box::new(observation));
                        first
                    }
                    Err(failure) => Err(failure),
                },
                Stage::Showing(observation) => tokio::select! {
                    biased;
                    () = self.web.stopping.cancelled() => return None,
                    () = sleep_until(self.day_ends) => return None,
                    () = sleep_until(self.quiet_until) => return Some(Ready::Quiet),
                    next = observation.next() => next,
                },
            };
            let snapshot = match heard {
                Ok(Some(snapshot)) => snapshot,
                Ok(None) => {
                    warn!("core ended the stream of the page's view");
                    return Some(Ready::Unavailable);
                }
                Err(failure) => {
                    warn!(
                        status = %failure.status,
                        error = format!("{failure:#}"),
                        "core does not show the page its view"
                    );
                    return Some(Ready::Unavailable);
                }
            };
            let links = Links {
                web: &self.web,
                key: &self.key,
                day: self.day,
            };
            let snapshot = ChatSnapshot::of(&links, self.viewed.open.as_ref(), snapshot).await;
            match serde_json::to_string(&Message::Snapshot {
                snapshot: &snapshot,
            }) {
                Ok(data) if data == self.sent => {}
                data => return Some(Ready::Snapshot(data)),
            }
        }
    }

    /// `ready` as the event the page is sent, or `None` once the stream's
    /// day has ended. Every event leaves through here, and nothing here
    /// awaits, so the day and the password it reads are the ones the event
    /// leaves under. A new password ends every stream within a keep-alive.
    fn send(&mut self, ready: Ready) -> Option<serde_json::Result<Event>> {
        let leaving = Instant::now();
        if leaving >= self.day_ends {
            return None;
        }
        if !self.key.is_current(&self.web.browser.password_file) {
            return Some(self.last(&Message::SignedOut));
        }
        self.quiet_until = leaving + KEEP_ALIVE;
        Some(match ready {
            Ready::Quiet => Ok(Event::default().comment("keep-alive")),
            Ready::Snapshot(Ok(data)) => {
                self.sent = data;
                Ok(Event::default().data(&self.sent))
            }
            Ready::Snapshot(Err(error)) => {
                self.stage = Stage::Over;
                Err(error)
            }
            Ready::Unavailable => self.last(&Message::Unavailable),
        })
    }

    fn last(&mut self, message: &Message<'_>) -> serde_json::Result<Event> {
        self.stage = Stage::Over;
        Ok(Event::default().data(serde_json::to_string(message)?))
    }
}

#[cfg(test)]
mod tests;
