//! Admission, runtime ownership, execution and delivery over one SQLite authority.
use crate::{
    db::{ClaimedTurn, Database},
    resources,
};
use anyhow::{Context as _, Result, anyhow};
use carillon_protocol::model::{
    Delivery, DeliveryAck, DeliveryReconciliation, EventReceipt, ExternalEvent, ModelQuery,
    RuntimeTurnRequest, RuntimeTurnResponse, SelectedProvider, StateCounts, TurnInspection,
    TurnSummary,
};
use std::path::PathBuf;
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
}

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("{0}")]
    InvalidInput(String),
    #[error("{0}")]
    Conflict(String),
    #[error(transparent)]
    Storage(#[from] anyhow::Error),
}

/// A core handle does not spawn workers or initialize any provider.
/// Clones share the same SQLite authority. The application owns the exclusive worker lifetime.
#[derive(Clone)]
pub struct Core {
    pub(crate) database: Database,
    pub(crate) options: CoreOptions,
}

impl Core {
    /// Create fresh operation storage. Does not replace an existing database.
    /// # Errors
    /// Returns invalid configuration or storage errors.
    pub fn create(options: CoreOptions) -> Result<Self> {
        validate_options(&options)?;
        let database = Database::create(&options.data_directory.join("carillon.sqlite3"))?;
        Ok(Self { database, options })
    }

    /// Open existing operation storage without creating it or recovering running turns.
    /// # Errors
    /// Returns invalid configuration or storage/schema errors.
    pub fn open(options: CoreOptions) -> Result<Self> {
        validate_options(&options)?;
        let database = Database::open(&options.data_directory.join("carillon.sqlite3"))?;
        Ok(Self { database, options })
    }

    /// Mark orphaned running turns interrupted, never replaying their input.
    /// Call only after acquiring exclusive worker ownership; opening another handle is not a restart.
    /// # Errors
    /// Returns storage errors.
    pub fn recover_interrupted(&self) -> Result<usize> {
        self.database.recover_running_turns()
    }

    pub(crate) fn snapshot_store(&self) -> PathBuf {
        self.options.data_directory.join("resource-snapshots")
    }

    /// Admit an event from an already authenticated interface, before acknowledging it.
    /// Duplicate receipts do not recapture mutable resources or reconsider runtime ownership.
    /// # Errors
    /// Returns invalid input, unavailable runtime ownership or storage/resource errors.
    pub fn accept(
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
        Ok(self
            .database
            .accept_event(&interface.id, &runtime.id, event, &providers, &resources)?)
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
        let outcome = async {
            let request = self.runtime_request(&turn);
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
            Ok(()) => Ok(WorkerStep::Advanced),
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
                    .interrupt_turn(&turn, &message)
                    .with_context(|| {
                        format!("record uncertain outcome of turn {}", turn.turn_id)
                    })?;
                warn!(turn_id = %turn.turn_id, error = %message, "runtime outcome is uncertain");
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

    pub(crate) fn runtime_request(&self, turn: &ClaimedTurn) -> RuntimeTurnRequest {
        RuntimeTurnRequest {
            correlation_id: turn.turn_id.clone(),
            interface_id: turn.interface_id.clone(),
            agent_id: turn.agent_id.clone(),
            audience_id: turn.audience_id.clone(),
            input: turn.input.clone(),
            resource_root: resources::root(&self.snapshot_store(), &turn.resource_snapshot_id)
                .display()
                .to_string(),
        }
    }

    fn admit(&self, event: &ExternalEvent) -> Result<(), CoreError> {
        if event.event_id.is_empty()
            || event.agent_id.is_empty()
            || event.user_id.is_empty()
            || event.audience_id.is_empty()
            || event.delivery_target_id.is_empty()
            || event.feedback_target_id.is_empty()
            || event.content.trim().is_empty()
        {
            return Err(CoreError::InvalidInput(
                "event identity, agent, audience, targets, user, and content are required"
                    .to_owned(),
            ));
        }
        if event.agent_id != self.options.agent_id {
            return Err(CoreError::InvalidInput("unknown agent".to_owned()));
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
