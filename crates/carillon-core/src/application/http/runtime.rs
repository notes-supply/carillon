//! What runtimes call core for, with their own credential: settling the
//! turns they hold, opening their own and reading back those core records
//! running. And the dispatcher that hands each of them its queued turns.

use axum::{
    Json,
    body::Bytes,
    extract::{Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
};
use carillon_installation::config::{LoadedConfig, Provider, ProviderRole};
use carillon_protocol::{
    model::OpenedTurn,
    turns::{RunningTurn, TurnOpening, TurnOutcome},
};
use std::{
    mem::{self, Discriminant},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use super::{ApiError, AppState, authorize, body, provider_identity, runtime_token};
use crate::{InboxFailure, RuntimeClient, Stalled, WorkerStep, db::Settled};

/// The runtime a call names, authenticated by the same credential core
/// presents to it. It may be disabled: it still settles the turns it
/// accepted before.
fn runtime_provider<'a>(
    headers: &HeaderMap,
    config: &'a LoadedConfig,
) -> Result<&'a Provider, ApiError> {
    let id = provider_identity(headers)?;
    let provider = config
        .provider(id, ProviderRole::Runtime)
        .map_err(|_| ApiError::forbidden("unknown runtime provider"))?;
    authorize(headers, &provider.token_hash)?;
    Ok(provider)
}

/// Settles a turn the calling runtime holds, enabled or not: 204 once
/// recorded, now or as the same outcome before; 409 when the turn ended
/// another way; 404 when the runtime holds no such turn.
pub(super) async fn outcome(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(turn_id): AxumPath<String>,
    bytes: Bytes,
) -> Result<StatusCode, ApiError> {
    let runtime = runtime_provider(&headers, &state.config())?.id.clone();
    let outcome: TurnOutcome = body(&bytes, "turn outcome")?;
    let settled = state
        .core
        .blocking(move |core| core.settle(&runtime, &turn_id, outcome))
        .await?;
    match settled {
        Settled::Recorded => Ok(StatusCode::NO_CONTENT),
        Settled::Superseded => Err(ApiError::new(
            StatusCode::CONFLICT,
            "the turn already ended another way",
        )),
        Settled::Unknown => Err(ApiError::not_found("this runtime holds no such turn")),
    }
}

/// Opens a running turn in one of the calling runtime's conversations, for
/// it to settle later, or returns the receipt of the one its key opened,
/// with the conversation's channel. A disabled runtime opens none.
pub(super) async fn open(
    State(state): State<AppState>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<Json<OpenedTurn>, ApiError> {
    let config = state.config();
    let runtime = runtime_provider(&headers, &config)?;
    if !runtime.enabled {
        return Err(ApiError::forbidden("runtime provider is disabled"));
    }
    let runtime = runtime.id.clone();
    let opening: TurnOpening = body(&bytes, "turn opening")?;
    Ok(Json(
        state
            .core
            .blocking(move |core| core.open_turn(&runtime, &opening))
            .await?,
    ))
}

/// The turns core records running on the calling runtime, enabled or not,
/// for it to take up again once its process restarts: each with the request
/// core handed it, and the key of a turn it opened.
pub(super) async fn running(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<RunningTurn>>, ApiError> {
    let runtime = runtime_provider(&headers, &state.config())?.id.clone();
    Ok(Json(state.core.running_turns(&runtime)?))
}

/// How long a dispatcher waits after its first failure, doubling with each
/// failure after it up to `LONGEST_RETRY`.
const FIRST_RETRY: Duration = Duration::from_secs(1);
const LONGEST_RETRY: Duration = Duration::from_secs(60);

/// A kind of failure a run logs once: the runtime's inbox failing one way,
/// or core's own failure.
type Kind = Option<Discriminant<InboxFailure>>;

/// A dispatcher's run of failures, while it lasts: the kind it last logged,
/// and how long it waits after the next.
#[derive(Debug, Default)]
pub(super) struct Failing(Option<(Kind, Duration)>);

impl Failing {
    /// How long to wait after `stalled`, and whether it is a new kind of
    /// failure, to be logged.
    pub(super) fn after(&mut self, stalled: &Stalled) -> (Duration, bool) {
        let kind = match stalled {
            Stalled::Inbox(failure) => Some(mem::discriminant(failure)),
            Stalled::Core(_) => None,
        };
        let (wait, new) = self
            .0
            .map_or((FIRST_RETRY, true), |(was, wait)| (wait, was != kind));
        self.0 = Some((kind, (wait * 2).min(LONGEST_RETRY)));
        (wait, new)
    }

    /// Ends the run of failures; whether there was one.
    pub(super) fn recovered(&mut self) -> bool {
        self.0.take().is_some()
    }
}

/// Hands a runtime its queued turns, one at a time and in order,
/// until `stopping` is cancelled. While it cannot, it backs off, logging
/// each new kind of failure once and the recovery once.
pub(super) async fn dispatcher(state: AppState, runtime_id: String, stopping: CancellationToken) {
    let mut work = state.core.work();
    let mut failing = Failing::default();
    while !stopping.is_cancelled() {
        work.mark_unchanged();
        let wait = match dispatch_next(&state, &runtime_id, &stopping).await {
            Ok(step @ (WorkerStep::Advanced | WorkerStep::Idle)) => {
                if failing.recovered() {
                    info!(runtime_id, "runtime provider takes turns again");
                }
                if step == WorkerStep::Idle {
                    // The sender lives as long as `state`, which this task holds.
                    let _ = stopping.run_until_cancelled(work.changed()).await;
                }
                continue;
            }
            Ok(WorkerStep::Backoff) => FIRST_RETRY,
            Err(stalled) => {
                let (wait, new) = failing.after(&stalled);
                match &stalled {
                    _ if !new => {}
                    Stalled::Inbox(InboxFailure::Misinstalled(_)) => {
                        error!(runtime_id, error = %stalled, "queued turns wait for the runtime provider's installation to be fixed");
                    }
                    Stalled::Inbox(_) => {
                        warn!(runtime_id, error = %stalled, "queued turns wait for the runtime provider");
                    }
                    Stalled::Core(_) => {
                        error!(runtime_id, error = %stalled, "queued turns wait for core's registry or storage");
                    }
                }
                wait
            }
        };
        stopping.run_until_cancelled(tokio::time::sleep(wait)).await;
    }
}

/// One step of a runtime's dispatcher: its oldest queued turn,
/// handed over.
pub(super) async fn dispatch_next(
    state: &AppState,
    runtime_id: &str,
    stopping: &CancellationToken,
) -> Result<WorkerStep, Stalled> {
    let config = state.config();
    let runtime = config.provider(runtime_id, ProviderRole::Runtime)?;
    if !runtime.enabled {
        return Ok(WorkerStep::Backoff);
    }
    let client = RuntimeClient::new(
        state.client.clone(),
        runtime.url.clone(),
        runtime_token(&config, runtime)?,
    );
    state
        .core
        .dispatch_next(runtime_id, stopping, async |request| {
            client.submit(&request).await
        })
        .await
}
