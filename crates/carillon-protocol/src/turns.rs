//! Turns a durable runtime holds: how it settles them and opens its own, and
//! the controls an interface relays to it while one waits.

use std::str::FromStr;

use anyhow::bail;
use serde::{Deserialize, Serialize};

use crate::model::{TurnActivity, valid_or_none};

/// How a durable runtime says a turn it holds ended. Core settles the turn
/// and its delivery from it once; the same outcome again is answered as
/// recorded.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum TurnOutcome {
    /// The turn's answer, sent to its conversation.
    Answered {
        #[serde(deserialize_with = "nonblank")]
        output: String,
        /// The runtime's final report; one that is not valid is dropped.
        #[serde(
            default,
            deserialize_with = "valid_or_none",
            skip_serializing_if = "Option::is_none"
        )]
        activity: Option<TurnActivity>,
    },
    /// The turn did not complete. `explanation` is phrased for the
    /// conversation and safe to display.
    Failed {
        #[serde(
            default,
            deserialize_with = "crate::models::optional_nonempty_string",
            skip_serializing_if = "Option::is_none"
        )]
        explanation: Option<String>,
        #[serde(
            default,
            deserialize_with = "valid_or_none",
            skip_serializing_if = "Option::is_none"
        )]
        activity: Option<TurnActivity>,
    },
    /// The message was steered into the turn running before it, whose answer
    /// covers it: nothing is sent for it.
    Steered {},
    /// Someone stopped the turn: nothing is sent for it.
    Stopped {},
}

fn nonblank<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    if value.trim().is_empty() {
        return Err(serde::de::Error::custom("expected a nonblank string"));
    }
    Ok(value)
}

/// Why a delivery has nothing to send: its turn was steered into another,
/// or stopped. The interface shows it on the message instead.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Silence {
    Steered,
    Stopped,
}

impl Silence {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Steered => "steered",
            Self::Stopped => "stopped",
        }
    }
}

impl FromStr for Silence {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(match value {
            "steered" => Self::Steered,
            "stopped" => Self::Stopped,
            other => bail!("unknown silence {other}"),
        })
    }
}

/// A turn a durable runtime opens in an existing conversation of its own,
/// for a schedule or a subagent's report. `key` makes it idempotent: the
/// same key opens one turn, and another conversation using it is refused.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TurnOpening {
    #[serde(deserialize_with = "crate::models::nonempty_string")]
    pub interface_id: String,
    #[serde(deserialize_with = "crate::models::nonempty_string")]
    pub agent_id: String,
    #[serde(deserialize_with = "crate::models::nonempty_string")]
    pub audience_id: String,
    #[serde(deserialize_with = "crate::models::nonempty_string")]
    pub key: String,
    /// What the turn is for, recorded as its input.
    #[serde(
        default,
        deserialize_with = "crate::models::optional_nonempty_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub label: Option<String>,
}

/// What a person may do with a message that waits behind a running turn.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlAction {
    /// Hand the message to the running turn now.
    Steer,
    /// End the running turn and run the message next.
    Interrupt,
    /// Run the message not at all.
    Stop,
}

impl ControlAction {
    pub const ALL: [Self; 3] = [Self::Steer, Self::Interrupt, Self::Stop];
}

/// The most controls one activity snapshot offers: each action once.
pub const CONTROL_LIMIT: usize = ControlAction::ALL.len();

/// An interface's request to control the turn of an event it submitted.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TurnControl {
    pub action: ControlAction,
}

/// Core's relay of a [`TurnControl`] to the runtime holding the turn,
/// naming the interface it serves.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RuntimeTurnControl {
    #[serde(deserialize_with = "crate::models::nonempty_string")]
    pub interface_id: String,
    pub action: ControlAction,
}
