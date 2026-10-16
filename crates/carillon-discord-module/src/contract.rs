//! The JSON exchanged between the Discord provider and its module. Every
//! message is an externally tagged enum. Unknown fields are ignored except
//! in the fixed [`ActivityMarkers`] schema, which is also provider config.

use std::collections::BTreeMap;

use carillon_protocol::{
    model::{CommandCatalog, Job, ModelScope as Scope, TurnActivity},
    models::{ModelState, Selection},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Contract version the module speaks; the provider refuses others.
pub const CONTRACT: u32 = 5;

/// Provider to module: the invocation payload.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Invocation {
    /// Ask for the module's contract version and command definitions.
    Describe,
    /// Handle an admitted interaction. Replies are ephemeral; a component
    /// interaction's reply updates the message carrying the component.
    Interaction(Box<Interaction>),
    /// Show a job on its card. The module may make no native call.
    Card(Box<CardRequest>),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Interaction {
    pub kind: InteractionKind,
    /// The command name or the component `custom_id`.
    pub name: String,
    /// Selected values of a select menu.
    #[serde(default)]
    pub values: Vec<String>,
    /// The command's `options` array as Discord delivered it.
    #[serde(default)]
    pub options: Value,
    /// Whether the interaction happened inside a conversation the agent
    /// participates in, so a conversation scope exists.
    pub conversation: bool,
    /// The provider's markers, as cards are drawn with them.
    pub markers: ActivityMarkers,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InteractionKind {
    Command,
    Component,
}

/// Module to provider: the result of an invocation.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Outcome {
    Description(Description),
    Reply(Reply),
    Card(Card),
    /// The module could not handle the invocation; the provider reports a
    /// generic error to the user and logs the message.
    Failure(String),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Description {
    pub contract: u32,
    /// Discord application command definitions in Discord's own JSON.
    pub commands: Vec<Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Reply {
    pub content: String,
    /// Embeds in Discord's own JSON, validated by the host as a card's embed
    /// is and sent without mentions.
    #[serde(default)]
    pub embeds: Vec<Value>,
    /// Discord message components in Discord's own JSON.
    #[serde(default)]
    pub components: Vec<Value>,
}

/// Module to provider: a native operation request, answered with a
/// [`HostReply`].
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum HostRequest {
    ModelState,
    SetModel {
        scope: Scope,
        /// `None` returns the scope to its default.
        selection: Option<Selection>,
        /// Apply only if the scope still resolves to this selection.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected: Option<Selection>,
    },
    /// The runtime's own commands for the conversation.
    Commands,
    /// Send `text` to the agent as the person's message in the conversation
    /// where the interaction happened, marked with its intent. The answer
    /// arrives in the conversation like any other.
    Submit {
        text: String,
        intent: Intent,
    },
}

/// Why the person submitted text through an interaction rather than writing
/// a message, as the runtime learns it (`native.intent`).
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    /// One of the runtime's own commands, sent verbatim.
    Command,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum HostReply {
    ModelState(ModelState),
    Commands(CommandCatalog),
    /// Core accepted the submission.
    Submitted,
    Error(String),
}

/// A job as its card shows it: what core recorded of it, the latest
/// snapshot the card saw of a turn while it ran, and the provider's markers.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CardRequest {
    pub job: Job,
    /// It stands for its turn's work while a read has no snapshot of the
    /// turn, and after the turn ended without a report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seen: Option<Seen>,
    pub markers: ActivityMarkers,
}

/// The latest snapshot a card saw of one of its job's running turns.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Seen {
    pub turn_id: String,
    pub activity: TurnActivity,
}

/// What a job's card shows.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Card {
    /// One embed in Discord's own JSON, validated by the host and sent without
    /// mentions. Operator modules may use Discord's other embed fields too.
    Show(Value),
    /// Only for a settled job: its card goes, as the job earned no summary.
    Remove,
}

/// Inline card markers, set in the provider's configuration, which
/// validates them; independent of the provider's reaction emoji.
/// Unknown fields are rejected to catch configuration typos; adding a marker
/// field therefore requires a new contract version.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ActivityMarkers {
    pub working: String,
    pub queued: String,
    pub finishing: String,
    pub running: String,
    pub succeeded: String,
    pub failed: String,
    pub uncertain: String,
    pub stopped: String,
    /// Optional brand emotes by model-ID prefix; the longest match wins.
    /// Unmatched models remain plain, rather than guessing their brand.
    pub models: BTreeMap<String, String>,
}

impl ActivityMarkers {
    /// The brand emote of the model with this ID: an operator's explicit
    /// mapping, never inferred, so an unmatched model has none.
    #[must_use]
    pub fn brand(&self, model: &str) -> &str {
        self.models
            .iter()
            .filter(|(prefix, _)| model.starts_with(prefix.as_str()))
            .max_by_key(|(prefix, _)| prefix.len())
            .map_or("", |(_, marker)| marker.as_str())
    }
}

impl Default for ActivityMarkers {
    fn default() -> Self {
        Self {
            working: "⏳".to_owned(),
            queued: "⏳".to_owned(),
            finishing: "⏳".to_owned(),
            running: "↳".to_owned(),
            succeeded: "✓".to_owned(),
            failed: "✗".to_owned(),
            uncertain: "⚠️".to_owned(),
            stopped: "⏹️".to_owned(),
            models: BTreeMap::new(),
        }
    }
}
