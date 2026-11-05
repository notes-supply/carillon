//! What a message points at: the message it replies to, or the one it
//! forwards. The turn's text opens with a short quote of it, so the agent
//! knows what the person answered without anything earlier changing.

use serde::{Deserialize, Serialize};
use twilight_model::{
    channel::{
        Attachment,
        message::{Message, MessageReferenceType, MessageType},
    },
    util::Timestamp,
};

/// How much of the referenced message's text the quote keeps.
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
    /// The bot, which speaks for the agent.
    Agent,
    /// Someone else, by the name Discord shows.
    Person(String),
}

impl Reference {
    /// The message `message` replies to or forwards, when Discord sent it
    /// along. A reply to a deleted message has none.
    pub(crate) fn of(message: &Message, bot_user_id: &str) -> Option<Self> {
        let forward = message
            .reference
            .as_ref()
            .is_some_and(|reference| reference.kind == MessageReferenceType::Forward);
        if forward {
            let snapshot = &message.message_snapshots.first()?.message;
            return Some(Self::new(
                ReferenceKind::Forward,
                None,
                snapshot.timestamp,
                &snapshot.content,
                &snapshot.attachments,
            ));
        }
        if message.kind != MessageType::Reply {
            return None;
        }
        let replied = message.referenced_message.as_deref()?;
        let author = if replied.author.id.to_string() == bot_user_id {
            Author::Agent
        } else {
            Author::Person(
                replied
                    .author
                    .global_name
                    .clone()
                    .unwrap_or_else(|| replied.author.name.clone()),
            )
        };
        Some(Self::new(
            ReferenceKind::Reply,
            Some(author),
            replied.timestamp,
            &replied.content,
            &replied.attachments,
        ))
    }

    fn new(
        kind: ReferenceKind,
        author: Option<Author>,
        sent_at: Timestamp,
        content: &str,
        attachments: &[Attachment],
    ) -> Self {
        Self {
            kind,
            author,
            sent_at: sent_at.iso_8601().with_microseconds(false).to_string(),
            content: content.to_owned(),
            files: attachments
                .iter()
                .map(|attachment| attachment.filename.clone())
                .collect(),
        }
    }

    /// The quote that opens the turn's text: who wrote the message, when,
    /// how it began and its files' names.
    pub(crate) fn quote(&self) -> String {
        let source = match (self.kind, &self.author) {
            (ReferenceKind::Reply, Some(Author::Agent)) => "In reply to your message".to_owned(),
            (ReferenceKind::Reply, Some(Author::Person(name))) => format!("In reply to {name}"),
            (ReferenceKind::Reply, None) => "In reply to a message".to_owned(),
            (ReferenceKind::Forward, Some(Author::Agent)) => "Forwarding your message".to_owned(),
            (ReferenceKind::Forward, Some(Author::Person(name))) => {
                format!("Forwarding a message by {name}")
            }
            (ReferenceKind::Forward, None) => "Forwarding a message".to_owned(),
        };
        let text = self.content.trim();
        let mut opening: String = text.chars().take(QUOTED_CHARS).collect();
        if opening.len() < text.len() {
            opening.push('…');
        }
        let mut lines = vec![format!("{source}, sent {}:", self.sent_at)];
        lines.extend(opening.lines().map(str::to_owned));
        if !self.files.is_empty() {
            lines.push(format!("Files: {}", self.files.join(", ")));
        }
        lines
            .iter()
            .map(|line| format!("> {line}").trim_end().to_owned())
            .collect::<Vec<_>>()
            .join("\n")
    }
}
