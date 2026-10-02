//! Admission, runtime ownership, execution and delivery over one SQLite authority.
use crate::{
    attachments::{self, Capture, Reconciled, Unusable},
    db::{ClaimedTurn, Database, TurnFiles},
    intake::Intake,
    resources,
};
use anyhow::{Context as _, Result, anyhow};
use carillon_installation::config::AttachmentLimits;
use carillon_protocol::model::{
    ActiveTurn, Delivery, DeliveryAck, DeliveryReconciliation, EventReceipt, EventStatus,
    ExternalEvent, ModelQuery, RuntimeTurnRequest, RuntimeTurnResponse, SelectedProvider,
    StateCounts, TurnInspection, TurnSummary, UnansweredTurn,
};
use std::{path::PathBuf, sync::Arc};
use tracing::warn;

/// Runtime identity and availability, without credentials or a transport implementation.
#[derive(Clone, Debug)]
pub struct RuntimeProvider {
    pub id: String,
    pub version: String,
    pub url: String,
    pub enabled: bool,
}

/// Application-supplied policy and owned storage locations for one agent.
#[derive(Clone, Debug)]
pub struct CoreOptions {
    pub data_directory: PathBuf,
    pub agent_id: String,
    pub resource_root: PathBuf,
    pub default_runtime: String,
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
    pub(crate) options: CoreOptions,
    pub(crate) intake: Arc<Intake>,
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

    fn assemble(database: Database, options: CoreOptions) -> Self {
        Self {
            intake: Arc::new(Intake::new(options.attachments)),
            database,
            options,
        }
    }

    /// Mark orphaned running turns interrupted, never replaying their input.
    /// Call only after acquiring exclusive worker ownership; opening another handle is not a restart.
    /// # Errors
    /// Returns storage errors.
    pub fn recover_interrupted(&self) -> Result<usize> {
        self.database.recover_running_turns()
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
    /// An event with attachments is accepted only once all its staged files
    /// are captured and published. The capture belongs to core, not to the
    /// caller: it continues if the caller goes away, a retry of the same
    /// event waits for it instead of copying again, and at most the
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
        if let Some(receipt) = self
            .database
            .event_receipt(&interface.id, &event.event_id)?
        {
            return Ok(receipt);
        }
        if event.attachments.is_some() {
            return self.intake.accept(self, interface, event).await;
        }
        self.intake.ensure_in_order(&interface.id, event)?;
        self.commit(interface, event, None)
    }

    /// Record the turn, referencing a published capture of its attachments.
    pub(crate) fn commit(
        &self,
        interface: &SelectedProvider,
        event: &ExternalEvent,
        capture: Option<&Capture>,
    ) -> Result<EventReceipt, CoreError> {
        let runtime = self.runtime_for(
            &interface.id,
            &ModelQuery {
                agent_id: event.agent_id.clone(),
                audience_id: Some(event.audience_id.clone()),
            },
        )?;
        let resources =
            resources::materialize(&self.options.resource_root, &self.snapshot_store())?;
        let selected = SelectedProvider {
            id: runtime.id.clone(),
            role: "runtime".to_owned(),
            version: runtime.version.clone(),
            url: runtime.url.clone(),
        };
        let providers =
            serde_json::to_value([interface, &selected]).map_err(anyhow::Error::from)?;
        Ok(self.database.accept_event(
            &interface.id,
            &runtime.id,
            event,
            &providers,
            &resources,
            capture,
        )?)
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

    /// Resolve the pinned conversation owner, or the default for a new conversation.
    /// # Errors
    /// Returns an unknown agent, missing/disabled owner or storage error.
    pub fn runtime_for(
        &self,
        interface_id: &str,
        query: &ModelQuery,
    ) -> Result<&RuntimeProvider, CoreError> {
        if query.agent_id != self.options.agent_id {
            return Err(CoreError::InvalidInput("unknown agent".to_owned()));
        }
        let owner = match &query.audience_id {
            Some(audience) => {
                self.database
                    .conversation_runtime(interface_id, &query.agent_id, audience)?
            }
            None => None,
        };
        let id = owner.as_deref().unwrap_or(&self.options.default_runtime);
        let runtime = self.runtime(id)?;
        if !runtime.enabled {
            return Err(CoreError::Conflict(
                "conversation runtime provider is disabled".to_owned(),
            ));
        }
        Ok(runtime)
    }

    fn runtime(&self, id: &str) -> Result<&RuntimeProvider, CoreError> {
        self.options
            .runtimes
            .iter()
            .find(|runtime| runtime.id == id)
            .ok_or_else(|| CoreError::Conflict(format!("runtime provider {id} is not configured")))
    }

    /// Claim and execute one eligible turn through a caller-supplied runtime operation.
    /// The application must verify local transport prerequisites (such as credentials) before this call.
    /// # Errors
    /// Returns registry or storage errors. A failed outcome commit leaves the turn running for recovery.
    pub async fn execute_next(
        &self,
        runtime_id: &str,
        submit: impl AsyncFnOnce(RuntimeTurnRequest) -> Result<RuntimeTurnResponse, TurnFailure>,
    ) -> Result<WorkerStep> {
        if !self.runtime(runtime_id)?.enabled {
            return Ok(WorkerStep::Backoff);
        }
        let Some(turn) = self.database.claim_turn(runtime_id)? else {
            return Ok(WorkerStep::Idle);
        };
        if let Err(unusable) = self.check_attachments(&turn).await {
            let (message, explanation) = match &unusable {
                Unusable::Altered(message) => (message.clone(), ALTERED_ATTACHMENT),
                Unusable::Unreadable(error) => (format!("{error:#}"), UNREADABLE_ATTACHMENT),
            };
            self.database
                .interrupt_turn(&turn, &message, Some(explanation))
                .with_context(|| format!("record unrunnable turn {}", turn.turn_id))?;
            warn!(turn_id = %turn.turn_id, error = %message, "captured attachment is not usable; turn not run");
            return Ok(WorkerStep::Advanced);
        }
        let limits = &self.options.attachments;
        let earlier =
            self.database
                .unnoted_files(&turn, limits.max_files, limits.max_event_bytes())?;
        // The files this turn carries to the runtime: its own and the earlier
        // ones offered with it.
        let carried = std::iter::once(turn.turn_id.clone())
            .chain(earlier.iter().map(|earlier| earlier.turn_id.clone()))
            .collect::<Vec<_>>();
        let unanswered = self.offer(earlier).await?;
        let outcome = async {
            let request = self.runtime_request(&turn, unanswered);
            let correlation_id = request.correlation_id.clone();
            let response = submit(request).await?;
            validate_response(&correlation_id, &response)?;
            self.database
                .complete_turn(&turn, &response)
                .map_err(TurnFailure::Uncertain)
        }
        .await;
        // Each outcome is written once. If that write fails, the turn stays
        // `running`, which blocks only its conversation and is never claimed
        // again; startup recovery then records it as interrupted.
        match outcome {
            Ok(()) => {
                // An answered turn's conversation was saved with everything
                // it carried. Should recording that fail, the files are
                // offered again and the runtime skips those it holds.
                if let Err(error) = self.database.mark_noted(&carried) {
                    warn!(turn_id = %turn.turn_id, %error, "attachments carried by an answered turn were not recorded as noted");
                }
                Ok(WorkerStep::Advanced)
            }
            Err(TurnFailure::Retry(failure)) => {
                self.database
                    .requeue_turn(&turn)
                    .with_context(|| format!("requeue turn {}", turn.turn_id))?;
                warn!(turn_id = %turn.turn_id, error = %failure, "runtime was unavailable; turn requeued");
                Ok(WorkerStep::Backoff)
            }
            Err(TurnFailure::Uncertain(failure)) => {
                let message = failure.to_string();
                self.database
                    .interrupt_turn(&turn, &message, None)
                    .with_context(|| {
                        format!("record uncertain outcome of turn {}", turn.turn_id)
                    })?;
                warn!(turn_id = %turn.turn_id, error = %message, "runtime outcome is uncertain");
                Ok(WorkerStep::Advanced)
            }
            Err(TurnFailure::Explained(explanation)) => {
                let message = format!("runtime reported failure: {explanation}");
                self.database
                    .interrupt_turn(&turn, &message, Some(&explanation))
                    .with_context(|| format!("record failure of turn {}", turn.turn_id))?;
                warn!(turn_id = %turn.turn_id, error = %message, "runtime reported failure");
                Ok(WorkerStep::Advanced)
            }
        }
    }

    /// Claim pending output for an authenticated interface process instance.
    /// # Errors
    /// Returns storage errors.
    pub fn claim_delivery(&self, interface: &str, owner: &str) -> Result<Option<Delivery>> {
        self.database.claim_delivery(interface, owner)
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

    /// Queued and running turns of an authenticated interface, in acceptance
    /// order, each with the ID of the runtime that owns it. `activity` is
    /// left empty: snapshots belong to the runtime.
    /// # Errors
    /// Returns storage errors.
    pub fn active_turns(&self, interface: &str) -> Result<Vec<(String, ActiveTurn)>> {
        self.database.active_turns(interface)
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

    /// Count execution states.
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

    pub(crate) fn runtime_request(
        &self,
        turn: &ClaimedTurn,
        unanswered: Vec<UnansweredTurn>,
    ) -> RuntimeTurnRequest {
        RuntimeTurnRequest {
            correlation_id: turn.turn_id.clone(),
            interface_id: turn.interface_id.clone(),
            agent_id: turn.agent_id.clone(),
            audience_id: turn.audience_id.clone(),
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
            unanswered,
        }
    }

    /// The earlier files offered to a turn, without any that are no longer
    /// the files accepted: those are logged and, once the turn is answered,
    /// not offered again.
    async fn offer(&self, earlier: Vec<TurnFiles>) -> Result<Vec<UnansweredTurn>> {
        if earlier.is_empty() {
            return Ok(Vec::new());
        }
        let data = self.options.data_directory.clone();
        tokio::task::spawn_blocking(move || {
            earlier
                .into_iter()
                .filter_map(|earlier| {
                    let attachments = earlier
                        .files
                        .iter()
                        .filter(|file| match attachments::check(&data, file) {
                            Ok(()) => true,
                            Err(unusable) => {
                                warn!(turn_id = %earlier.turn_id, error = %unusable, "an earlier attachment is not offered again");
                                false
                            }
                        })
                        .map(attachments::CapturedFile::wire)
                        .collect::<Vec<_>>();
                    (!attachments.is_empty()).then_some(UnansweredTurn {
                        correlation_id: earlier.turn_id,
                        attachments,
                    })
                })
                .collect()
        })
        .await
        .context("check earlier attachments")
    }

    /// Re-read a claimed turn's captured files before any runtime sees them,
    /// off the async reactor.
    async fn check_attachments(&self, turn: &ClaimedTurn) -> Result<(), Unusable> {
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
        if event.agent_id != self.options.agent_id {
            return Err(CoreError::InvalidInput("unknown agent".to_owned()));
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

/// How a turn execution attempt failed.
#[derive(Debug)]
pub enum TurnFailure {
    /// The runtime provably never received the turn; running it again is safe.
    Retry(anyhow::Error),
    /// Model work may have run; it must not be automatically replayed.
    Uncertain(anyhow::Error),
    /// The runtime reported that the turn failed, explaining why in words
    /// phrased for the conversation and safe to display. As with `Uncertain`,
    /// model work may have run and is not automatically replayed.
    Explained(String),
}

pub(crate) fn validate_response(
    correlation_id: &str,
    response: &RuntimeTurnResponse,
) -> Result<(), TurnFailure> {
    if response.correlation_id != correlation_id {
        return Err(TurnFailure::Uncertain(anyhow!(
            "runtime returned a mismatched correlation ID"
        )));
    }
    if response.output.trim().is_empty() {
        return Err(TurnFailure::Uncertain(anyhow!(
            "runtime returned empty output"
        )));
    }
    Ok(())
}

fn validate_options(options: &CoreOptions) -> Result<()> {
    anyhow::ensure!(
        !options.agent_id.is_empty() && !options.default_runtime.is_empty(),
        "agent identity and default runtime are required"
    );
    let mut ids = std::collections::BTreeSet::new();
    for runtime in &options.runtimes {
        anyhow::ensure!(
            !runtime.id.is_empty() && ids.insert(&runtime.id),
            "runtime identities must be nonempty and unique"
        );
    }
    Ok(())
}
