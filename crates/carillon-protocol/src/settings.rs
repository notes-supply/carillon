//! Settings an owner declares and renderers draw. The owner of a setting
//! names it, offers its choices, validates a change and computes its
//! revision; a renderer shows what the owner returned and sends changes
//! back unchanged.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::fields::{nonempty_string, required_nullable};

/// What a page of settings is about. Every revision covers it, so a change
/// drawn for one agent never applies after the channel moved to another.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Subject {
    /// The agent's own values, which its channels inherit.
    Agent {
        #[serde(deserialize_with = "nonempty_string")]
        agent_id: String,
    },
    /// One routed channel of an interface, and the agent it routes to.
    Channel {
        #[serde(deserialize_with = "nonempty_string")]
        interface_id: String,
        #[serde(deserialize_with = "nonempty_string")]
        channel_id: String,
        #[serde(deserialize_with = "nonempty_string")]
        agent_id: String,
    },
}

impl Subject {
    #[must_use]
    pub fn agent_id(&self) -> &str {
        match self {
            Self::Agent { agent_id } | Self::Channel { agent_id, .. } => agent_id,
        }
    }
}

/// One line of a page.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Setting {
    /// The owner's name for the setting, such as `model` or `model.thinking`.
    #[serde(deserialize_with = "nonempty_string")]
    pub id: String,
    /// What the line is called, such as `Model`.
    #[serde(deserialize_with = "nonempty_string")]
    pub name: String,
    /// The values the owner accepts here now. Without any, the line is shown
    /// and not editable.
    pub choices: Vec<Choice>,
    /// What applies here, or `None` when nothing does.
    #[serde(deserialize_with = "required_nullable")]
    pub value: Option<Value>,
    /// The value comes from the agent, not from this subject.
    pub inherited: bool,
    pub revision: Revision,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Choice {
    #[serde(deserialize_with = "nonempty_string")]
    pub id: String,
    #[serde(deserialize_with = "nonempty_string")]
    pub name: String,
    /// The heading a renderer lists the choice under, such as its provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

/// A setting's value, as choice IDs.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Value {
    One {
        #[serde(deserialize_with = "nonempty_string")]
        id: String,
    },
    Many {
        ids: Vec<String>,
    },
}

/// The owner's digest of everything that decides what a setting means at its
/// subject: 64 lowercase hexadecimal digits. A change names the revision it
/// was drawn from, and the owner refuses it once the revision is another.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(try_from = "String", into = "String")]
pub struct Revision(String);

impl Revision {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Revision {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            Ok(Self(value))
        } else {
            Err("a revision is 64 lowercase hexadecimal digits".to_owned())
        }
    }
}

impl From<Revision> for String {
    fn from(revision: Revision) -> Self {
        revision.0
    }
}

impl fmt::Display for Revision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A change to one setting of a subject.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Change {
    #[serde(deserialize_with = "nonempty_string")]
    pub setting: String,
    /// `None` returns the setting to what it inherits.
    #[serde(deserialize_with = "required_nullable")]
    pub value: Option<Value>,
    /// The setting's revision the change was drawn from.
    pub expected: Revision,
}

/// A read of a subject's runtime settings, with a change to apply first.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SettingsRequest {
    pub subject: Subject,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<Change>,
}

/// The settings a runtime owns for a subject, in the order it shows them.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct RuntimeSettings {
    pub settings: Vec<Setting>,
}

/// Everything about one subject: the interface's own lines, then the
/// runtime's.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Page {
    /// What the interface calls the subject.
    pub title: String,
    pub subject: Subject,
    pub interface: Vec<Setting>,
    pub runtime: RuntimePart,
}

/// The runtime's lines of a page, or why it could not give them.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RuntimePart {
    Settings(RuntimeSettings),
    Unavailable { reason: String },
}

impl Page {
    /// Every line, the interface's first.
    pub fn settings(&self) -> impl Iterator<Item = &Setting> {
        let runtime = match &self.runtime {
            RuntimePart::Settings(part) => part.settings.as_slice(),
            RuntimePart::Unavailable { .. } => &[],
        };
        self.interface.iter().chain(runtime)
    }
}
