//! What a message points at: the message it replies to, or the one it
//! forwards. The turn's text opens with a short quote of it, so the agent
//! knows what the person answered without anything earlier changing.
//!
//! A quote is Carillon's line saying what is quoted, then the quoted words,
//! every line of them marked with `>`: nothing quoted reads as one of
//! Carillon's lines. Names and file names the line carries are JSON
//! strings, and a person is named with their user ID, so no name passes for
//! anything else.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use twilight_model::{
    channel::{
        Attachment,
        message::{Message, MessageType},
    },
    id::{Id, marker::MessageMarker},
    util::Timestamp,
};

use super::{IncomingMessage, ParticipationState, Sender};

/// How much of the quoted message's text the quote keeps.
const QUOTED_CHARS: usize = 300;

/// The message a new one points at, as Discord showed it to its sender.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Reference {
    pub kind: ReferenceKind,
    /// Who wrote it. A forward's snapshot does not say.
    #[serde(default)]
    pub author: Option<Author>,
    /// When it was sent, in RFC 3339.
    pub sent_at: String,
    /// It forwards another message, whose words and files these are.
    #[serde(default)]
    pub forwarding: bool,
    #[serde(default)]
    pub content: String,
    /// Its files' names, in message order.
    #[serde(default)]
    pub files: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceKind {
    Reply,
    Forward,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Author {
    /// The bot, which sends for every agent: the provider's record of the
    /// message says which.
    Bot { message_id: Id<MessageMarker> },
    /// A person, by user ID and the name Discord shows.
    Person { id: String, name: String },
}

impl Reference {
    /// The message `message` replies to or forwards, when Discord sent it
    /// along. A reply to a deleted message has none.
    pub(crate) fn of(message: &Message, bot_user_id: &str) -> Option<Self> {
        if let Some(snapshot) = message.message_snapshots.first() {
            let forwarded = &snapshot.message;
            return Some(Self::new(
                ReferenceKind::Forward,
                None,
                forwarded.timestamp,
                &forwarded.content,
                &forwarded.attachments,
                false,
            ));
        }
        if message.kind != MessageType::Reply {
            return None;
        }
        let replied = message.referenced_message.as_deref()?;
        let author = if replied.author.id.to_string() == bot_user_id {
            Author::Bot {
                message_id: replied.id,
            }
        } else {
            Author::Person {
                id: replied.author.id.to_string(),
                name: replied
                    .author
                    .global_name
                    .clone()
                    .unwrap_or_else(|| replied.author.name.clone()),
            }
        };
        let (content, attachments, forwarding) = match replied.message_snapshots.first() {
            Some(snapshot) => (
                &snapshot.message.content,
                &snapshot.message.attachments,
                true,
            ),
            None => (&replied.content, &replied.attachments, false),
        };
        Some(Self::new(
            ReferenceKind::Reply,
            Some(author),
            replied.timestamp,
            content,
            attachments,
            forwarding,
        ))
    }

    fn new(
        kind: ReferenceKind,
        author: Option<Author>,
        sent_at: Timestamp,
        content: &str,
        attachments: &[Attachment],
        forwarding: bool,
    ) -> Self {
        Self {
            kind,
            author,
            sent_at: rfc3339(sent_at),
            forwarding,
            content: content.to_owned(),
            files: attachments
                .iter()
                .map(|attachment| attachment.filename.clone())
                .collect(),
        }
    }

    /// Its quote for `reader`: who wrote the message, when, whether it
    /// forwards one, its files' names and how it began.
    fn quote(&self, reader: &Reader<'_>) -> String {
        let (who, from) = match &self.author {
            Some(Author::Bot { message_id }) => {
                let sender = reader.participation.senders.get(message_id);
                (reader.message_of(sender), reader.from(sender))
            }
            Some(Author::Person { id, name }) => (
                format!("a message from {} (Discord user {id})", json(name)),
                String::new(),
            ),
            None => ("a message".to_owned(), String::new()),
        };
        let lead = match self.kind {
            ReferenceKind::Reply => "In reply to",
            ReferenceKind::Forward => "Forwarding",
        };
        let forwarding = if self.forwarding {
            ", forwarding a message"
        } else {
            ""
        };
        quote(
            format!("{lead} {who}, sent {}{from}{forwarding}", self.sent_at),
            &self.files,
            &self.content,
        )
    }
}

/// Whom a turn's quotes are for: the agent the turn goes to, in the channel
/// it came from, and what the provider recorded of the bot's messages.
pub(crate) struct Reader<'a> {
    pub(crate) agent_id: &'a str,
    pub(crate) channel_id: &'a str,
    pub(crate) participation: &'a ParticipationState,
}

impl Reader<'_> {
    /// What opens the turn's text for `message`: for a direct message, what
    /// was sent its person directly before it, oldest first, but for the
    /// message it replies to; then the message it replies to or forwards.
    pub(crate) fn quotes(&self, message: &IncomingMessage) -> Vec<String> {
        let replied = match &message.reference {
            Some(Reference {
                author: Some(Author::Bot { message_id }),
                ..
            }) => Some(*message_id),
            _ => None,
        };
        let sent = match (&message.guild_id, message.event_id.parse()) {
            (None, Ok(id)) => Some(self.participation.unanswered_before(&message.user_id, id)),
            _ => None,
        };
        sent.into_iter()
            .flatten()
            .filter(|sent| Some(sent.message_id) != replied)
            .map(|sent| sent.quote(self))
            .chain(
                message
                    .reference
                    .iter()
                    .map(|reference| reference.quote(self)),
            )
            .collect()
    }

    /// Whose a message of the bot was, as its sender is recorded: the
    /// reader's own only when the record says so.
    fn message_of(&self, sender: Option<&Sender>) -> String {
        match sender {
            Some(sender) if sender.agent_id == self.agent_id => "your message".to_owned(),
            Some(sender) => format!("a message from agent {}", json(&sender.agent_id)),
            None => "a message from another agent".to_owned(),
        }
    }

    /// Who had the bot send a message, as the subject of a sentence.
    pub(super) fn subject(&self, sender: Option<&Sender>) -> String {
        match sender {
            Some(sender) if sender.agent_id == self.agent_id => "You".to_owned(),
            Some(sender) => format!("Agent {}", json(&sender.agent_id)),
            None => "Another agent".to_owned(),
        }
    }

    /// Where an agent sent a message from, when that is another channel.
    pub(super) fn from(&self, sender: Option<&Sender>) -> String {
        sender
            .and_then(|sender| sender.channel_id)
            .filter(|channel| channel.to_string() != self.channel_id)
            .map(|channel| format!(" from <#{channel}>"))
            .unwrap_or_default()
    }
}

/// A quote: `line`, naming `files` when there are any, then the opening of
/// `content`, each line marked as quoted.
pub(super) fn quote(mut line: String, files: &[String], content: &str) -> String {
    if !files.is_empty() {
        line.push_str(", with files ");
        line.push_str(&escaped(&Value::from(files.to_vec()).to_string()));
    }
    line.push(':');
    let text = content.trim();
    let mut opening: String = text.chars().take(QUOTED_CHARS).collect();
    if opening.len() < text.len() {
        opening.push('…');
    }
    std::iter::once(line)
        .chain(
            opening
                .replace("\r\n", "\n")
                .split(LINE_BREAKS)
                .map(|quoted| format!("> {quoted}").trim_end().to_owned()),
        )
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every character a reader may take for the end of a line.
const LINE_BREAKS: [char; 7] = [
    '\n', '\r', '\u{0b}', '\u{0c}', '\u{85}', '\u{2028}', '\u{2029}',
];

/// `text` as a JSON string, which nothing in it can end early.
fn json(text: &str) -> String {
    escaped(&Value::from(text).to_string())
}

/// JSON leaves the Unicode line and paragraph separators unescaped.
fn escaped(json: &str) -> String {
    json.replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

pub(super) fn rfc3339(at: Timestamp) -> String {
    at.iso_8601().with_microseconds(false).to_string()
}
