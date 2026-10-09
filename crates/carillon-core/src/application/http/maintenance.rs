//! The operator's pause of the runtime workers, so maintenance stops the
//! services only once nothing core runs would be interrupted. One holder at
//! a time pauses them for a lease it renews. While the pause is in force
//! the supervisor runs no worker: claimed messages settle and their answers
//! enter the outbox, observations are set aside to be asked again, and
//! queued work waits. Accepting events and delivering answers go on.
//!
//! A drain reports facts for the holder to judge: how many workers have not
//! yet ended, once the supervisor has arranged them for this very pause;
//! how many answers enabled interfaces have yet to settle; and, once no
//! worker remains, each runtime's own word on whether a restart loses
//! anything only its process knows. Nothing here is persisted: a restarted
//! core has no pause.

use std::time::Duration;

use axum::{
    Json,
    extract::{Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
};
use carillon_installation::config::{LoadedConfig, ProviderRole};
use carillon_protocol::model::RestartReadiness;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::{task::JoinSet, time::Instant};
use tracing::info;
use uuid::Uuid;

use super::{ApiError, AppState, Pause, authorize, runtime_token};
use crate::RuntimeClient;

/// The longest a holder may leave its pause unrenewed.
const LONGEST_LEASE: Duration = Duration::from_secs(60);

/// How long a pause lasts unless its holder renews it.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Lease {
    pub(crate) lease_ms: u64,
}

/// How a pause's drain stands.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Drain {
    pub(crate) holder: Uuid,
    /// Workers that may still claim or hold a turn, a retired one counting
    /// until its task ends; unknown until the supervisor has arranged the
    /// workers under this pause.
    pub(crate) workers: Option<usize>,
    /// Answers to enabled interfaces not yet delivered.
    pub(crate) undelivered: u64,
    /// Each runtime's word on a restart, asked once no worker remains.
    pub(crate) runtimes: Vec<RuntimeRestart>,
}

/// One runtime's word on a restart.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct RuntimeRestart {
    pub(crate) id: String,
    /// Why it is not ready: its own reason, or that it gave no readiness,
    /// which never counts as ready.
    pub(crate) unready: Option<String>,
}

/// Pauses the workers for a new holder, refused while another's is in force.
pub(super) async fn begin(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(lease): Json<Lease>,
) -> Result<Json<Drain>, ApiError> {
    authorize(&headers, &state.config().value.core.operator_token_hash)?;
    let lease = Duration::from_millis(lease.lease_ms);
    if lease.is_zero() || lease > LONGEST_LEASE {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!(
                "a pause lease lasts up to {} seconds",
                LONGEST_LEASE.as_secs()
            ),
        ));
    }
    let (holder, now) = (Uuid::now_v7(), Instant::now());
    let begun = state.pause.send_if_modified(|pause| {
        if pause.is_some_and(|pause| pause.live(now)) {
            return false;
        }
        *pause = Some(Pause {
            holder,
            lease,
            until: now + lease,
        });
        true
    });
    if !begun {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "another maintenance operation holds core's pause; one whose holder is gone lapses \
             within its lease",
        ));
    }
    info!(%holder, "runtime workers paused for maintenance");
    Ok(Json(drain(&state, holder).await?))
}

/// Renews the holder's pause and reports its drain. A pause that lapsed or
/// ended is not renewed: core may have claimed work since.
pub(super) async fn renew(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(holder): AxumPath<Uuid>,
) -> Result<Json<Drain>, ApiError> {
    authorize(&headers, &state.config().value.core.operator_token_hash)?;
    let now = Instant::now();
    let mut renewed = false;
    // Only `until` moves, so the supervisor is not told: it wakes at the
    // old lapse and finds the pause still in force.
    state.pause.send_if_modified(|pause| {
        if let Some(pause) = pause
            .as_mut()
            .filter(|pause| pause.holder == holder && pause.live(now))
        {
            pause.until = now + pause.lease;
            renewed = true;
        }
        false
    });
    if !renewed {
        return Err(not_in_force());
    }
    Ok(Json(drain(&state, holder).await?))
}

fn not_in_force() -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "this pause lapsed or ended; core's workers may have claimed work since",
    )
}

/// Ends the holder's pause, if it still holds one.
pub(super) async fn end(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(holder): AxumPath<Uuid>,
) -> Result<StatusCode, ApiError> {
    authorize(&headers, &state.config().value.core.operator_token_hash)?;
    let ended = state.pause.send_if_modified(|pause| {
        let held = pause.is_some_and(|pause| pause.holder == holder);
        if held {
            *pause = None;
        }
        held
    });
    if ended {
        info!(%holder, "runtime workers resumed");
    }
    Ok(StatusCode::NO_CONTENT)
}

/// The drain of the holder's pause. Asking the runtimes takes time: a pause
/// that lapsed or ended meanwhile is reported as such, never as drained.
async fn drain(state: &AppState, holder: Uuid) -> Result<Drain, ApiError> {
    let config = state.config();
    // Only an arrangement made under this pause counts: an earlier one's
    // census says nothing of workers started since it lapsed or ended.
    let workers = {
        let census = state.workers.borrow();
        (census.paused == Some(holder)).then(|| {
            census
                .runtimes
                .values()
                .flat_map(|runtime| [runtime.messages, runtime.observations])
                .map(|lane| lane.claiming + lane.settling)
                .sum()
        })
    };
    let interfaces = config
        .value
        .providers
        .iter()
        .filter(|provider| provider.role == ProviderRole::Interface && provider.enabled)
        .map(|provider| provider.id.as_str())
        .collect::<Vec<_>>();
    let undelivered = state.core.undelivered(&interfaces)?;
    let runtimes = if workers == Some(0) {
        restart_readiness(&state.client, &config).await
    } else {
        Vec::new()
    };
    let in_force = state
        .pause
        .borrow()
        .is_some_and(|pause| pause.holder == holder && pause.live(Instant::now()));
    if !in_force {
        return Err(not_in_force());
    }
    Ok(Drain {
        holder,
        workers,
        undelivered,
        runtimes,
    })
}

/// Asks every runtime of the registry at once, enabled or not: a disabled
/// runtime's program may still run and hold what a restart loses.
async fn restart_readiness(client: &Client, config: &LoadedConfig) -> Vec<RuntimeRestart> {
    let mut asked = JoinSet::new();
    let runtimes = config
        .value
        .providers
        .iter()
        .filter(|provider| provider.role == ProviderRole::Runtime);
    for (index, runtime) in runtimes.enumerate() {
        let token = runtime_token(config, runtime);
        let (client, url, id) = (client.clone(), runtime.url.clone(), runtime.id.clone());
        asked.spawn(async move {
            let answer = match token {
                Ok(token) => RuntimeClient::new(client, url, token)
                    .restart_readiness()
                    .await
                    .map_err(|error| format!("{error:#}")),
                Err(error) => Err(format!("{error:#}")),
            };
            let unready = match answer {
                Ok(RestartReadiness::Ready {}) => None,
                Ok(RestartReadiness::Unready { reason }) => Some(reason),
                Err(error) => Some(format!("it did not say whether it can restart ({error})")),
            };
            (index, RuntimeRestart { id, unready })
        });
    }
    let mut answers = asked.join_all().await;
    answers.sort_by_key(|(index, _)| *index);
    answers.into_iter().map(|(_, answer)| answer).collect()
}
