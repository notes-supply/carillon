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
    #[serde(
        default,
        deserialize_with = "optional_value",
        skip_serializing_if = "Option::is_none"
    )]
    pub conversation: Option<ScopeState>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct AvailableModel {
    pub provider: String,
    pub model: String,
    pub name: String,
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
    #[serde(deserialize_with = "required_nullable")]
    pub selection: Option<Selection>,
    pub overridden: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Selection {
    #[serde(deserialize_with = "nonempty_string")]
    pub provider: String,
    #[serde(deserialize_with = "nonempty_string")]
    pub model: String,
    #[serde(default)]
    pub options: BTreeMap<String, String>,
}

// Serde otherwise treats Option fields as optional even without #[serde(default)].
pub(crate) fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

pub(crate) fn optional_value<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

pub(crate) fn nonempty_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    if value.is_empty() {
        return Err(serde::de::Error::custom("expected a nonempty string"));
    }
    Ok(value)
}

pub(crate) fn optional_nonempty_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    nonempty_string(deserializer).map(Some)
}
