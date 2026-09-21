//! Shared model-control vocabulary; option meanings belong to the runtime.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The runtime's view of model availability and selection, relayed
/// unchanged by the provider and core.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ModelState {
    pub available: Vec<AvailableModel>,
    pub agent: ScopeState,
    /// Absent outside a conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<ScopeState>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct AvailableModel {
    pub provider: String,
    pub model: String,
    pub name: String,
    #[serde(default)]
    pub options: Vec<ModelOption>,
}

/// One engine setting the runtime supports for a model, with the choices it
/// accepts.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ModelOption {
    pub id: String,
    pub name: String,
    pub choices: Vec<String>,
    /// Used when no scope chose a supported value.
    pub default: String,
}

/// The effective selection for one scope and whether it was set at that
/// scope or inherited.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ScopeState {
    #[serde(default)]
    pub selection: Option<Selection>,
    #[serde(default)]
    pub overridden: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Selection {
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub options: BTreeMap<String, String>,
}
