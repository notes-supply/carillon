//! Wire types shared by core and its providers.

use std::str::FromStr;

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
/// `Uncertain` records that Carillon cannot prove whether runtime work ran;
/// only an operator moves such a turn on, to `Queued` (retry) or `Skipped`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnState {
    Queued,
    Running,
    Completed,
    Uncertain,
    Skipped,
}

impl FromStr for TurnState {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(match value {
            "queued" => Self::Queued,
            "running" => Self::Running,
            "completed" => Self::Completed,
            "uncertain" => Self::Uncertain,
            "skipped" => Self::Skipped,
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

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ResourceSnapshot {
    pub id: String,
    pub system_sha256: String,
    pub context_sha256: String,
    pub settings_sha256: String,
    pub extensions_sha256: String,
    pub skills_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeResources {
    pub root: String,
    pub system_sha256: String,
    pub context_sha256: String,
    pub settings_sha256: String,
    pub extensions_sha256: String,
    pub skills_sha256: String,
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
    pub continuation_ref: Option<String>,
    pub resources: RuntimeResources,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeTurnResponse {
    pub correlation_id: String,
    pub output: String,
    pub continuation_ref: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Delivery {
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
    pub resources: ResourceSnapshot,
    pub delivery_state: Option<DeliveryState>,
    pub delivery_native_ref: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TurnSummary {
    pub turn_id: String,
    pub event_id: String,
    pub state: TurnState,
    pub started_at: i64,
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

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SimulateRequest {
    pub event_id: String,
    pub guild_id: Option<String>,
    pub user_id: String,
    pub channel_id: String,
    #[serde(default)]
    pub parent_channel_id: Option<String>,
    pub content: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SimulateResponse {
    pub receipt: EventReceipt,
    pub delivery: Option<Delivery>,
    pub conversation_id: String,
    pub feedback: Vec<String>,
}
