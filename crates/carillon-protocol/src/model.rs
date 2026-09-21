//! Wire types shared by core and its providers.

use std::str::FromStr;

use crate::models::Selection;
use anyhow::bail;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Header naming the configured provider on provider-to-core requests.
pub const PROVIDER_HEADER: &str = "x-carillon-provider";

/// Header naming the interface provider process instance that claims
/// deliveries. A fresh instance identity per process start lets core release
/// claims that a dead instance left behind.
pub const INSTANCE_HEADER: &str = "x-carillon-instance";

/// Lifecycle of an accepted turn.
///
/// `Interrupted` ends an incomplete turn without replaying it or blocking
/// the next user message. Tool side effects may already have occurred.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnState {
    Queued,
    Running,
    Completed,
    Interrupted,
}

impl FromStr for TurnState {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(match value {
            "queued" => Self::Queued,
            "running" => Self::Running,
            "completed" => Self::Completed,
            "interrupted" => Self::Interrupted,
            other => bail!("unknown turn state {other}"),
        })
    }
}

/// Lifecycle of a delivery intent.
///
/// `Uncertain` records that the interface may or may not have sent the
/// content; an unclaimed `Pending` delivery is safe to claim again.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    Pending,
    Delivering,
    Delivered,
    Uncertain,
}

impl DeliveryState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Delivering => "delivering",
            Self::Delivered => "delivered",
            Self::Uncertain => "uncertain",
        }
    }
}

impl FromStr for DeliveryState {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(match value {
            "pending" => Self::Pending,
            "delivering" => Self::Delivering,
            "delivered" => Self::Delivered,
            "uncertain" => Self::Uncertain,
            other => bail!("unknown delivery state {other}"),
        })
    }
}

/// The two outcomes an interface may report for a claimed delivery, so an ack
/// cannot express a non-terminal state.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryOutcome {
    Delivered,
    Uncertain,
}

impl From<DeliveryOutcome> for DeliveryState {
    fn from(outcome: DeliveryOutcome) -> Self {
        match outcome {
            DeliveryOutcome::Delivered => Self::Delivered,
            DeliveryOutcome::Uncertain => Self::Uncertain,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Health {
    pub id: String,
    pub role: String,
    pub version: String,
    pub status: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExternalEvent {
    pub event_id: String,
    pub agent_id: String,
    pub user_id: String,
    pub audience_id: String,
    pub delivery_target_id: String,
    pub feedback_target_id: String,
    pub content: String,
    #[serde(default)]
    pub native: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EventReceipt {
    pub event_id: String,
    pub turn_id: String,
    pub state: TurnState,
    pub duplicate: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SelectedProvider {
    pub id: String,
    pub role: String,
    pub version: String,
    pub url: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeTurnRequest {
    pub correlation_id: String,
    pub interface_id: String,
    pub agent_id: String,
    pub audience_id: String,
    pub input: String,
    /// The captured resource snapshot the turn was accepted with, as a
    /// directory the runtime reads in place.
    pub resource_root: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeTurnResponse {
    pub correlation_id: String,
    pub output: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Delivery {
    /// Outcome of the task, independent of whether this notice is delivered.
    pub turn_state: TurnState,
    pub id: i64,
    pub turn_id: String,
    pub target_id: String,
    pub feedback_target_id: String,
    pub content: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DeliveryReconciliation {
    pub feedback_target_ids: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DeliveryAck {
    pub native_ref: Option<String>,
    pub outcome: DeliveryOutcome,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TurnInspection {
    pub turn_id: String,
    pub event_id: String,
    pub audience_id: String,
    pub user_id: String,
    pub input: String,
    pub state: TurnState,
    pub output: Option<String>,
    pub providers: Value,
    pub resource_snapshot_id: String,
    pub delivery_state: Option<DeliveryState>,
    pub delivery_native_ref: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TurnSummary {
    pub turn_id: String,
    pub event_id: String,
    pub state: TurnState,
    pub accepted_at: i64,
    pub delivery_state: Option<DeliveryState>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StatusReport {
    pub release: String,
    pub agent_id: String,
    pub providers: Vec<Health>,
    pub turns: StateCounts,
    pub deliveries: StateCounts,
    pub recent_turns: Vec<TurnSummary>,
}

/// Operator-facing tally using one vocabulary for turns and deliveries.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct StateCounts {
    pub pending: u64,
    pub active: u64,
    pub completed: u64,
    pub interrupted: u64,
    pub uncertain: u64,
}

/// Scope of a model selection: the agent default or one conversation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelScope {
    Agent,
    Conversation,
}

/// Interface-to-core request for the model state of an agent and, when the
/// interaction happens inside a conversation, that conversation's audience.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ModelQuery {
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience_id: Option<String>,
}

/// Interface-to-core request to change a model selection. `selection` is the
/// shared model vocabulary; `None` returns the scope to its default.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ModelUpdate {
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience_id: Option<String>,
    pub scope: ModelScope,
    pub selection: Option<Selection>,
    /// Runtime-owned precondition for read-modify-write controls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<Selection>,
}

/// Core-to-runtime form of [`ModelQuery`]; core adds the interface identity
/// that names the conversation together with agent and audience.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeModelQuery {
    pub interface_id: String,
    #[serde(flatten)]
    pub query: ModelQuery,
}

/// Core-to-runtime form of [`ModelUpdate`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeModelUpdate {
    pub interface_id: String,
    #[serde(flatten)]
    pub update: ModelUpdate,
}
