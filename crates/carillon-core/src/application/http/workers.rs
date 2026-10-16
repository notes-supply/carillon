//! The runtime workers core runs, kept to what the serving registry says:
//! each enabled runtime gets its `parallel_turns` message workers and a
//! fixed few observation workers, and a disabled runtime none. One
//! supervisor owns them. When the registry changes it starts the workers a
//! lane lacks and retires those it no longer wants: a retired worker claims
//! nothing more, and the turn it holds settles as at shutdown. A lane gets
//! new workers only as retired ones end, so it never runs more turns at once
//! than the registry has allowed it since.
//!
//! While an operator's [`Pause`] is in force it runs none: every worker
//! retires, an observation in progress is set aside to be asked again, and
//! nothing is started until the pause ends or lapses.

use std::{
    collections::{BTreeMap, HashMap},
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

use super::{AppState, runtime_worker};
use crate::Lane;

/// How many observations of one runtime core follows at once. More wait in
/// the queue; messages are unaffected.
pub(super) const OBSERVATION_WORKERS: usize = 4;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct Workers {
    pub(crate) claiming: usize,
    /// Retired workers still settling the turn they hold.
    pub(crate) settling: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct RuntimeWorkers {
    pub(crate) messages: Workers,
    pub(crate) observations: Workers,
}

/// Every runtime's workers, as the supervisor arranged them for `registry`.
#[derive(Clone, Debug, Default)]
pub(super) struct Census {
    pub(super) registry: Option<Arc<LoadedConfig>>,
    /// The holder of the pause in force when they were arranged.
    pub(super) paused: Option<Uuid>,
    pub(super) runtimes: BTreeMap<String, RuntimeWorkers>,
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
    /// Parent of every claiming worker's token: cancelled by a pause, or
    /// by shutdown through `stopping`, it retires them all and sets aside
    /// the observations they run. Replaced when the pause ends.
    set_aside: CancellationToken,
    /// The holder of the pause the workers are arranged under.
    paused: Option<Uuid>,
    tasks: JoinSet<()>,
    workers: HashMap<Id, Worker>,
}

impl Supervisor {
    /// Starts the workers each lane lacks and retires those it no longer
    /// wants, counting retired workers still settling as the lane's own.
    /// A pause in force wants none.
    fn arrange(&mut self, config: &Arc<LoadedConfig>) {
        self.paused = in_force(&self.state.pause).map(|pause| pause.holder);
        let mut wanted = wanted(config);
        if self.paused.is_some() {
            self.set_aside.cancel();
            for workers in wanted.values_mut() {
                *workers = 0;
            }
        } else if self.set_aside.is_cancelled() {
            self.set_aside = self.stopping.child_token();
        }
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
                let retire = self.set_aside.child_token();
                let task = self.tasks.spawn(runtime_worker(
                    self.state.clone(),
                    runtime.clone(),
                    lane,
                    retire.clone(),
                    self.set_aside.clone(),
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
                error!(error = %failure, "runtime worker ended abnormally");
                failure.id()
            }
        };
        self.workers.remove(&id);
    }
}

/// Keeps the workers to the serving registry and the operator's pause until
/// `stopping`, then waits for every worker to settle the turn it holds.
/// `census` tells who asks how the workers stand.
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
        set_aside: stopping.child_token(),
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
    // Each worker's token descends from `stopping`: none claims again, and
    // each settles the turn it holds, or sets aside its observation.
    let config = registry.borrow().clone();
    while let Some(ended) = supervisor.tasks.join_next_with_id().await {
        supervisor.ended(ended);
        supervisor.publish(&config);
    }
}
