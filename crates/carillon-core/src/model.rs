use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    pub state: String,
    pub duplicate: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
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

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryOutcome {
    Delivered,
    Uncertain,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TurnInspection {
    pub turn_id: String,
    pub event_id: String,
    pub audience_id: String,
    pub user_id: String,
    pub input: String,
    pub state: String,
    pub output: Option<String>,
    pub providers: Value,
    pub resources: ResourceSnapshot,
    pub delivery_state: Option<String>,
    pub delivery_native_ref: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TurnSummary {
    pub turn_id: String,
    pub event_id: String,
    pub state: String,
    pub started_at: i64,
    pub delivery_state: Option<String>,
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
