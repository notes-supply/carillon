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
    /// The person's text; may be empty when the event carries attachments.
    pub content: String,
    #[serde(default)]
    pub native: Value,
    /// Files the interface staged for core to capture with this event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachments: Option<StagedAttachments>,
}

/// Files an interface staged in its own staging directory of the
/// installation. The event names a batch, never a path: core derives where
/// the batch lives from the authenticated interface.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct StagedAttachments {
    /// The batch directory the interface created for this event.
    pub batch: String,
    /// In message order; file `i` is staged as `<batch>/<i>`.
    pub files: Vec<StagedFile>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct StagedFile {
    /// The uploader's name for the file: display text, never a path.
    pub name: String,
    pub size: u64,
    /// The media type the platform declared, unverified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
}

/// A file core captured with an accepted turn.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TurnAttachment {
    /// Where the file is, relative to the installation's current attachment
    /// root; never an absolute path.
    pub path: String,
    /// The uploader's name for the file: display text, never a path.
    pub name: String,
    pub size: u64,
    /// The media type the platform declared, unverified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// Lowercase hexadecimal SHA-256 of the captured bytes.
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EventReceipt {
    pub event_id: String,
    pub turn_id: String,
    pub state: TurnState,
    pub duplicate: bool,
}

/// What core knows about an event an interface submitted.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum EventStatus {
    /// Accepted; the receipt says which turn it became.
    Accepted(EventReceipt),
    /// Core is still capturing the event's attachments; the outcome will
    /// follow.
    Capturing,
    /// Neither accepted nor being captured now.
    Unknown,
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
    /// Interface-owned metadata captured with the accepted event.
    #[serde(default)]
    pub native: Value,
    /// The directory holding every captured attachment of the installation,
    /// resolved when the turn is claimed. Earlier turns' relative paths
    /// resolve against it too, so it is sent with every turn.
    pub attachment_root: String,
    /// This turn's captured files, in message order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<TurnAttachment>,
    /// Files of earlier turns in this conversation that no answered turn has
    /// carried yet, oldest first: the runtime's conversation may never have
    /// recorded them, so the runtime makes sure it knows them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unanswered: Vec<UnansweredTurn>,
}

/// An earlier turn's captured files, offered again to a later turn.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct UnansweredTurn {
    pub correlation_id: String,
    pub attachments: Vec<TurnAttachment>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeTurnResponse {
    pub correlation_id: String,
    pub output: String,
    /// Native work linked to this turn will continue the conversation after
    /// this answer. Core queues a follow-up turn for the same conversation
    /// whose request carries this runtime-owned cursor in `native.follow_up`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_up: Option<Value>,
}

/// The neutral key of a turn's `native` metadata through which an
/// interface says the input invokes one of the runtime's own commands.
pub const NATIVE_INTENT: &str = "intent";
/// The value of [`NATIVE_INTENT`] for a command invocation.
pub const INTENT_COMMAND: &str = "command";
/// The neutral key of a turn's `native` metadata that only core sets, on a
/// turn it queued itself, to carry a runtime's follow-up cursor.
pub const NATIVE_FOLLOW_UP: &str = "follow_up";

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

/// A queued or running turn of the requesting interface, as core reports it
/// for display while the turn is in progress. `activity` is the owning
/// runtime's latest snapshot for a running turn, or `None` when core has none.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ActiveTurn {
    pub turn_id: String,
    pub state: TurnState,
    pub target_id: String,
    /// Unix seconds.
    pub accepted_at: i64,
    #[serde(deserialize_with = "crate::models::required_nullable")]
    pub activity: Option<TurnActivity>,
}

/// What a runtime is doing in one executing turn. The runtime formats each
/// `detail` from its own tool semantics; consumers show it as given and
/// never receive raw tool arguments.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TurnActivity {
    pub phase: ActivityPhase,
    /// The model's own latest public commentary, when it wrote any.
    #[serde(
        default,
        deserialize_with = "crate::models::optional_nonempty_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub headline: Option<String>,
    pub running: Vec<RunningTool>,
    /// The most recently finished calls, oldest first.
    pub recent: Vec<FinishedTool>,
    /// Every call started in the turn, including those no longer listed.
    pub tool_calls: u32,
    /// Delegated and transferred work, when the runtime observes any.
    #[serde(
        default,
        deserialize_with = "crate::models::optional_value",
        skip_serializing_if = "Option::is_none"
    )]
    pub work: Option<WorkSummary>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActivityPhase {
    Starting,
    /// The runtime accepted the turn, which waits behind other work on its
    /// native conversation.
    Queued,
    Model,
    Tools,
    /// The turn's native work waits on something outside the model.
    Waiting,
    Retrying,
    Compacting,
}

/// The most work items one snapshot lists.
pub const WORK_ITEM_LIMIT: usize = 16;

/// A turn's related work, bounded. `partial` says the runtime could not
/// observe all of it; `omitted` is an exact count of further current items
/// when the runtime knows one.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct WorkSummary {
    #[serde(deserialize_with = "bounded_items")]
    pub items: Vec<WorkItem>,
    pub partial: bool,
    #[serde(
        default,
        deserialize_with = "crate::models::optional_value",
        skip_serializing_if = "Option::is_none"
    )]
    pub omitted: Option<std::num::NonZeroU32>,
}

/// One unit of native work related to a turn that is not a tool call. The
/// runtime keeps `id` stable while it observes the work and states only what
/// the native engine last confirmed. `label` and `actor` are bounded display
/// text, never prompts, results or arguments.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct WorkItem {
    #[serde(deserialize_with = "crate::models::nonempty_string")]
    pub id: String,
    pub kind: WorkKind,
    #[serde(deserialize_with = "crate::models::nonempty_string")]
    pub label: String,
    pub state: WorkState,
    /// Another listed item this work was observed under.
    #[serde(
        default,
        deserialize_with = "crate::models::optional_nonempty_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub parent: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::models::optional_nonempty_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub actor: Option<String>,
    /// Unix seconds.
    #[serde(
        default,
        deserialize_with = "crate::models::optional_value",
        skip_serializing_if = "Option::is_none"
    )]
    pub started_at: Option<i64>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkKind {
    /// Other tracked work.
    Task,
    /// A child agent the turn delegated to.
    Delegation,
    /// The conversation's context transferred to another model or provider.
    Handoff,
    /// A branch of the conversation into another one.
    Fork,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkState {
    Queued,
    Running,
    Waiting,
    Succeeded,
    Failed,
    Cancelled,
    /// The runtime can no longer confirm this work's state.
    Unknown,
}

fn bounded_items<'de, D>(deserializer: D) -> Result<Vec<WorkItem>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let items = Vec::<WorkItem>::deserialize(deserializer)?;
    if items.len() > WORK_ITEM_LIMIT {
        return Err(serde::de::Error::custom(format!(
            "at most {WORK_ITEM_LIMIT} work items are allowed"
        )));
    }
    Ok(items)
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RunningTool {
    #[serde(deserialize_with = "crate::models::nonempty_string")]
    pub tool: String,
    #[serde(
        default,
        deserialize_with = "crate::models::optional_nonempty_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub detail: Option<String>,
    /// Unix seconds.
    pub started_at: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct FinishedTool {
    #[serde(deserialize_with = "crate::models::nonempty_string")]
    pub tool: String,
    #[serde(
        default,
        deserialize_with = "crate::models::optional_nonempty_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub detail: Option<String>,
    pub ok: bool,
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
    /// The files captured with the turn, in message order.
    #[serde(default)]
    pub attachments: Vec<TurnAttachment>,
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
    #[serde(deserialize_with = "crate::models::nonempty_string")]
    pub agent_id: String,
    #[serde(
        default,
        deserialize_with = "crate::models::optional_nonempty_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub audience_id: Option<String>,
}

/// Interface-to-core request to change a model selection. `selection` is the
/// shared model vocabulary; `None` returns the scope to its default.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ModelUpdate {
    #[serde(deserialize_with = "crate::models::nonempty_string")]
    pub agent_id: String,
    #[serde(
        default,
        deserialize_with = "crate::models::optional_nonempty_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub audience_id: Option<String>,
    pub scope: ModelScope,
    #[serde(deserialize_with = "crate::models::required_nullable")]
    pub selection: Option<Selection>,
    /// Runtime-owned precondition for read-modify-write controls.
    #[serde(
        default,
        deserialize_with = "crate::models::optional_value",
        skip_serializing_if = "Option::is_none"
    )]
    pub expected: Option<Selection>,
}

/// Core-to-runtime form of [`ModelQuery`]; core adds the interface identity
/// that names the conversation together with agent and audience.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeModelQuery {
    #[serde(deserialize_with = "crate::models::nonempty_string")]
    pub interface_id: String,
    #[serde(flatten)]
    pub query: ModelQuery,
}

/// One of the runtime's own commands (or user-invocable skills), as its
/// engine names and describes it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RuntimeCommand {
    #[serde(deserialize_with = "crate::models::nonempty_string")]
    pub name: String,
    #[serde(
        default,
        deserialize_with = "crate::models::optional_nonempty_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub description: Option<String>,
    /// The arguments the engine expects, when it says.
    #[serde(
        default,
        deserialize_with = "crate::models::optional_nonempty_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub hint: Option<String>,
}

/// The commands a person may run in a conversation, in the runtime's order.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct CommandCatalog {
    pub commands: Vec<RuntimeCommand>,
}

/// Core-to-runtime form of [`ModelUpdate`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeModelUpdate {
    #[serde(deserialize_with = "crate::models::nonempty_string")]
    pub interface_id: String,
    #[serde(flatten)]
    pub update: ModelUpdate,
}
