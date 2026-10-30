//! The runtime workers core runs, kept to what the serving registry says:
//! each enabled runtime gets one dispatcher, which hands it its queued
//! turns in order, and a disabled runtime none. One supervisor owns them.
//! When the registry changes it starts the dispatchers runtimes lack and
//! retires those it no longer wants: a retired dispatcher hands over nothing
//! more, and ends once the hand-over it is making has. A runtime gets a new
//! dispatcher only once its retired one has ended, so it never has two.
//!
//! While an operator's [`Pause`] is in force it runs none: every dispatcher
//! retires, and nothing is started until the pause ends or lapses.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Arc,
    time::Duration,
};

use carillon_installation::config::{LoadedConfig, ProviderRole};
use serde::{Deserialize, Serialize};
use tokio::{
    sync::watch,
    task::{Id, JoinSet},
    time::Instant,
};
use tokio_util::sync::CancellationToken;
use tracing::error;
use uuid::Uuid;

use super::{AppState, runtime};

/// How a runtime's dispatcher stands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Dispatcher {
    #[default]
    Stopped,
    /// Handing the runtime its queued turns.
    Dispatching,
    /// Retired, ending once the hand-over it is making has.
    Retiring,
}

/// Every runtime's dispatcher, as the supervisor arranged them for `registry`.
#[derive(Clone, Debug, Default)]
pub(super) struct Census {
    pub(super) registry: Option<Arc<LoadedConfig>>,
    /// The holder of the pause in force when they were arranged.
    pub(super) paused: Option<Uuid>,
    pub(super) runtimes: BTreeMap<String, Dispatcher>,
}

/// An operator's hold on the runtime workers while maintenance drains core.
/// Its holder renews it within each `lease`; unrenewed, it lapses at
/// `until` and the workers return.
#[derive(Clone, Copy, Debug)]
pub(super) struct Pause {
    pub(super) holder: Uuid,
    pub(super) lease: Duration,
    pub(super) until: Instant,
}

impl Pause {
    /// The one test of whether a pause is still in force, for its holder's
    /// renewals and for the supervisor alike.
    pub(super) fn live(&self, now: Instant) -> bool {
        now < self.until
    }
}

fn in_force(pause: &watch::Sender<Option<Pause>>) -> Option<Pause> {
    pause.borrow().filter(|pause| pause.live(Instant::now()))
}

/// The registry's runtimes, and of them, the enabled ones, which want a dispatcher.
fn runtimes(config: &LoadedConfig) -> impl Iterator<Item = (&str, bool)> {
    config
        .value
        .providers
        .iter()
        .filter(|provider| provider.role == ProviderRole::Runtime)
        .map(|provider| (provider.id.as_str(), provider.enabled))
}

struct Worker {
    runtime: String,
    retire: CancellationToken,
}

/// The one owner of the runtime workers.
struct Supervisor {
    state: AppState,
    census: Arc<watch::Sender<Census>>,
    stopping: CancellationToken,
    /// Parent of every dispatcher's token: cancelled by a pause, or by
    /// shutdown through `stopping`, it retires them all. Replaced when the
    /// pause ends.
    serving: CancellationToken,
    /// The holder of the pause the workers are arranged under.
    paused: Option<Uuid>,
    tasks: JoinSet<()>,
    workers: HashMap<Id, Worker>,
}

impl Supervisor {
    /// Starts the dispatchers runtimes lack and retires those it no longer
    /// wants, a retiring one counting as its runtime's. A pause in force
    /// wants none.
    fn arrange(&mut self, config: &Arc<LoadedConfig>) {
        self.paused = in_force(&self.state.pause).map(|pause| pause.holder);
        let wanted: BTreeSet<&str> = if self.paused.is_some() {
            self.serving.cancel();
            BTreeSet::new()
        } else {
            if self.serving.is_cancelled() {
                self.serving = self.stopping.child_token();
            }
            runtimes(config)
                .filter_map(|(runtime, enabled)| enabled.then_some(runtime))
                .collect()
        };
        for worker in self.workers.values() {
            if !wanted.contains(worker.runtime.as_str()) {
                worker.retire.cancel();
            }
        }
        for runtime in wanted {
            if self
                .workers
                .values()
                .any(|worker| worker.runtime == runtime)
            {
                continue;
            }
            let retire = self.serving.child_token();
            let task = self.tasks.spawn(runtime::dispatcher(
                self.state.clone(),
                runtime.to_owned(),
                retire.clone(),
            ));
            self.workers.insert(
                task.id(),
                Worker {
                    runtime: runtime.to_owned(),
                    retire,
                },
            );
        }
        self.publish(config);
    }

    fn publish(&self, config: &Arc<LoadedConfig>) {
        let mut runtimes = runtimes(config)
            .map(|(runtime, _)| (runtime.to_owned(), Dispatcher::Stopped))
            .collect::<BTreeMap<_, _>>();
        for worker in self.workers.values() {
            let dispatcher = if worker.retire.is_cancelled() {
                Dispatcher::Retiring
            } else {
                Dispatcher::Dispatching
            };
            runtimes.insert(worker.runtime.clone(), dispatcher);
        }
        self.census.send_replace(Census {
            registry: Some(config.clone()),
            paused: self.paused,
            runtimes,
        });
    }

    /// When the pause the workers are arranged under ends, to arrange them
    /// again then. A renewal moves `until` without notice, so waking at an
    /// earlier one finds the pause still in force. One that has ended, or
    /// that the pause no longer is, is due at once, however late this is
    /// asked; arranged under no pause, there is nothing to wake for.
    fn lapse(&self) -> Option<Instant> {
        let holder = self.paused?;
        Some(
            self.state
                .pause
                .borrow()
                .filter(|pause| pause.holder == holder)
                .map_or_else(Instant::now, |pause| pause.until),
        )
    }

    fn ended(&mut self, ended: Result<(Id, ()), tokio::task::JoinError>) {
        let id = match ended {
            Ok((id, ())) => id,
            Err(failure) => {
                error!(error = %failure, "runtime dispatcher ended abnormally");
                failure.id()
            }
        };
        self.workers.remove(&id);
    }
}

/// Keeps the dispatchers to the serving registry and the operator's pause
/// until `stopping`, then waits for every one to end its hand-over.
/// `census` tells who asks how they stand.
pub(super) async fn supervise(
    state: AppState,
    census: Arc<watch::Sender<Census>>,
    stopping: CancellationToken,
) {
    let mut registry = state.registry.subscribe();
    let mut pause = state.pause.subscribe();
    let mut supervisor = Supervisor {
        state,
        census,
        serving: stopping.child_token(),
        stopping: stopping.clone(),
        paused: None,
        tasks: JoinSet::new(),
        workers: HashMap::new(),
    };
    let config = registry.borrow_and_update().clone();
    pause.borrow_and_update();
    supervisor.arrange(&config);
    loop {
        let lapse = supervisor.lapse();
        tokio::select! {
            () = stopping.cancelled() => break,
            changed = registry.changed() => {
                if changed.is_err() {
                    break;
                }
                let config = registry.borrow_and_update().clone();
                supervisor.arrange(&config);
            }
            changed = pause.changed() => {
                if changed.is_err() {
                    break;
                }
                pause.borrow_and_update();
                let config = registry.borrow().clone();
                supervisor.arrange(&config);
            }
            () = tokio::time::sleep_until(lapse.unwrap_or_else(Instant::now)), if lapse.is_some() => {
                let config = registry.borrow().clone();
                supervisor.arrange(&config);
            }
            Some(ended) = supervisor.tasks.join_next_with_id() => {
                supervisor.ended(ended);
                let config = registry.borrow().clone();
                supervisor.arrange(&config);
            }
        }
    }
    // Each dispatcher's token descends from `stopping`: none hands over
    // another turn, and each ends its hand-over.
    let config = registry.borrow().clone();
    while let Some(ended) = supervisor.tasks.join_next_with_id().await {
        supervisor.ended(ended);
        supervisor.publish(&config);
    }
}
