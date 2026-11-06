//! What the bot sent, and for which agent. The provider sends every agent's
//! answers and what agents send through their Discord tools, so it records
//! who had the bot send each of its latest messages, and what agents sent
//! each person directly since that person's last direct message: a reply
//! to the bot then says whose message it answers, and a person's next
//! direct message opens with what they were sent.

use serde::{Deserialize, Serialize};
use twilight_model::{
    channel::Message,
    id::{
        Id,
        marker::{ChannelMarker, MessageMarker},
    },
};

use super::{
    ParticipationState,
    reference::{Reader, quote, rfc3339},
};

/// How many of the bot's latest messages the record names a sender for.
const SENDERS: usize = 1000;
/// How many of the direct messages sent to a person the record keeps for
/// their next one.
const UNANSWERED: usize = 10;

/// The agent that had the bot send a message and, when it sent through its
/// Discord tools, the channel or thread of the conversation it sent from.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Sender {
    pub(crate) agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) channel_id: Option<Id<ChannelMarker>>,
}

/// A direct message sent through the Discord tools, which the person's next
/// direct message quotes.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct SentDirect {
    pub(crate) message_id: Id<MessageMarker>,
    /// `None` when no agent's conversation sent it, as for
    /// `carillon discord call`.
    #[serde(default)]
    pub(crate) sender: Option<Sender>,
    pub(crate) sent_at: String,
    pub(crate) content: String,
    pub(crate) files: Vec<String>,
}

impl SentDirect {
    pub(crate) fn new(message: &Message, sender: Option<Sender>) -> Self {
        Self {
            message_id: message.id,
            sender,
            sent_at: rfc3339(message.timestamp),
            content: message.content.clone(),
            files: files(message),
        }
    }

    pub(super) fn quote(&self, reader: &Reader<'_>) -> String {
        let sender = self.sender.as_ref();
        quote(
            format!(
                "{} sent them this direct message at {}{}",
                reader.subject(sender),
                self.sent_at,
                reader.from(sender)
            ),
            &self.files,
            &self.content,
        )
    }
}

impl ParticipationState {
    /// Records `sender` as who had the bot send `messages`, forgetting the
    /// oldest records beyond [`SENDERS`].
    pub(crate) fn sent_by(
        &mut self,
        messages: impl IntoIterator<Item = Id<MessageMarker>>,
        sender: &Sender,
    ) {
        for message in messages {
            self.senders.insert(message, sender.clone());
        }
        while self.senders.len() > SENDERS {
            self.senders.pop_first();
        }
    }

    /// Keeps `message`, sent to `person` directly, for their next direct
    /// message, forgetting the oldest beyond [`UNANSWERED`].
    pub(crate) fn sent_directly(&mut self, person: &str, message: SentDirect) {
        let kept = self.unanswered.entry(person.to_owned()).or_default();
        if kept.len() == UNANSWERED {
            kept.remove(0);
        }
        kept.push(message);
    }

    /// Keeps a direct message the person has yet to answer as `message` now
    /// reads, after an edit.
    pub(crate) fn edited_directly(&mut self, message: &Message) {
        for sent in self.unanswered.values_mut().flatten() {
            if sent.message_id == message.id {
                sent.content.clone_from(&message.content);
                sent.files = files(message);
            }
        }
    }

    /// Forgets a direct message the person has yet to answer once it is
    /// deleted, so nothing quotes what they can no longer see.
    pub(crate) fn deleted_directly(&mut self, message: Id<MessageMarker>) {
        for kept in self.unanswered.values_mut() {
            kept.retain(|sent| sent.message_id != message);
        }
        self.unanswered.retain(|_, kept| !kept.is_empty());
    }

    /// What was sent `person` directly before their direct message
    /// `message`, oldest first.
    pub(super) fn unanswered_before(
        &self,
        person: &str,
        message: Id<MessageMarker>,
    ) -> impl Iterator<Item = &SentDirect> {
        self.unanswered
            .get(person)
            .into_iter()
            .flatten()
            .filter(move |sent| sent.message_id < message)
    }

    /// Forgets what [`Self::unanswered_before`] gives for `message`, which
    /// quoted it, returning whether there was any.
    pub(crate) fn answered(&mut self, person: &str, message: Id<MessageMarker>) -> bool {
        let Some(kept) = self.unanswered.get_mut(person) else {
            return false;
        };
        let before = kept.len();
        kept.retain(|sent| sent.message_id > message);
        let forgot = kept.len() != before;
        if kept.is_empty() {
            self.unanswered.remove(person);
        }
        forgot
    }
}

fn files(message: &Message) -> Vec<String> {
    message
        .attachments
        .iter()
        .map(|attachment| attachment.filename.clone())
        .collect()
}
