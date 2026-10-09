//! The runtime workers core runs, kept to what the serving registry says:
//! each enabled runtime gets its `parallel_turns` message workers and a
//! fixed few observation workers, and a disabled runtime none. One
//! supervisor owns them. When the registry changes it starts the workers a
//! lane lacks and retires those it no longer wants: a retired worker claims
//! nothing more, and the turn it holds settles as at shutdown. A lane gets
//! new workers only as retired ones end, so it never runs more turns at once
//! than the registry has allowed it since.

use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

use carillon_installation::config::{LoadedConfig, ProviderRole};
use serde::{Deserialize, Serialize};
use tokio::{
    sync::watch,
    task::{Id, JoinSet},
};
use tokio_util::sync::CancellationToken;
use tracing::error;

use super::{AppState, runtime_worker};
use crate::Lane;

/// How many observations of one runtime core follows at once. More wait in
/// the queue; messages are unaffected.
pub(super) const OBSERVATION_WORKERS: usize = 4;

/// The workers of one runtime's lane.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct Workers {
    /// Workers claiming turns.
    pub(crate) claiming: usize,
    /// Retired workers still settling the turn they hold.
    pub(crate) settling: usize,
}

/// The workers of one runtime, by lane.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct RuntimeWorkers {
    pub(crate) messages: Workers,
    pub(crate) observations: Workers,
}

/// Every runtime's workers, as the supervisor arranged them for `registry`.
#[derive(Clone, Debug, Default)]
pub(super) struct Census {
    pub(super) registry: Option<Arc<LoadedConfig>>,
    pub(super) runtimes: BTreeMap<String, RuntimeWorkers>,
}

/// The workers the registry asks for, by runtime and lane.
fn wanted(config: &LoadedConfig) -> BTreeMap<(String, Lane), usize> {
    let mut wanted = BTreeMap::new();
    for runtime in config
        .value
        .providers
        .iter()
        .filter(|provider| provider.role == ProviderRole::Runtime)
    {
        let (messages, observations) = if runtime.enabled {
            (runtime.parallel_turns() as usize, OBSERVATION_WORKERS)
        } else {
            (0, 0)
        };
        wanted.insert((runtime.id.clone(), Lane::Messages), messages);
        wanted.insert((runtime.id.clone(), Lane::Observations), observations);
    }
    wanted
}

struct Worker {
    runtime: String,
    lane: Lane,
    retire: CancellationToken,
}

/// The one owner of the runtime workers.
struct Supervisor {
    state: AppState,
    census: Arc<watch::Sender<Census>>,
    stopping: CancellationToken,
    tasks: JoinSet<()>,
    workers: HashMap<Id, Worker>,
}

impl Supervisor {
    /// Starts the workers each lane lacks and retires those it no longer
    /// wants, counting retired workers still settling as the lane's own.
    fn arrange(&mut self, config: &Arc<LoadedConfig>) {
        let wanted = wanted(config);
        let mut lanes = wanted.keys().cloned().collect::<Vec<_>>();
        for worker in self.workers.values() {
            let lane = (worker.runtime.clone(), worker.lane);
            if !lanes.contains(&lane) {
                lanes.push(lane);
            }
        }
        for (runtime, lane) in lanes {
            let target = wanted.get(&(runtime.clone(), lane)).copied().unwrap_or(0);
            let (mut claiming, mut settling) = (Vec::new(), 0);
            for worker in self.workers.values() {
                if worker.runtime == runtime && worker.lane == lane {
                    if worker.retire.is_cancelled() {
                        settling += 1;
                    } else {
                        claiming.push(&worker.retire);
                    }
                }
            }
            for retire in claiming.iter().skip(target) {
                retire.cancel();
            }
            for _ in claiming.len() + settling..target {
                let retire = self.stopping.child_token();
                let task = self.tasks.spawn(runtime_worker(
                    self.state.clone(),
                    runtime.clone(),
                    lane,
                    retire.clone(),
                ));
                self.workers.insert(
                    task.id(),
                    Worker {
                        runtime: runtime.clone(),
                        lane,
                        retire,
                    },
                );
            }
        }
        self.publish(config);
    }

    /// Tells who asks how every runtime's workers stand.
    fn publish(&self, config: &Arc<LoadedConfig>) {
        let mut runtimes = wanted(config)
            .into_keys()
            .map(|(runtime, _)| (runtime, RuntimeWorkers::default()))
            .collect::<BTreeMap<_, _>>();
        for worker in self.workers.values() {
            let runtime = runtimes.entry(worker.runtime.clone()).or_default();
            let lane = match worker.lane {
                Lane::Messages => &mut runtime.messages,
                Lane::Observations => &mut runtime.observations,
            };
            if worker.retire.is_cancelled() {
                lane.settling += 1;
            } else {
                lane.claiming += 1;
            }
        }
        self.census.send_replace(Census {
            registry: Some(config.clone()),
            runtimes,
        });
    }

    fn ended(&mut self, ended: Result<(Id, ()), tokio::task::JoinError>) {
        let id = match ended {
            Ok((id, ())) => id,
            Err(failure) => {
                error!(error = %failure, "runtime worker ended abnormally");
                failure.id()
            }
        };
        self.workers.remove(&id);
    }
}

/// Keeps the workers to the serving registry until `stopping`, then waits
/// for every worker to settle the turn it holds. `census` tells who asks
/// how the workers stand.
pub(super) async fn supervise(
    state: AppState,
    census: Arc<watch::Sender<Census>>,
    stopping: CancellationToken,
) {
    let mut registry = state.registry.subscribe();
    let mut supervisor = Supervisor {
        state,
        census,
        stopping: stopping.clone(),
        tasks: JoinSet::new(),
        workers: HashMap::new(),
    };
    let config = registry.borrow_and_update().clone();
    supervisor.arrange(&config);
    loop {
        tokio::select! {
            () = stopping.cancelled() => break,
            changed = registry.changed() => {
                if changed.is_err() {
                    break;
                }
                let config = registry.borrow_and_update().clone();
                supervisor.arrange(&config);
            }
            Some(ended) = supervisor.tasks.join_next_with_id() => {
                supervisor.ended(ended);
                let config = registry.borrow().clone();
                supervisor.arrange(&config);
            }
        }
    }
    // Each worker's token is a child of `stopping`: none claims again, and
    // each settles the turn it holds.
    let config = registry.borrow().clone();
    while let Some(ended) = supervisor.tasks.join_next_with_id().await {
        supervisor.ended(ended);
        supervisor.publish(&config);
    }
}
