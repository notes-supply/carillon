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
    pub guild_id: Option<String>,
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
    pub root: String,
    pub system_sha256: String,
    pub context_sha256: String,
    pub settings_sha256: String,
    pub extensions_sha256: String,
    pub skills_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SelectedComponent {
    pub id: String,
    pub role: String,
    pub version: String,
    pub url: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CapabilitySelection {
    pub component: SelectedComponent,
    pub token: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeTurnRequest {
    pub correlation_id: String,
    pub agent_id: String,
    pub audience_id: String,
    pub input: String,
    pub continuation_ref: Option<String>,
    pub resources: ResourceSnapshot,
    pub capability: Option<CapabilitySelection>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeTurnResponse {
    pub correlation_id: String,
    pub output: String,
    pub continuation_ref: String,
    pub native: Value,
    #[serde(default)]
    pub capability_activity: Vec<Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeEvidence {
    pub correlation_id: String,
    pub state: String,
    pub error: Option<String>,
    pub result: Option<RuntimeTurnResponse>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeExecutionProfileModel {
    pub key: String,
    pub provider: String,
    pub provider_name: String,
    pub id: String,
    pub name: String,
    pub reasoning: Vec<String>,
    pub service_tiers: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExecutionProfile {
    pub model: String,
    pub reasoning: Option<String>,
    pub service_tier: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExecutionProfileEvidence {
    pub requested: ExecutionProfile,
    pub effective: Option<ExecutionProfile>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeExecutionProfiles {
    pub models: Vec<RuntimeExecutionProfileModel>,
    pub selected: Option<ExecutionProfileEvidence>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionProfileScope {
    CurrentConversation,
    NewConversations,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExecutionProfileSelection {
    pub key: String,
    pub reasoning: Option<String>,
    pub service_tier: Option<String>,
    pub scope: ExecutionProfileScope,
    pub audience_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeExecutionProfileSelection {
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    pub scope: ExecutionProfileScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continuation_ref: Option<String>,
    pub agent_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audience_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceSnapshot>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExecutionProfileApplication {
    pub scope: ExecutionProfileScope,
    pub requested: ExecutionProfile,
    pub effective: ExecutionProfile,
    pub continuation_ref: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExtensionRequest {
    pub correlation_id: String,
    pub command: String,
    pub arguments: String,
    pub guild_id: String,
    pub user_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExtensionResponse {
    pub content: String,
    pub native: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CapabilityRequest {
    pub correlation_id: String,
    pub operation: String,
    pub input: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CapabilityResponse {
    pub output: String,
    pub native: Value,
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
    pub components: Value,
    pub resources: ResourceSnapshot,
    pub native_evidence: Option<Value>,
    pub capability_activity: Vec<Value>,
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
    pub components: Vec<Health>,
    pub turns: StateCounts,
    pub deliveries: StateCounts,
    pub latest_backup: Option<String>,
    pub recent_turns: Vec<TurnSummary>,
    pub execution_profile: Option<ExecutionProfileEvidence>,
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
