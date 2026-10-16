//! Admission, runtime ownership, execution and delivery over one SQLite authority.
mod ownership;

use crate::{
    attachments::{self, Capture, Reconciled, Unusable},
    db::{Admission, ClaimedTurn, Database, JobPage, TurnFiles},
    intake::Intake,
    resources,
};
use anyhow::{Context as _, Result, anyhow};
use carillon_installation::config::AttachmentLimits;
use carillon_protocol::model::{
    ACTIVITY_REPORT_LIMIT, Delivery, DeliveryAck, DeliveryReconciliation, EventReceipt,
    EventStatus, ExternalEvent, JOB_READ_LIMIT, Job, ModelQuery, RuntimeTurnRequest,
    RuntimeTurnResponse, SelectedProvider, StateCounts, TurnActivity, TurnInspection, TurnSummary,
    UnansweredTurn,
};
use ownership::{Owner, stranded};
use std::{path::PathBuf, sync::Arc};

/// What admission decided before any capture work.
enum Admitted {
    Settled(EventReceipt),
    /// Attachments to capture for a current agent before the turn is recorded.
    Capturing(AgentOptions),
}
use tokio::sync::watch;
use tokio_util::{sync::CancellationToken, task::TaskTracker};
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

/// The two kinds of queued turn, each run by workers of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Lane {
    /// Turns for the messages people send: each conversation's run one at
    /// a time, in acceptance order.
    Messages,
    /// Turns core queues to carry the report of work a message's turn left
    /// running. They only read native records and may wait for hours, so
    /// they run beside messages, never in their place.
    Observations,
}

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
    /// Marked changed, per lane, whenever this handle may have made a turn
    /// claimable.
    work: Arc<[watch::Sender<()>; 2]>,
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
            work: Arc::new([watch::Sender::new(()), watch::Sender::new(())]),
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

    /// Changes whenever a turn of `lane` may have become claimable through
    /// this handle or its clones: a message accepted, a message finished,
    /// which frees its conversation's next one, or an observation queued.
    /// A worker that finds nothing to claim waits for the next change
    /// rather than polling; marking the receiver unchanged before each
    /// claim means no change is missed.
    #[must_use]
    pub fn work(&self, lane: Lane) -> watch::Receiver<()> {
        self.work[lane as usize].subscribe()
    }

    fn wake(&self, lane: Lane) {
        self.work[lane as usize].send_replace(());
    }

    /// Recover running turns, then interrupt queued turns whose agent moved or was removed.
    /// Call only after acquiring exclusive worker ownership; opening another handle is not a restart.
    /// # Errors
    /// Returns storage errors.
    pub fn recover_interrupted(&self) -> Result<usize> {
        let mut recovered = self.database.recover_running_turns()?;
        for (agent_id, runtime_id) in self.database.queued_owners()? {
            if self
                .agent(&agent_id)
                .is_ok_and(|agent| agent.runtime == runtime_id)
            {
                continue;
            }
            let explanation = stranded(&runtime_id, &agent_id, None);
            let observation_notice = format!(
                "{explanation}\nWork already started there will not be reported here. Actions already taken may have had effects."
            );
            recovered += self.database.interrupt_queued(
                &agent_id,
                &runtime_id,
                &explanation,
                &observation_notice,
            )?;
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
            &ModelQuery {
                agent_id: event.agent_id.clone(),
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
            self.wake(Lane::Messages);
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

    /// Replaces the runtimes core admits work for and runs it by, as the
    /// application derived them from its registry. A runtime disabled from
    /// now on admits no executable turn and starts no queued one;
    /// turns already running settle as they would. Workers hear of the
    /// change, so queued turns of a runtime enabled again are claimed.
    /// # Errors
    /// Returns invalid runtime facts, which leave the current ones in place.
    pub fn set_runtimes(&self, runtimes: Vec<RuntimeProvider>) -> Result<()> {
        validate_runtimes(&runtimes)?;
        self.runtimes.send_replace(runtimes.into());
        self.wake(Lane::Messages);
        self.wake(Lane::Observations);
        Ok(())
    }

    /// Claim and run a runtime's oldest claimable turn of `lane` through a
    /// caller-supplied runtime operation, and record its outcome once.
    /// The application must verify local transport prerequisites (such as
    /// credentials) before this call. After opening existing storage, recover
    /// interrupted turns under exclusive ownership before starting workers.
    ///
    /// A message whose runtime provably never received it is queued again;
    /// any other failure ends it, never replaying work that may have run.
    /// An observation dispatches nothing and only reads native records, so
    /// one that got no answer is asked again too, and its report is
    /// delivered once, when it completes. A runtime's explained failure, or
    /// an answer core cannot read, ends it like a message.
    ///
    /// Once `stopping` is cancelled no turn is claimed, even by storage work
    /// that was waiting for the database writer; one already claimed runs to
    /// its outcome.
    /// # Errors
    /// Returns registry or storage errors. A failed outcome commit leaves the turn running for recovery.
    pub async fn run_next(
        &self,
        runtime_id: &str,
        lane: Lane,
        stopping: &CancellationToken,
        submit: impl AsyncFnOnce(RuntimeTurnRequest) -> Result<RuntimeTurnResponse, TurnFailure>,
    ) -> Result<WorkerStep> {
        if !self.runtime(runtime_id)?.enabled {
            return Ok(WorkerStep::Backoff);
        }
        let (runtime, stopping) = (runtime_id.to_owned(), stopping.clone());
        let Some(turn) = self
            .blocking(move |core| core.database.claim(&runtime, lane, &stopping))
            .await?
        else {
            return Ok(WorkerStep::Idle);
        };
        let step = self.run(&turn, submit).await?;
        // A message that left `running` frees its conversation's next one.
        if step == WorkerStep::Advanced && turn.follow_up.is_none() {
            self.wake(Lane::Messages);
        }
        Ok(step)
    }

    async fn run(
        &self,
        turn: &ClaimedTurn,
        submit: impl AsyncFnOnce(RuntimeTurnRequest) -> Result<RuntimeTurnResponse, TurnFailure>,
    ) -> Result<WorkerStep> {
        let observation = turn.follow_up.is_some();
        // An observation carries no files and offers none.
        let (unanswered, mut carried) = if observation {
            (Vec::new(), Vec::new())
        } else {
            if let Err(unusable) = self.check_attachments(turn).await {
                let (message, explanation) = match &unusable {
                    Unusable::Altered(message) => (message.clone(), ALTERED_ATTACHMENT),
                    Unusable::Unreadable(error) => (format!("{error:#}"), UNREADABLE_ATTACHMENT),
                };
                self.interrupt(turn, message.clone(), interrupted(Some(explanation)), None)
                    .await
                    .with_context(|| format!("record unrunnable turn {}", turn.turn_id))?;
                warn!(turn_id = %turn.turn_id, error = %message, "captured attachment is not usable; turn not run");
                return Ok(WorkerStep::Advanced);
            }
            let limits = &self.options.attachments;
            let earlier =
                self.database
                    .unnoted_files(turn, limits.max_files, limits.max_event_bytes())?;
            let offered = self.offer(earlier).await?;
            // The files this turn carries to the runtime: its own, first, and
            // the earlier turns offered with it, even when none of a turn's
            // files was left.
            let carried = std::iter::once(turn.turn_id.clone())
                .chain(offered.iter().map(|earlier| earlier.correlation_id.clone()))
                .collect::<Vec<_>>();
            let unanswered = offered
                .into_iter()
                .filter(|earlier| !earlier.attachments.is_empty())
                .collect();
            (unanswered, carried)
        };
        let outcome = async {
            let request = self.runtime_request(turn, unanswered);
            let correlation_id = request.correlation_id.clone();
            let mut response = submit(request).await?;
            validate_response(&correlation_id, &response)?;
            response.activity = kept_report(&turn.turn_id, response.activity.take());
            let answered = (
                response.follow_up.is_some(),
                response.unanswered_left,
                response.attachments_left,
            );
            let claimed = turn.clone();
            self.blocking(move |core| core.database.complete_turn(&claimed, &response))
                .await
                .map_err(TurnFailure::Uncertain)?;
            Ok(answered)
        }
        .await;
        // Each outcome is written once. If that write fails, the turn stays
        // `running`, which blocks only its conversation and is never claimed
        // again; startup recovery then records it as interrupted, or asks an
        // observation again.
        let (error, notice, activity) = match outcome {
            Ok((follow_up, unanswered_left, attachments_left)) => {
                if follow_up {
                    self.wake(Lane::Observations);
                }
                // An answered turn's conversation was saved with everything
                // it carried: its own files and the earlier ones, unless its
                // runtime left either for a later turn. Should recording that
                // fail, the files are offered again and the runtime skips
                // those it holds.
                if unanswered_left {
                    carried.truncate(1);
                }
                if attachments_left {
                    carried.retain(|carried| *carried != turn.turn_id);
                }
                if !carried.is_empty()
                    && let Err(error) = self
                        .blocking(move |core| core.database.mark_noted(&carried))
                        .await
                {
                    warn!(turn_id = %turn.turn_id, %error, "attachments carried by an answered turn were not recorded as noted");
                }
                return Ok(WorkerStep::Advanced);
            }
            Err(TurnFailure::Retry(failure)) => return self.requeue(turn, &failure).await,
            Err(TurnFailure::Unanswered(failure)) if observation => {
                return self.requeue(turn, &failure).await;
            }
            Err(TurnFailure::Uncertain(failure) | TurnFailure::Unanswered(failure)) => {
                (failure.to_string(), interrupted(None), None)
            }
            Err(TurnFailure::Reported(reported)) => {
                let error = reported.diagnostic();
                let Reported {
                    explanation,
                    activity,
                    ..
                } = *reported;
                (
                    error,
                    interrupted(explanation.as_deref()),
                    kept_report(&turn.turn_id, activity),
                )
            }
        };
        self.interrupt(turn, error.clone(), notice, activity)
            .await
            .with_context(|| format!("record failure of turn {}", turn.turn_id))?;
        warn!(turn_id = %turn.turn_id, observation, %error, "turn did not complete");
        Ok(WorkerStep::Advanced)
    }

    async fn interrupt(
        &self,
        turn: &ClaimedTurn,
        error: String,
        notice: String,
        activity: Option<TurnActivity>,
    ) -> Result<()> {
        let turn = turn.clone();
        self.blocking(move |core| {
            core.database
                .interrupt_turn(&turn, &error, &notice, activity.as_ref())
        })
        .await
    }

    /// Returns a turn whose runtime gave no answer to the queue, for its
    /// worker to claim again after backing off.
    async fn requeue(&self, turn: &ClaimedTurn, failure: &anyhow::Error) -> Result<WorkerStep> {
        let claimed = turn.clone();
        self.blocking(move |core| core.database.requeue_turn(&claimed))
            .await
            .with_context(|| format!("requeue turn {}", turn.turn_id))?;
        warn!(turn_id = %turn.turn_id, error = %failure, "runtime gave no answer; turn queued again");
        Ok(WorkerStep::Backoff)
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
            follow_up: turn.follow_up.clone(),
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

    /// The earlier turns offered to a turn, each whole: Pi notes a turn's
    /// files once. A file that is no longer the one accepted is left out for
    /// good; a turn with a file that cannot be read now is not offered, and
    /// is offered again later.
    async fn offer(&self, earlier: Vec<TurnFiles>) -> Result<Vec<UnansweredTurn>> {
        if earlier.is_empty() {
            return Ok(Vec::new());
        }
        let data = self.options.data_directory.clone();
        tokio::task::spawn_blocking(move || {
            earlier
                .into_iter()
                .filter_map(|earlier| {
                    let mut attachments = Vec::with_capacity(earlier.files.len());
                    for file in &earlier.files {
                        match attachments::check(&data, file) {
                            Ok(()) => attachments.push(file.wire()),
                            Err(Unusable::Altered(message)) => {
                                warn!(turn_id = %earlier.turn_id, error = %message, "a changed earlier attachment is not offered again");
                            }
                            Err(Unusable::Unreadable(error)) => {
                                warn!(turn_id = %earlier.turn_id, error = %error, "an earlier message's files are offered later: one cannot be read now");
                                return None;
                            }
                        }
                    }
                    Some(UnansweredTurn {
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
        if event
            .event_id
            .starts_with(crate::db::FOLLOW_UP_EVENT_PREFIX)
        {
            return Err(CoreError::InvalidInput(format!(
                "event identities starting {} are reserved for core",
                crate::db::FOLLOW_UP_EVENT_PREFIX
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

#[derive(Debug)]
pub enum TurnFailure {
    /// The runtime provably never received the turn; running it again is safe.
    Retry(anyhow::Error),
    /// The request may have reached the runtime, but no answer arrived: the
    /// connection was lost or timed out. A message is treated as
    /// `Uncertain`; an observation, which only reads, is asked again.
    Unanswered(anyhow::Error),
    /// Model work may have run; it must not be automatically replayed.
    Uncertain(anyhow::Error),
    /// The runtime answered that the turn failed. As with `Uncertain`,
    /// model work may have run and is not automatically replayed.
    Reported(Box<Reported>),
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

/// What a runtime answered when it failed a turn.
#[derive(Debug)]
pub struct Reported {
    pub status: u16,
    /// Why, in words phrased for the conversation and safe to display, when
    /// the runtime gave such an explanation.
    pub explanation: Option<String>,
    /// The runtime's final report of what the turn did, when it gave one.
    pub activity: Option<TurnActivity>,
}

impl Reported {
    /// The operator's diagnostic of the failure.
    fn diagnostic(&self) -> String {
        self.explanation.as_ref().map_or_else(
            || {
                format!(
                    "runtime provider failed the turn with {} and no explanation",
                    self.status
                )
            },
            |explanation| format!("runtime reported failure: {explanation}"),
        )
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
