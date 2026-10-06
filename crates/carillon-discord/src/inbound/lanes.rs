//! Conversation lanes: messages of one conversation are prepared one at a
//! time in arrival order, through acceptance or refusal, while other
//! conversations proceed. A message's lane is the conversation it will
//! belong to, known on arrival: a message that will start a thread is its
//! own new thread's lane, so later messages in that thread wait for it.
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
//! handed to core before the dispatcher returns.
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

use serde_json::Value;
use tokio::{sync::mpsc, task::JoinSet};
use tracing::warn;

use super::{Ingress, QueuedMessage, prepare};
use crate::{
    config::{DiscordConfig, DiscordConversation},
    conversations::{IncomingMessage, admit_message},
    delivery::{Feedback, explain, terminal_feedback},
};

/// Messages waiting or in preparation, across every lane.
pub(crate) const MAX_PENDING: usize = 64;
/// Messages waiting or in preparation in one conversation.
pub(crate) const MAX_PER_LANE: usize = 16;
/// Overload replies being sent at once; beyond them refusals are only logged.
pub(crate) const REFUSALS: usize = 4;

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
pub(crate) async fn dispatch(ingress: Arc<Ingress>, mut messages: mpsc::Receiver<QueuedMessage>) {
    let mut lanes: HashMap<String, VecDeque<QueuedMessage>> = HashMap::new();
    let mut pending = 0_usize;
    let mut running = JoinSet::new();
    let mut open = true;
    loop {
        tokio::select! {
            received = messages.recv(), if open => {
                let Some(message) = received else {
                    open = false;
                    continue;
                };
                let key = lane(&ingress.config, &message.request);
                let queue = lanes.entry(key.clone()).or_default();
                if pending >= MAX_PENDING || queue.len() >= MAX_PER_LANE {
                    if queue.is_empty() {
                        lanes.remove(&key);
                    }
                    refuse_overloaded(&ingress, message.request);
                    continue;
                }
                pending += 1;
                queue.push_back(message);
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
    queue: &VecDeque<QueuedMessage>,
) {
    if let Some(head) = queue.front().cloned() {
        let ingress = ingress.clone();
        running.spawn(async move {
            prepare(&ingress, head).await;
            key
        });
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
    let ingress = ingress.clone();
    tokio::spawn(async move {
        let source = format!("{}:{}", message.channel_id, message.event_id);
        explain(
            &ingress.http,
            &source,
            "Carillon is too busy to take this message right now; please send it again shortly.",
        )
        .await;
        terminal_feedback(&ingress.http, &ingress.config, &source, Feedback::Failed).await;
        drop(permit);
    });
}
