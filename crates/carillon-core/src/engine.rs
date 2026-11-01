//! Admission, runtime ownership, hand-over and delivery over one SQLite authority.
mod dispatch;
mod ownership;
mod settle;

use crate::{
    attachments::{self, Capture, Reconciled, Unusable},
    db::{Admission, Database, JobPage, QueuedTurn},
    intake::Intake,
    resources,
};
use anyhow::{Result, anyhow};
use carillon_installation::config::AttachmentLimits;
use carillon_protocol::model::{
    ACTIVITY_REPORT_LIMIT, ConversationQuery, Delivery, DeliveryAck, DeliveryReconciliation,
    EventReceipt, EventStatus, ExternalEvent, JOB_READ_LIMIT, Job, RuntimeTurnRequest,
    SelectedProvider, StateCounts, TurnActivity, TurnInspection, TurnSummary,
};
pub use dispatch::{InboxFailure, Stalled};
use ownership::{Owner, stranded};
use std::{path::PathBuf, sync::Arc};

/// What admission decided before any capture work.
enum Admitted {
    Settled(EventReceipt),
    /// Attachments to capture for a current agent before the turn is recorded.
    Capturing(AgentOptions),
}
use tokio::sync::watch;
use tokio_util::task::TaskTracker;
use tracing::warn;

/// Runtime identity and availability, without credentials or a transport implementation.
#[derive(Clone, Debug)]
pub struct RuntimeProvider {
    pub id: String,
    pub version: String,
    pub url: String,
    pub enabled: bool,
}

/// An agent conversations are admitted for: the runtime its new
/// conversations run on and the resources its turns capture.
#[derive(Clone, Debug)]
pub struct AgentOptions {
    pub id: String,
    pub runtime: String,
    /// `None` captures an empty resource snapshot: the runtime's own
    /// resources alone apply.
    pub resources: Option<PathBuf>,
}

/// Application-supplied policy and owned storage locations.
#[derive(Clone, Debug)]
pub struct CoreOptions {
    pub data_directory: PathBuf,
    pub agents: Vec<AgentOptions>,
    /// The runtimes core starts with; [`Core::set_runtimes`] replaces them
    /// while it serves.
    pub runtimes: Vec<RuntimeProvider>,
    pub attachments: AttachmentLimits,
}

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("{0}")]
    InvalidInput(String),
    #[error("{0}")]
    Conflict(String),
    /// Not now: retrying the same submission later may succeed.
    #[error("{0}")]
    Unavailable(String),
    /// The attachment storage budget or free space would be exceeded.
    #[error("{0}")]
    InsufficientStorage(String),
    #[error(transparent)]
    Storage(#[from] anyhow::Error),
}

impl CoreError {
    /// The same refusal for another waiter on a shared outcome.
    pub(crate) fn share(&self) -> Self {
        match self {
            Self::InvalidInput(message) => Self::InvalidInput(message.clone()),
            Self::Conflict(message) => Self::Conflict(message.clone()),
            Self::Unavailable(message) => Self::Unavailable(message.clone()),
            Self::InsufficientStorage(message) => Self::InsufficientStorage(message.clone()),
            Self::Storage(error) => Self::Storage(anyhow!("{error:#}")),
        }
    }
}

/// An attachment that no longer matches its capture runs nothing.
const ALTERED_ATTACHMENT: &str = "a file attached to that message was changed or removed after \
Carillon accepted it, so the message was not run";
/// Nor does one that cannot be read when the turn starts.
const UNREADABLE_ATTACHMENT: &str = "a file attached to that message could not be read, so the \
message was not run";

/// A core handle does not spawn workers or initialize any provider.
/// Clones share the same SQLite authority. The application owns the exclusive worker lifetime.
#[derive(Clone)]
pub struct Core {
    pub(crate) database: Database,
    /// Fixed for the handle's life; its runtimes live in `runtimes`.
    pub(crate) options: CoreOptions,
    /// The runtimes work is admitted for and run by, as the application last
    /// derived them.
    runtimes: Arc<watch::Sender<Arc<[RuntimeProvider]>>>,
    pub(crate) intake: Arc<Intake>,
    /// Marked changed whenever this handle may have queued a turn.
    work: Arc<watch::Sender<()>>,
    /// Storage work running on Tokio's blocking pool for an async caller.
    storage: TaskTracker,
}

impl Core {
    /// Create fresh operation storage. Does not replace an existing database.
    /// # Errors
    /// Returns invalid configuration or storage errors.
    pub fn create(options: CoreOptions) -> Result<Self> {
        validate_options(&options)?;
        let database = Database::create(&options.data_directory.join("carillon.sqlite3"))?;
        Ok(Self::assemble(database, options))
    }

    /// Open existing operation storage without creating it or recovering running turns.
    /// # Errors
    /// Returns invalid configuration or storage/schema errors.
    pub fn open(options: CoreOptions) -> Result<Self> {
        validate_options(&options)?;
        let database = Database::open(&options.data_directory.join("carillon.sqlite3"))?;
        Ok(Self::assemble(database, options))
    }

    fn assemble(database: Database, mut options: CoreOptions) -> Self {
        let runtimes = std::mem::take(&mut options.runtimes);
        Self {
            intake: Arc::new(Intake::new(options.attachments)),
            database,
            options,
            runtimes: Arc::new(watch::Sender::new(runtimes.into())),
            work: Arc::new(watch::Sender::new(())),
            storage: TaskTracker::new(),
        }
    }

    /// This handle's storage with other options, as a restart with them
    /// would serve it.
    #[cfg(test)]
    pub(crate) fn reconfigured(&self, mut options: CoreOptions) -> Self {
        let runtimes = std::mem::take(&mut options.runtimes);
        Self {
            options,
            runtimes: Arc::new(watch::Sender::new(runtimes.into())),
            ..self.clone()
        }
    }

    /// Runs storage work that may block on Tokio's blocking pool and waits
    /// for it to settle: SQLite writes, which wait up to five seconds for
    /// another writer, and resource snapshots, which read, hash and sync
    /// files. An async caller's worker thread meanwhile serves other
    /// requests. Plain reads need not come here: in WAL mode a reader never
    /// waits for the writer. The work finishes even if the caller stops
    /// waiting, and its outcome is the database's, as for a synchronous call.
    /// # Errors
    /// Returns the work's error, or a storage error if it panicked.
    pub async fn blocking<T, E>(
        &self,
        work: impl FnOnce(&Self) -> Result<T, E> + Send + 'static,
    ) -> Result<T, E>
    where
        T: Send + 'static,
        E: From<anyhow::Error> + Send + 'static,
    {
        let core = self.clone();
        self.storage
            .spawn_blocking(move || work(&core))
            .await
            .map_err(|error| E::from(anyhow!(error).context("storage work did not finish")))?
    }

    /// Wait until storage work started for async callers has finished, so
    /// exclusive ownership can be released with no write still running.
    pub async fn storage_settled(&self) {
        self.storage.close();
        self.storage.wait().await;
    }

    /// Changes whenever a turn may have been queued through this handle or
    /// its clones, or the runtimes changed. A dispatcher that finds nothing
    /// to hand over waits for the next change rather than polling; marking
    /// the receiver unchanged before each look means no change is missed.
    #[must_use]
    pub fn work(&self) -> watch::Receiver<()> {
        self.work.subscribe()
    }

    fn wake(&self) {
        self.work.send_replace(());
    }

    /// Recover running turns, then interrupt queued turns whose agent moved or was removed.
    /// Call only after acquiring exclusive worker ownership; opening another handle is not a restart.
    /// # Errors
    /// Returns storage errors.
    pub fn recover_interrupted(&self) -> Result<usize> {
        let runtimes = self.runtimes.borrow().clone();
        let mut recovered = self.database.recover_running_turns(
            &runtimes
                .iter()
                .map(|runtime| runtime.id.as_str())
                .collect::<Vec<_>>(),
        )?;
        for (agent_id, runtime_id) in self.database.queued_owners()? {
            if self
                .agent(&agent_id)
                .is_ok_and(|agent| agent.runtime == runtime_id)
            {
                continue;
            }
            let explanation = stranded(&runtime_id, &agent_id, None);
            recovered += self
                .database
                .interrupt_queued(&agent_id, &runtime_id, &explanation)?;
        }
        Ok(recovered)
    }

    /// Remove abandoned capture copies and captures no committed turn
    /// references. Call only with exclusive ownership of the state, before
    /// accepting events: an acceptance in progress elsewhere would lose its
    /// capture.
    /// # Errors
    /// Returns storage errors.
    pub fn reconcile_attachments(&self) -> Result<Reconciled> {
        attachments::reconcile(&self.options.data_directory, &self.database.captures()?)
    }

    /// Ask captures in progress to stop at their next chunk, for shutdown.
    /// Their submissions are refused as unavailable and may be retried.
    pub fn stop_capturing(&self) {
        self.intake.stop();
    }

    /// Wait until every capture in progress has returned and settled, so
    /// exclusive ownership can be released without a copy still running.
    pub async fn captures_settled(&self) {
        self.intake.settled().await;
    }

    pub(crate) fn snapshot_store(&self) -> PathBuf {
        self.options.data_directory.join("resource-snapshots")
    }

    /// Admit an event from an already authenticated interface, before acknowledging it.
    /// Duplicate receipts do not recapture mutable resources or reconsider runtime ownership.
    ///
    /// An executable event with attachments is accepted only once all its
    /// staged files are captured and published. Stranded conversations settle
    /// with their explanation without capturing attachments. The capture
    /// belongs to core: it continues if the caller goes away, a retry of
    /// the same event waits for it instead of copying again, and at most the
    /// configured number run at once.
    /// # Errors
    /// Returns invalid input, unavailable runtime ownership, busy or full
    /// attachment storage, or storage/resource errors.
    pub async fn accept(
        &self,
        interface: &SelectedProvider,
        event: &ExternalEvent,
    ) -> Result<EventReceipt, CoreError> {
        self.admit(event)?;
        if interface.id.is_empty() || interface.role != "interface" {
            return Err(CoreError::InvalidInput(
                "an interface identity is required".to_owned(),
            ));
        }
        let (arrived, received) = (interface.clone(), event.clone());
        match self
            .blocking(move |core| core.admission(&arrived, &received))
            .await?
        {
            Admitted::Settled(receipt) => Ok(receipt),
            Admitted::Capturing(agent) => self.intake.accept(self, interface, event, &agent).await,
        }
    }

    /// Settles an event that needs no capture: an earlier receipt, a stranded
    /// conversation's explanation, or a turn without attachments. An event
    /// with attachments for a current agent is left to capture.
    fn admission(
        &self,
        interface: &SelectedProvider,
        event: &ExternalEvent,
    ) -> Result<Admitted, CoreError> {
        let owner = self.conversation_owner(
            &interface.id,
            &ConversationQuery {
                agent_id: event.agent_id.clone(),
                channel_id: event.channel_id.clone(),
                audience_id: Some(event.audience_id.clone()),
            },
        )?;
        // Read after the owner: a stranded notice moves its conversation in
        // the transaction that records its receipt, so a duplicate that sees
        // the move also sees the receipt.
        if let Some(receipt) = self
            .database
            .event_receipt(&interface.id, &event.event_id)?
        {
            return Ok(Admitted::Settled(receipt));
        }
        let agent = match owner {
            Owner::Current(agent) if event.attachments.is_some() => {
                return Ok(Admitted::Capturing(agent.clone()));
            }
            Owner::Current(agent) => agent,
            Owner::Stranded {
                runtime_id,
                current,
            } => {
                self.intake.ensure_in_order(&interface.id, event)?;
                let current = current.map(|agent| agent.runtime.as_str());
                return Ok(Admitted::Settled(self.database.accept_event(
                    &interface.id,
                    current.unwrap_or(&runtime_id),
                    event,
                    Admission::Stranded {
                        previous_runtime: &runtime_id,
                        explanation: &stranded(&runtime_id, &event.agent_id, current),
                    },
                    None,
                )?));
            }
        };
        self.intake.ensure_in_order(&interface.id, event)?;
        self.commit(interface, event, agent, None)
            .map(Admitted::Settled)
    }

    /// Record the turn, referencing a published capture of its attachments.
    pub(crate) fn commit(
        &self,
        interface: &SelectedProvider,
        event: &ExternalEvent,
        agent: &AgentOptions,
        capture: Option<&Capture>,
    ) -> Result<EventReceipt, CoreError> {
        let runtime = self.enabled_runtime(&agent.runtime)?;
        let resources = resources::materialize(agent.resources.as_deref(), &self.snapshot_store())?;
        let selected = SelectedProvider {
            id: runtime.id.clone(),
            role: "runtime".to_owned(),
            version: runtime.version.clone(),
            url: runtime.url.clone(),
        };
        let providers =
            serde_json::to_value([interface, &selected]).map_err(anyhow::Error::from)?;
        let receipt = self.database.accept_event(
            &interface.id,
            &runtime.id,
            event,
            Admission::Queued {
                providers: &providers,
                resource_snapshot_id: &resources,
            },
            capture,
        )?;
        if !receipt.duplicate {
            self.wake();
        }
        Ok(receipt)
    }

    /// What core knows about an event of an authenticated interface: its
    /// receipt, a capture still in progress, or nothing. An interface asks
    /// before acquiring attachments again, and to settle a submission whose
    /// answer it lost; only the transactional acceptance decides.
    /// # Errors
    /// Returns storage errors.
    pub fn event_status(&self, interface: &str, event_id: &str) -> Result<EventStatus> {
        if let Some(receipt) = self.database.event_receipt(interface, event_id)? {
            return Ok(EventStatus::Accepted(receipt));
        }
        if self.intake.capturing(interface, event_id) {
            return Ok(EventStatus::Capturing);
        }
        // A capture leaves the in-progress set only after its commit settled.
        Ok(self
            .database
            .event_receipt(interface, event_id)?
            .map_or(EventStatus::Unknown, EventStatus::Accepted))
    }

    fn agent(&self, id: &str) -> Result<&AgentOptions, CoreError> {
        self.options
            .agents
            .iter()
            .find(|agent| agent.id == id)
            .ok_or_else(|| CoreError::InvalidInput("unknown agent".to_owned()))
    }

    fn enabled_runtime(&self, id: &str) -> Result<RuntimeProvider, CoreError> {
        let runtime = self.runtime(id)?;
        if !runtime.enabled {
            return Err(CoreError::Conflict(
                "conversation runtime provider is disabled".to_owned(),
            ));
        }
        Ok(runtime)
    }

    fn runtime(&self, id: &str) -> Result<RuntimeProvider, CoreError> {
        self.runtimes
            .borrow()
            .iter()
            .find(|runtime| runtime.id == id)
            .cloned()
            .ok_or_else(|| CoreError::Conflict(format!("runtime provider {id} is not configured")))
    }

    /// Replaces the runtimes core admits work for and hands it to, as the
    /// application derived them from its registry. A runtime disabled from
    /// now on admits no executable turn and is handed no queued one; turns
    /// it holds settle as they would. Dispatchers hear of the change, so
    /// queued turns of a runtime enabled again are handed over.
    /// # Errors
    /// Returns invalid runtime facts, which leave the current ones in place.
    pub fn set_runtimes(&self, runtimes: Vec<RuntimeProvider>) -> Result<()> {
        validate_runtimes(&runtimes)?;
        self.runtimes.send_replace(runtimes.into());
        self.wake();
        Ok(())
    }

    /// Claim pending output for an authenticated interface process instance.
    /// # Errors
    /// Returns storage errors.
    pub fn claim_delivery(&self, interface: &str, owner: &str) -> Result<Option<Delivery>> {
        self.database.claim_delivery(interface, owner)
    }

    /// Claim the pending output of one turn, as [`Self::claim_delivery`] does.
    /// # Errors
    /// Returns storage errors.
    pub fn claim_turn_delivery(
        &self,
        interface: &str,
        owner: &str,
        turn_id: &str,
    ) -> Result<Option<Delivery>> {
        self.database.claim_turn_delivery(interface, owner, turn_id)
    }

    /// Settle abandoned delivery claims without silently resending uncertain output.
    /// # Errors
    /// Returns storage errors.
    pub fn reconcile_deliveries(
        &self,
        interface: &str,
        owner: &str,
    ) -> Result<DeliveryReconciliation> {
        Ok(DeliveryReconciliation {
            feedback_target_ids: self.database.reconcile_delivery_claims(interface, owner)?,
        })
    }

    /// Record the delivery outcome only for its claiming interface instance.
    /// # Errors
    /// Returns claim conflicts or storage errors.
    pub fn acknowledge_delivery(
        &self,
        interface: &str,
        owner: &str,
        id: i64,
        ack: &DeliveryAck,
    ) -> Result<(), CoreError> {
        if !self.database.acknowledge_delivery(
            interface,
            id,
            owner,
            ack.outcome,
            ack.native_ref.as_deref(),
        )? {
            return Err(CoreError::Conflict(format!(
                "delivery {id} is not claimed by this interface instance"
            )));
        }
        Ok(())
    }

    /// A page of an authenticated interface's jobs at work, after the job
    /// whose turn at work was `after`, each with the ID of the runtime that
    /// owns it, and where the next page begins when more follow. A running
    /// turn's `activity` is left empty: snapshots belong to the runtime.
    /// # Errors
    /// Returns a cursor that is not one of the interface's turns as invalid
    /// input, or storage errors.
    pub(crate) fn jobs_at_work(
        &self,
        interface: &str,
        after: Option<&str>,
    ) -> Result<JobPage, CoreError> {
        self.database
            .jobs_at_work(interface, after)?
            .ok_or_else(|| CoreError::InvalidInput("no such page of jobs".to_owned()))
    }

    /// The jobs of an authenticated interface it names by their origin, at
    /// most [`JOB_READ_LIMIT`], however they stand, each with the ID of the
    /// runtime that owns it; a name of nothing the interface began is left
    /// out.
    /// # Errors
    /// Returns too many names as invalid input, or storage errors.
    pub fn named_jobs(
        &self,
        interface: &str,
        named: &[String],
    ) -> Result<Vec<(String, Job)>, CoreError> {
        if named.len() > JOB_READ_LIMIT {
            return Err(CoreError::InvalidInput(format!(
                "a job read names at most {JOB_READ_LIMIT} jobs"
            )));
        }
        Ok(self.database.named_jobs(interface, named)?)
    }

    /// Inspect a turn, without granting external operator authority.
    /// # Errors
    /// Returns storage errors.
    pub fn inspect_turn(&self, id: &str) -> Result<Option<TurnInspection>> {
        self.database.inspect_turn(id)
    }

    /// Read recent turns; callers own any external authentication policy.
    /// # Errors
    /// Returns storage errors.
    pub fn recent_turns(&self, event_id: Option<&str>, limit: u32) -> Result<Vec<TurnSummary>> {
        self.database.recent_turns(event_id, limit)
    }

    /// # Errors
    /// Returns storage errors.
    pub fn turn_counts(&self) -> Result<StateCounts> {
        self.database.turn_counts()
    }

    /// Count delivery states independently from execution.
    /// # Errors
    /// Returns storage errors.
    pub fn delivery_counts(&self) -> Result<StateCounts> {
        self.database.delivery_counts()
    }

    /// Answers to `interfaces` not yet delivered: waiting for their
    /// interface, or claimed by it and not acknowledged.
    /// # Errors
    /// Returns storage errors.
    pub fn undelivered(&self, interfaces: &[&str]) -> Result<u64> {
        self.database.undelivered(interfaces)
    }

    pub(crate) fn runtime_request(&self, turn: &QueuedTurn) -> RuntimeTurnRequest {
        RuntimeTurnRequest {
            correlation_id: turn.turn_id.clone(),
            interface_id: turn.interface_id.clone(),
            agent_id: turn.agent_id.clone(),
            audience_id: turn.audience_id.clone(),
            channel_id: turn.channel_id.clone(),
            input: turn.input.clone(),
            native: turn.native.clone(),
            resource_root: resources::root(&self.snapshot_store(), &turn.resource_snapshot_id)
                .display()
                .to_string(),
            attachment_root: attachments::store(&self.options.data_directory)
                .display()
                .to_string(),
            attachments: turn
                .attachments
                .iter()
                .map(attachments::CapturedFile::wire)
                .collect(),
        }
    }

    /// Re-read a turn's captured files before any runtime sees them,
    /// off the async reactor.
    async fn check_attachments(&self, turn: &QueuedTurn) -> Result<(), Unusable> {
        if turn.attachments.is_empty() {
            return Ok(());
        }
        let (data, files) = (
            self.options.data_directory.clone(),
            turn.attachments.clone(),
        );
        tokio::task::spawn_blocking(move || {
            files
                .iter()
                .try_for_each(|file| attachments::check(&data, file))
        })
        .await
        .map_err(|error| Unusable::Unreadable(error.into()))?
    }

    fn admit(&self, event: &ExternalEvent) -> Result<(), CoreError> {
        if event.event_id.is_empty()
            || event.agent_id.is_empty()
            || event.user_id.is_empty()
            || event.audience_id.is_empty()
            || event.delivery_target_id.is_empty()
            || event.feedback_target_id.is_empty()
            || (event.content.trim().is_empty() && event.attachments.is_none())
        {
            return Err(CoreError::InvalidInput(
                "event identity, agent, audience, targets, user, and content or attachments are required"
                    .to_owned(),
            ));
        }
        if event.event_id.starts_with(crate::db::RUNTIME_EVENT_PREFIX) {
            return Err(CoreError::InvalidInput(format!(
                "event identities starting {} are reserved for core",
                crate::db::RUNTIME_EVENT_PREFIX
            )));
        }
        if let Some(staged) = &event.attachments {
            crate::intake::admit(&self.options.attachments, staged)?;
        }
        Ok(())
    }
}
#[derive(Debug, PartialEq, Eq)]
pub enum WorkerStep {
    Idle,
    Advanced,
    Backoff,
}

/// What the conversation is told when a turn did not complete, with the
/// runtime's explanation when it gave one.
fn interrupted(explanation: Option<&str>) -> String {
    const CONTINUE: &str = "Send another message to continue in this conversation. Actions already taken may have had effects.";
    match explanation {
        Some(explanation) => format!("That turn was interrupted: {explanation}\n{CONTINUE}"),
        None => format!("That turn was interrupted. {CONTINUE}"),
    }
}

/// A runtime's final report as core keeps it: one larger than
/// [`ACTIVITY_REPORT_LIMIT`] is not kept, since display never fails a turn.
fn kept_report(turn_id: &str, activity: Option<TurnActivity>) -> Option<TurnActivity> {
    let bytes = serde_json::to_vec(activity.as_ref()?).ok()?.len();
    if bytes > ACTIVITY_REPORT_LIMIT {
        warn!(
            turn_id,
            bytes, "a runtime's final report is over its limit and is not kept"
        );
        return None;
    }
    activity
}

fn validate_options(options: &CoreOptions) -> Result<()> {
    let mut ids = std::collections::BTreeSet::new();
    for agent in &options.agents {
        anyhow::ensure!(
            !agent.id.is_empty() && ids.insert(&agent.id) && !agent.runtime.is_empty(),
            "agent identities must be nonempty and unique, each with a runtime"
        );
    }
    validate_runtimes(&options.runtimes)
}

fn validate_runtimes(runtimes: &[RuntimeProvider]) -> Result<()> {
    let mut ids = std::collections::BTreeSet::new();
    for runtime in runtimes {
        anyhow::ensure!(
            !runtime.id.is_empty() && ids.insert(&runtime.id),
            "runtime identities must be nonempty and unique"
        );
    }
    Ok(())
}
