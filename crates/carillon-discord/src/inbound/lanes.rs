//! Conversation lanes: messages of one conversation are prepared one at a
//! time in arrival order, through acceptance or refusal, while other
//! conversations proceed. A message's lane is the conversation it will
//! belong to, known on arrival: a message that will start a thread is its
//! own new thread's lane, so later messages in that thread wait for it.
//!
//! A `/command` submission takes its place in its conversation's lane like
//! a message, so core accepts it after the messages before it and before
//! those after. When the lane reaches it, the submission is given the
//! lane's [`Turn`], and the lane waits until that is dropped. A submission
//! whose answer from core did not come in time hands its event back with
//! [`Turn::settle`]: core may still be accepting it, so the lane submits it
//! again under its own identity until core answers, as for a message, and
//! only then goes on; the feedback that follows does not hold it.
//!
//! Lanes live in memory, bounded overall and per lane; a message beyond the
//! bounds is refused at once rather than queued without limit. Only a
//! lane's first message runs, so a slow transfer holds back its own
//! conversation and, through the download slots, at most the configured
//! number of other transfers. The dispatcher itself never waits on the
//! network.
//!
//! When the provider stops, waiting messages are dropped, as a crash would
//! drop them, and running ones end their preparation or settle what they
//! handed to core before the dispatcher returns. Once it is stopping, a
//! command's submission, from its first attempt to the lane's settlement,
//! starts no attempt beyond its first: one core has not confirmed when the
//! attempt under way ends is left unconfirmed rather than holding the stop.
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

use carillon_protocol::model::ExternalEvent;
use serde_json::Value;
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::{Ingress, QueuedMessage, prepare, submit_event_with_retry};
use crate::{
    config::{DiscordConfig, DiscordConversation},
    conversations::{IncomingMessage, admit_message},
    delivery::{Feedback, explain},
};

/// Messages waiting or in preparation, across every lane.
pub(crate) const MAX_PENDING: usize = 64;
/// Messages waiting or in preparation in one conversation.
pub(crate) const MAX_PER_LANE: usize = 16;
/// Overload replies being sent at once; beyond them refusals are only logged.
pub(crate) const REFUSALS: usize = 4;

/// What a conversation's lane holds, in arrival order.
pub(crate) enum Queued {
    Message(QueuedMessage),
    /// A `/command` submission, waiting for its conversation's turn.
    Submission(Submission),
}

impl From<QueuedMessage> for Queued {
    fn from(message: QueuedMessage) -> Self {
        Self::Message(message)
    }
}

/// A `/command` submission's place in the lane of the conversation in
/// `channel`: a command never starts a thread, so its conversation's lane
/// is its channel. The lane sends it the [`Turn`] when it reaches it.
pub(crate) struct Submission {
    channel: String,
    /// Taken when the lane reaches the submission.
    turn: Option<oneshot::Sender<Turn>>,
}

/// The lane's turn, held by a submission while it runs; the lane goes on
/// once it is dropped, or once the event handed to [`Turn::settle`] is.
pub(crate) struct Turn {
    unsettled: oneshot::Sender<ExternalEvent>,
    stopping: CancellationToken,
}

impl Turn {
    /// Cancelled when the provider stops: the submission then starts no
    /// attempt after its first.
    pub(crate) const fn stopping(&self) -> &CancellationToken {
        &self.stopping
    }

    /// Leaves `event`, which core may still be accepting after the
    /// submission's own attempts, to the lane: the conversation goes on only
    /// once core has accepted or refused it, and a stopping provider leaves
    /// it unconfirmed if core still has not.
    pub(crate) fn settle(self, event: ExternalEvent) {
        let _ = self.unsettled.send(event);
    }
}

/// Where `/command` submissions take their place among the messages of
/// their conversations.
#[derive(Clone)]
pub(crate) struct Lanes(mpsc::Sender<Queued>);

impl Lanes {
    pub(crate) const fn new(sender: mpsc::Sender<Queued>) -> Self {
        Self(sender)
    }

    /// The turn of the conversation in `channel`, once the messages before
    /// it have been accepted or refused. `None` when the lanes are full or
    /// stopping and the submission was not queued, or was dropped unrun.
    pub(crate) async fn turn(&self, channel: &str) -> Option<Turn> {
        let (turn, given) = oneshot::channel();
        let submission = Submission {
            channel: channel.to_owned(),
            turn: Some(turn),
        };
        // Like a gateway message, a submission never waits for room.
        self.0.try_send(Queued::Submission(submission)).ok()?;
        given.await.ok()
    }
}

/// The conversation a message will belong to, before any Discord lookup:
/// its own thread when it starts one, otherwise its channel or thread.
pub(crate) fn lane(config: &DiscordConfig, message: &IncomingMessage) -> String {
    let starts_thread = config.routes.iter().any(|route| {
        route.channel_id == message.channel_id && route.conversation == DiscordConversation::Thread
    });
    if starts_thread {
        message.event_id.clone()
    } else {
        message.channel_id.clone()
    }
}

/// Prepare messages as they arrive until the gateway stops sending them and
/// every lane has drained, or until the provider stops and running
/// preparations have settled.
pub(crate) async fn dispatch(ingress: Arc<Ingress>, mut messages: mpsc::Receiver<Queued>) {
    let mut lanes: HashMap<String, VecDeque<Queued>> = HashMap::new();
    let mut pending = 0_usize;
    let mut running = JoinSet::new();
    let mut open = true;
    loop {
        tokio::select! {
            received = messages.recv(), if open => {
                let Some(queued) = received else {
                    open = false;
                    continue;
                };
                let key = match &queued {
                    Queued::Message(message) => lane(&ingress.config, &message.request),
                    Queued::Submission(submission) => submission.channel.clone(),
                };
                let queue = lanes.entry(key.clone()).or_default();
                if pending >= MAX_PENDING || queue.len() >= MAX_PER_LANE {
                    if queue.is_empty() {
                        lanes.remove(&key);
                    }
                    // A refused submission is dropped: its command is not sent.
                    if let Queued::Message(message) = queued {
                        refuse_overloaded(&ingress, message.request);
                    }
                    continue;
                }
                pending += 1;
                queue.push_back(queued);
                if queue.len() == 1 {
                    start(&mut running, &ingress, key, queue);
                }
            }
            Some(finished) = running.join_next() => {
                let key = match finished {
                    Ok(key) => key,
                    Err(failure) => {
                        // Preparation code does not panic; if it ever does,
                        // the lane it held stays blocked rather than reordered.
                        warn!(error = %failure, "a Discord message preparation task failed");
                        continue;
                    }
                };
                pending -= 1;
                let Some(queue) = lanes.get_mut(&key) else {
                    continue;
                };
                queue.pop_front();
                if queue.is_empty() || ingress.stopping.is_cancelled() {
                    lanes.remove(&key);
                } else {
                    start(&mut running, &ingress, key, queue);
                }
            }
            () = ingress.stopping.cancelled(), if open => {
                open = false;
                let waiting = pending - running.len();
                if waiting > 0 {
                    warn!(waiting, "Discord stopped before preparing waiting messages");
                }
            }
            else => break,
        }
    }
}

fn start(
    running: &mut JoinSet<String>,
    ingress: &Arc<Ingress>,
    key: String,
    queue: &mut VecDeque<Queued>,
) {
    match queue.front_mut() {
        Some(Queued::Message(head)) => {
            let (ingress, head) = (ingress.clone(), head.clone());
            running.spawn(async move {
                prepare(&ingress, head).await;
                key
            });
        }
        Some(Queued::Submission(head)) => {
            // The submission runs while it holds the turn. A turn nobody
            // takes, as for a submission that stopped waiting, is dropped
            // here, and the lane goes on at once.
            let (unsettled, handed) = oneshot::channel();
            let turn = Turn {
                unsettled,
                stopping: ingress.stopping.clone(),
            };
            if let Some(waiting) = head.turn.take() {
                let _ = waiting.send(turn);
            }
            let ingress = ingress.clone();
            running.spawn(async move {
                if let Ok(event) = handed.await {
                    settle(&ingress, &event).await;
                }
                key
            });
        }
        None => {}
    }
}

/// Settles a command core may still be accepting, as an ambiguous
/// submission of a message is settled: the same event again until core
/// answers. An acceptance is shown by its delivery, and a refusal on the
/// command's marker, beside the lane. The submission already made its first
/// attempt, so once the provider is stopping only an attempt under way here
/// ends, and none starts: a command core then has not answered for is left
/// unconfirmed, and its marker says so.
async fn settle(ingress: &Ingress, event: &ExternalEvent) {
    let target = event.feedback_target_id.clone();
    match submit_event_with_retry(&ingress.core, &ingress.stopping, event).await {
        Some(Ok(_)) => {}
        Some(Err(failure)) => {
            warn!(error = %failure.message, event_id = %event.event_id, "core refused a command after its answer was late");
            ingress.reactions.show_outcome(target, Feedback::Failed);
        }
        None => {
            warn!(event_id = %event.event_id, "Discord stopped before core confirmed whether it accepted a command");
            ingress.reactions.show_outcome(target, Feedback::Uncertain);
        }
    }
}

/// Refuse a message beyond the bounds. Only a message admission would
/// activate is answered, so overload never replies to messages Carillon
/// would ignore; a thread the provider has not yet seen is not answered.
pub(crate) fn refuse_overloaded(ingress: &Arc<Ingress>, message: IncomingMessage) {
    warn!(
        channel = %message.channel_id,
        message = %message.event_id,
        "Discord message refused: too many messages are waiting"
    );
    let mut request = message.clone();
    if request.parent_channel_id.is_none() {
        request.parent_channel_id = ingress.participation.parent_route(&request.channel_id);
    }
    if !matches!(
        admit_message(
            &ingress.config,
            &ingress.participation,
            request,
            Value::Null
        ),
        Ok(Some(_))
    ) {
        return;
    }
    let Ok(permit) = ingress.refusals.clone().try_acquire_owned() else {
        return;
    };
    // The refusal is feedback: the connection's reactions own it, so it
    // ends when they stop.
    let owner = ingress.clone();
    ingress.reactions.spawn(async move {
        let source = format!("{}:{}", message.channel_id, message.event_id);
        explain(
            &owner.http,
            &source,
            "Carillon is too busy to take this message right now; please send it again shortly.",
        )
        .await;
        owner.reactions.outcome(&source, Feedback::Failed).await;
        drop(permit);
    });
}
