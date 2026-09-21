//! The JSON exchanged between the Discord provider and its command module.
//! Every message is an externally tagged enum; unknown fields are ignored so
//! either side can grow without breaking the other within one contract
//! version.

use carillon_protocol::{
    model::ModelScope as Scope,
    models::{ModelState, Selection},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Contract version the module speaks; the provider refuses others.
pub const CONTRACT: u32 = 2;

/// Provider to module: the invocation payload.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Invocation {
    /// Ask for the module's contract version and command definitions.
    Describe,
    /// Handle an admitted interaction. Replies are ephemeral; a component
    /// interaction's reply updates the message carrying the component.
    Interaction(Interaction),
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
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum HostReply {
    ModelState(ModelState),
    Error(String),
}
