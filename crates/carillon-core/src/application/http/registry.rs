//! The registry core serves. The application owns the installation's
//! configuration and credentials: it loads `carillon.toml` at start, and an
//! apply replaces what serves with the file, whole, when the two differ only
//! in what applies live: whether a runtime is enabled. Core's engine
//! receives only the runtime facts derived from it, and the runtime workers
//! follow it.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use anyhow::Result;
use carillon_installation::config::{Config, CoreConfig, LoadedConfig, Provider, ProviderRole};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, watch};

use super::workers::{Census, Dispatcher};
use crate::{Core, RuntimeProvider};

/// The runtime facts core's engine admits and runs work by.
pub(super) fn runtime_facts(config: &LoadedConfig) -> Vec<RuntimeProvider> {
    config
        .value
        .providers
        .iter()
        .filter(|provider| provider.role == ProviderRole::Runtime)
        .map(|provider| RuntimeProvider {
            id: provider.id.clone(),
            version: provider.version.clone(),
            url: provider.url.clone(),
            enabled: provider.enabled,
        })
        .collect()
}

/// How a registry file differs from the registry that serves, by field:
/// what an apply would change, and what only a restart applies.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct Difference {
    pub(crate) live: Vec<String>,
    pub(crate) restart: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct ServingRuntime {
    pub(crate) id: String,
    /// Whether core admits work for it and hands it its queued turns.
    pub(crate) enabled: bool,
    pub(crate) dispatcher: Dispatcher,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct RegistryStatus {
    pub(crate) runtimes: Vec<ServingRuntime>,
    /// Whether maintenance holds the workers paused.
    pub(crate) paused: bool,
    pub(crate) file: FileState,
}

/// How the registry file stands beside the serving registry.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum FileState {
    /// It reads as a registry; these fields differ from what serves.
    Read(Difference),
    Invalid {
        error: String,
    },
}

/// A completed apply: the fields it changed, and the runtimes as they serve.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Applied {
    pub(crate) changed: Vec<String>,
    pub(crate) runtimes: Vec<ServingRuntime>,
    /// Whether maintenance holds the workers paused.
    pub(crate) paused: bool,
}

impl ServingRuntime {
    pub(super) fn all(config: &LoadedConfig, census: &Census) -> Vec<Self> {
        config
            .value
            .providers
            .iter()
            .filter(|provider| provider.role == ProviderRole::Runtime)
            .map(|provider| Self {
                id: provider.id.clone(),
                enabled: provider.enabled,
                dispatcher: census
                    .runtimes
                    .get(&provider.id)
                    .copied()
                    .unwrap_or_default(),
            })
            .collect()
    }
}

impl RegistryStatus {
    pub(super) fn of(config: &LoadedConfig, census: &Census, file: Result<Difference>) -> Self {
        Self {
            runtimes: ServingRuntime::all(config, census),
            paused: census.paused.is_some(),
            file: match file {
                Ok(difference) => FileState::Read(difference),
                Err(error) => FileState::Invalid {
                    error: format!("{error:#}"),
                },
            },
        }
    }
}

impl Difference {
    /// Every field that differs, classified. Fields are named as in the
    /// file; a provider's by its ID. Every field is listed here, so a new
    /// one must be classified before it compiles.
    pub(super) fn between(serving: &Config, file: &Config) -> Self {
        let mut difference = Self::default();
        let Config {
            data_dir,
            core,
            providers,
            agents,
        } = file;
        let CoreConfig {
            listen,
            operator_token_hash,
            attachments,
        } = core;
        let mut restart = |changed: bool, field: &str| {
            if changed {
                difference.restart.push(field.to_owned());
            }
        };
        restart(*data_dir != serving.data_dir, "data_dir");
        restart(*listen != serving.core.listen, "core.listen");
        restart(
            *operator_token_hash != serving.core.operator_token_hash,
            "core.operator_token_hash",
        );
        restart(*attachments != serving.core.attachments, "core.attachments");
        // Core admits for the agents it started with, and runtimes and
        // interfaces read theirs at start: any change is a restart's.
        restart(*agents != serving.agents, "agents");
        let before = serving
            .providers
            .iter()
            .map(|provider| (provider.id.as_str(), provider))
            .collect::<BTreeMap<_, _>>();
        let after = providers
            .iter()
            .map(|provider| (provider.id.as_str(), provider))
            .collect::<BTreeMap<_, _>>();
        for id in before.keys().filter(|id| !after.contains_key(*id)) {
            difference.restart.push(format!("providers.{id} (removed)"));
        }
        for (id, provider) in &after {
            match before.get(id) {
                None => difference.restart.push(format!("providers.{id} (added)")),
                Some(serving) => difference.provider(serving, provider),
            }
        }
        difference
    }

    fn provider(&mut self, serving: &Provider, file: &Provider) {
        let Provider {
            id,
            role,
            version,
            url,
            token_hash,
            enabled,
        } = file;
        let mut changed = |changed: bool, field: &str, live: bool| {
            if changed {
                let field = format!("providers.{id}.{field}");
                if live {
                    self.live.push(field);
                } else {
                    self.restart.push(field);
                }
            }
        };
        changed(*role != serving.role, "role", false);
        changed(*version != serving.version, "version", false);
        changed(*url != serving.url, "url", false);
        changed(*token_hash != serving.token_hash, "token_hash", false);
        // An interface's flag decides whether its program serves at all, and
        // core refuses a disabled interface's calls: only a restart of both
        // changes it without stranding the deliveries it holds.
        let runtime = *role == ProviderRole::Runtime && serving.role == ProviderRole::Runtime;
        changed(*enabled != serving.enabled, "enabled", runtime);
    }
}

/// Why an apply left the serving registry in place.
#[derive(Debug)]
pub(super) enum Refusal {
    Invalid(anyhow::Error),
    /// The file changes fields only a restart applies.
    Restart(Difference),
    /// Core's engine refused the derived runtime facts.
    Facts(anyhow::Error),
}

pub(super) struct Registry {
    path: PathBuf,
    serving: watch::Sender<Arc<LoadedConfig>>,
    /// One apply at a time, so the file each reads is the one that serves.
    applying: Mutex<()>,
}

impl Registry {
    pub(super) fn new(path: PathBuf, config: LoadedConfig) -> Self {
        Self {
            path,
            serving: watch::Sender::new(Arc::new(config)),
            applying: Mutex::new(()),
        }
    }

    /// The registry serving now. Each request reads it once.
    pub(super) fn current(&self) -> Arc<LoadedConfig> {
        self.serving.borrow().clone()
    }

    /// Changes whenever an apply replaces the serving registry.
    pub(super) fn subscribe(&self) -> watch::Receiver<Arc<LoadedConfig>> {
        self.serving.subscribe()
    }

    /// # Errors
    /// Returns why the file is not a valid registry.
    pub(super) fn compare(&self, serving: &LoadedConfig) -> Result<Difference> {
        let file = LoadedConfig::load(&self.path)?;
        Ok(Difference::between(&serving.value, &file.value))
    }

    /// Serves the file in place of the registry serving now, when they
    /// differ only in what applies live, after handing core's engine the
    /// runtime facts derived from it. Returns the fields it changed.
    /// # Errors
    /// Returns why the serving registry stays in place.
    pub(super) async fn apply(&self, core: &Core) -> Result<Vec<String>, Refusal> {
        let _one = self.applying.lock().await;
        let file = LoadedConfig::load(&self.path).map_err(Refusal::Invalid)?;
        let difference = Difference::between(&self.current().value, &file.value);
        if !difference.restart.is_empty() {
            return Err(Refusal::Restart(difference));
        }
        if difference.live.is_empty() {
            return Ok(Vec::new());
        }
        core.set_runtimes(runtime_facts(&file))
            .map_err(Refusal::Facts)?;
        self.serving.send_replace(Arc::new(file));
        Ok(difference.live)
    }
}
