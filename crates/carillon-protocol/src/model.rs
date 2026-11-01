//! Wire types shared by core and its providers.

use std::str::FromStr;

use crate::turns::{CONTROL_LIMIT, ControlAction, Silence};
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

impl TurnState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Interrupted => "interrupted",
        }
    }
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
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
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
    /// The interface's channel the conversation belongs to. Core records it
    /// with the conversation's first admitted message.
    #[serde(deserialize_with = "crate::fields::nonempty_string")]
    pub channel_id: String,
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

/// What core recorded of a turn a runtime opened, with the channel core
/// recorded for its conversation.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OpenedTurn {
    #[serde(flatten)]
    pub receipt: EventReceipt,
    #[serde(deserialize_with = "crate::fields::nonempty_string")]
    pub channel_id: String,
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
    /// The channel core recorded for the conversation.
    #[serde(deserialize_with = "crate::fields::nonempty_string")]
    pub channel_id: String,
    pub input: String,
    /// The captured resource snapshot the turn was accepted with, as a
    /// directory the runtime reads in place.
    pub resource_root: String,
    /// Interface-owned metadata captured with the accepted event.
    #[serde(default)]
    pub native: Value,
    /// The directory holding every captured attachment of the installation,
    /// resolved when the turn is handed over; the files' relative paths
    /// resolve against it.
    pub attachment_root: String,
    /// This turn's captured files, in message order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<TurnAttachment>,
}

/// Whether restarting a runtime now would lose something only its process
/// knows. Only the runtime can say; a runtime that gives no such answer has
/// not shown a restart is safe.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum RestartReadiness {
    /// Nothing the runtime holds is lost by a restart. A struct variant, so
    /// an answer that says more than this is refused rather than read as it.
    Ready {},
    /// A restart now would lose what `reason` names.
    Unready { reason: String },
}

/// The most bytes a turn's final report takes, serialized. A runtime keeps
/// its report within it; core stores no larger one.
pub const ACTIVITY_REPORT_LIMIT: usize = 16 * 1024;

/// Display data that is not valid is absent, never an error of what carries it.
pub(crate) fn valid_or_none<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    Ok(Option::<Value>::deserialize(deserializer)?
        .and_then(|value| serde_json::from_value(value).ok()))
}

/// The neutral key of a turn's `native` metadata through which an
/// interface says the input invokes one of the runtime's own commands.
pub const NATIVE_INTENT: &str = "intent";
/// The value of [`NATIVE_INTENT`] for a command invocation.
pub const INTENT_COMMAND: &str = "command";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Delivery {
    /// Outcome of the task, independent of whether this notice is delivered.
    pub turn_state: TurnState,
    pub id: i64,
    pub turn_id: String,
    pub target_id: String,
    pub feedback_target_id: String,
    pub content: String,
    /// Where the turn's work began: the job [`Job`] reads describe.
    pub origin: Origin,
    /// The turn ended with nothing to send; `content` is empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub silence: Option<Silence>,
}

/// Where a turn's work began, which names its job: the turn itself.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Origin {
    pub turn_id: String,
    /// When core accepted that turn, Unix seconds.
    pub accepted_at: i64,
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

/// The most jobs one read names, and the most jobs at work one page lists.
pub const JOB_READ_LIMIT: usize = 32;
/// The most turns module contract 5 lets a job read list of one job.
pub const JOB_TURN_LIMIT: usize = 8;

/// Jobs an interface read: a page of its jobs at work, or the jobs it named.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct JobPage {
    pub jobs: Vec<Job>,
    /// More jobs at work follow: the next page is read after this.
    #[serde(
        default,
        deserialize_with = "crate::fields::optional_nonempty_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub next: Option<String>,
}

/// One job of the requesting interface, as core recorded it: one turn, a
/// message's or one its runtime opened. Different turns are different jobs,
/// even in one conversation.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Job {
    pub origin: Origin,
    /// Where the job's answers go.
    pub target_id: String,
    /// The message that began the job, which feedback is shown on; absent
    /// for a job its runtime opened.
    #[serde(
        default,
        deserialize_with = "crate::fields::optional_nonempty_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub feedback_target_id: Option<String>,
    /// The job's turns, oldest first, at most [`JOB_TURN_LIMIT`]; the last
    /// queued or running is the job at work. Core lists the job's one turn;
    /// module contract 5 keeps the list.
    pub turns: Vec<JobTurn>,
    /// The job has turns before those listed; never, from core.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub earlier: bool,
}

impl Job {
    /// The job's turn queued or running: its newest, when it is at work.
    #[must_use]
    pub fn at_work(&self) -> Option<&JobTurn> {
        self.turns
            .last()
            .filter(|turn| matches!(turn.state, TurnState::Queued | TurnState::Running))
    }

    /// How the job's last turn ended and its answer or notice was sent, once
    /// both are recorded: nothing more will happen to the job.
    #[must_use]
    pub fn settled(&self) -> Option<(TurnState, DeliveryOutcome)> {
        let last = self.turns.last().filter(|turn| turn.ended())?;
        let delivery = match last.delivery? {
            DeliveryState::Delivered => DeliveryOutcome::Delivered,
            DeliveryState::Uncertain => DeliveryOutcome::Uncertain,
            DeliveryState::Pending | DeliveryState::Delivering => return None,
        };
        Some((last.state, delivery))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct JobTurn {
    pub turn_id: String,
    pub state: TurnState,
    /// Unix seconds.
    pub accepted_at: i64,
    /// When the turn ended, Unix seconds.
    #[serde(
        default,
        deserialize_with = "crate::fields::optional_value",
        skip_serializing_if = "Option::is_none"
    )]
    pub finished_at: Option<i64>,
    /// An ended turn's final report, stored with its outcome, or a running
    /// turn's latest snapshot, when its runtime gives one.
    #[serde(
        default,
        deserialize_with = "crate::fields::optional_value",
        skip_serializing_if = "Option::is_none"
    )]
    pub activity: Option<TurnActivity>,
    /// How an ended turn's answer or notice stands. Sending it is separate
    /// from the turn's outcome.
    #[serde(
        default,
        deserialize_with = "crate::fields::optional_value",
        skip_serializing_if = "Option::is_none"
    )]
    pub delivery: Option<DeliveryState>,
}

impl JobTurn {
    #[must_use]
    pub const fn ended(&self) -> bool {
        matches!(self.state, TurnState::Completed | TurnState::Interrupted)
    }
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
        deserialize_with = "crate::fields::optional_nonempty_string",
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
        deserialize_with = "crate::fields::optional_value",
        skip_serializing_if = "Option::is_none"
    )]
    pub work: Option<WorkSummary>,
    /// What the native provider published about its reasoning, as bounded
    /// excerpts, oldest first: never the answer, and never reasoning the
    /// provider did not expose.
    #[serde(
        default,
        deserialize_with = "bounded_notes",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub notes: Vec<String>,
    /// What the native engine reports about the agent running the turn.
    #[serde(
        default,
        deserialize_with = "crate::fields::optional_value",
        skip_serializing_if = "Option::is_none"
    )]
    pub agent: Option<AgentInfo>,
    /// Part of this report was left out to keep it within
    /// [`ACTIVITY_REPORT_LIMIT`]: what it shows is still what the runtime
    /// observed, but not all of it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub partial: bool,
    /// What a person may do with the turn while it waits, each once, so at
    /// most [`CONTROL_LIMIT`]. A runtime's list is read leniently.
    #[serde(
        default,
        deserialize_with = "distinct_controls",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub controls: Vec<ControlAction>,
}

/// The controls a runtime lists that this build knows, each kept once where
/// it first appears: a repeated or unknown one costs the snapshot nothing.
fn distinct_controls<'de, D>(deserializer: D) -> Result<Vec<ControlAction>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Listed {
        Known(ControlAction),
        Unknown(serde::de::IgnoredAny),
    }
    let mut controls = Vec::with_capacity(CONTROL_LIMIT);
    for listed in Vec::<Listed>::deserialize(deserializer)? {
        if let Listed::Known(action) = listed
            && !controls.contains(&action)
        {
            controls.push(action);
        }
    }
    Ok(controls)
}

/// The most reasoning notes one snapshot carries.
pub const NOTE_LIMIT: usize = 8;
/// The longest note, in UTF-16 code units, as JavaScript counts them.
pub const NOTE_LENGTH: usize = 500;

fn bounded_notes<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let notes = Vec::<String>::deserialize(deserializer)?;
    if notes.len() > NOTE_LIMIT {
        return Err(serde::de::Error::custom(format!(
            "at most {NOTE_LIMIT} notes are allowed"
        )));
    }
    if notes
        .iter()
        .any(|note| note.is_empty() || note.encode_utf16().count() > NOTE_LENGTH)
    {
        return Err(serde::de::Error::custom(format!(
            "a note has 1 to {NOTE_LENGTH} characters"
        )));
    }
    Ok(notes)
}

/// What a native engine reports about an agent; each part is absent when
/// the engine does not say.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct AgentInfo {
    /// The model, as the engine names it.
    #[serde(
        default,
        deserialize_with = "crate::fields::optional_nonempty_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub model: Option<String>,
    /// The reasoning effort or thinking level, as the engine names it.
    #[serde(
        default,
        deserialize_with = "crate::fields::optional_nonempty_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub effort: Option<String>,
    /// How much of its context window the agent's conversation fills, as
    /// the engine last estimated it.
    #[serde(
        default,
        deserialize_with = "crate::fields::optional_value",
        skip_serializing_if = "Option::is_none"
    )]
    pub context: Option<ContextUse>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ContextUse {
    /// Tokens the conversation occupies.
    pub used: u64,
    /// The window's size, when the engine says.
    #[serde(
        default,
        deserialize_with = "crate::fields::optional_value",
        skip_serializing_if = "Option::is_none"
    )]
    pub window: Option<u64>,
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
        deserialize_with = "crate::fields::optional_value",
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
    #[serde(deserialize_with = "crate::fields::nonempty_string")]
    pub id: String,
    pub kind: WorkKind,
    #[serde(deserialize_with = "crate::fields::nonempty_string")]
    pub label: String,
    pub state: WorkState,
    /// Another listed item this work was observed under.
    #[serde(
        default,
        deserialize_with = "crate::fields::optional_nonempty_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub parent: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::fields::optional_nonempty_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub actor: Option<String>,
    /// Unix seconds.
    #[serde(
        default,
        deserialize_with = "crate::fields::optional_value",
        skip_serializing_if = "Option::is_none"
    )]
    pub started_at: Option<i64>,
    /// `started_at` is when the runtime first saw the work running: the
    /// engine gave no start of its own.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub estimated: bool,
    /// How long finished work ran, when the engine reported its start and end.
    #[serde(
        default,
        deserialize_with = "crate::fields::optional_value",
        skip_serializing_if = "Option::is_none"
    )]
    pub duration_ms: Option<u64>,
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
    #[serde(deserialize_with = "crate::fields::nonempty_string")]
    pub tool: String,
    #[serde(
        default,
        deserialize_with = "crate::fields::optional_nonempty_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub detail: Option<String>,
    /// Unix seconds.
    pub started_at: i64,
    /// `started_at` is when the runtime first saw the call: the engine gave
    /// no start of its own.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub estimated: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct FinishedTool {
    #[serde(deserialize_with = "crate::fields::nonempty_string")]
    pub tool: String,
    #[serde(
        default,
        deserialize_with = "crate::fields::optional_nonempty_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub detail: Option<String>,
    pub ok: bool,
    /// How long the call ran, when the engine reported its start and end.
    #[serde(
        default,
        deserialize_with = "crate::fields::optional_value",
        skip_serializing_if = "Option::is_none"
    )]
    pub duration_ms: Option<u64>,
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
    /// The configured agents' IDs, in registry order.
    pub agents: Vec<String>,
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

/// Interface-to-core request about an agent's conversation, such as the
/// commands a person may run in it.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ConversationQuery {
    #[serde(deserialize_with = "crate::fields::nonempty_string")]
    pub agent_id: String,
    /// The interface's channel the conversation belongs to.
    #[serde(deserialize_with = "crate::fields::nonempty_string")]
    pub channel_id: String,
    #[serde(
        default,
        deserialize_with = "crate::fields::optional_nonempty_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub audience_id: Option<String>,
}

/// One of the runtime's own commands (or user-invocable skills), as its
/// engine names and describes it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RuntimeCommand {
    /// At most [`COMMAND_NAME_LIMIT`] UTF-16 code units, as JavaScript counts them.
    #[serde(deserialize_with = "command_name")]
    pub name: String,
    #[serde(
        default,
        deserialize_with = "crate::fields::optional_nonempty_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub description: Option<String>,
    /// The arguments the engine expects, when it says.
    #[serde(
        default,
        deserialize_with = "crate::fields::optional_nonempty_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub hint: Option<String>,
}

/// The longest command name a runtime may list, which an interface can
/// carry whole in its own controls.
pub const COMMAND_NAME_LIMIT: usize = 100;

fn command_name<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let name = crate::fields::nonempty_string(deserializer)?;
    if name.encode_utf16().count() > COMMAND_NAME_LIMIT {
        return Err(serde::de::Error::custom(format!(
            "a command name has at most {COMMAND_NAME_LIMIT} characters"
        )));
    }
    Ok(name)
}

/// The commands a person may run in a conversation, in the runtime's order.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct CommandCatalog {
    pub commands: Vec<RuntimeCommand>,
}
