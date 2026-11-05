//! The running provider is the only program that changes its settings file:
//! `/settings`, `carillon settings`, setup and module changes all reach it
//! here. One change at a time reads the file, refuses it when it differs
//! from what serves, applies the change, validates the result, writes it
//! atomically and serves it. A file renamed into place serves even when its
//! directory did not sync, so what serves is always the file. A change to
//! what only a restart applies is written without serving, and later
//! changes build on that file. Only while the provider is stopped does the
//! CLI write the file itself, under the installation's maintenance lock.

use std::{path::PathBuf, sync::Arc};

use anyhow::{Context as _, Result, anyhow};
use axum::http::StatusCode;
use carillon_installation::{config::LoadedConfig, maintenance::MaintenanceLock};
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{MutexGuard, oneshot},
    time::{Instant, timeout_at},
};
use tracing::{error, info, warn};

use super::{Difference, DiscordConfig, DiscordRoute, Settings, WriteError};
use crate::{
    InterfaceError,
    error::discord_failure_is_terminal,
    module::{Change, ChangeError},
};

/// A change the CLI asks of the settings file: the running provider applies
/// it, and a stopped provider's file is changed in place.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "change", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum FileChange {
    /// Who is admitted where, as setup asks it, keeping everything else.
    Admission {
        guild_id: String,
        bot_user_id: String,
        /// The bot token's file, when setup wrote a new one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bot_token_file: Option<PathBuf>,
        allowed_user_ids: Vec<String>,
        routes: Routes,
        /// The agent that answers direct messages, unless the settings
        /// already name one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        direct_messages: Option<String>,
    },
    /// Whether the custom module serves, and its file when given; without
    /// one the file keeps the path it declares.
    Module {
        enabled: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<PathBuf>,
    },
}

/// The routes an admission change brings.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "routes", content = "list", rename_all = "snake_case")]
pub(crate) enum Routes {
    /// Guided setup's answers, which replace the routes.
    Answered(Vec<DiscordRoute>),
    /// Unattended setup's first routes: accepted while the file routes no
    /// channel or routes exactly these, so a repeated install changes
    /// nothing, and refused otherwise.
    First(Vec<DiscordRoute>),
}

impl FileChange {
    /// Applies the change to `discord`.
    ///
    /// # Errors
    ///
    /// Refuses first routes that would replace other routes.
    pub(crate) fn apply(self, discord: &mut DiscordConfig) -> Result<(), Refused> {
        match self {
            Self::Admission {
                guild_id,
                bot_user_id,
                bot_token_file,
                allowed_user_ids,
                routes,
                direct_messages,
            } => {
                let routes = match routes {
                    Routes::Answered(routes) => {
                        in_order_of(routes, &discord.routes, |one, other| {
                            one.channel_id == other.channel_id
                        })
                    }
                    Routes::First(routes) if discord.routes.is_empty() => routes,
                    Routes::First(routes) if same_routes(&discord.routes, &routes) => {
                        discord.routes.clone()
                    }
                    Routes::First(_) => {
                        return Err(Refused::Invalid(
                            "--route only creates the first routes, and the Discord settings \
                             already route channels; change a channel's route with \
                             `carillon settings <channel> agent=… listens=… replies=…` or \
                             /settings in Discord"
                                .to_owned(),
                        ));
                    }
                };
                discord.guild_id = guild_id;
                discord.bot_user_id = bot_user_id;
                if bot_token_file.is_some() {
                    discord.bot_token_file = bot_token_file;
                }
                discord.allowed_user_ids =
                    in_order_of(allowed_user_ids, &discord.allowed_user_ids, PartialEq::eq);
                discord.routes = routes;
                if discord.direct_messages.is_none() {
                    discord.direct_messages =
                        direct_messages.map(|agent| super::DirectMessages { agent });
                }
            }
            Self::Module { enabled, path } => {
                discord.module.enabled = enabled;
                if path.is_some() {
                    discord.module.path = path;
                }
            }
        }
        Ok(())
    }

    /// Applies the change to the file of a provider that is not running,
    /// under the installation's maintenance lock, as the provider would.
    /// Without a file, setup's admission starts from the defaults.
    ///
    /// # Errors
    ///
    /// Returns why the file is unchanged: another maintenance operation
    /// holds the lock, the file does not read, the change is refused or the
    /// result is not valid.
    pub(crate) fn write(self, config: &LoadedConfig, provider_id: &str) -> Result<()> {
        let unchanged = "the Discord settings were not changed";
        let _maintenance = MaintenanceLock::acquire_unless_held(&config.root).context(unchanged)?;
        let config = &config.reload().context(unchanged)?;
        let mut discord = file_or_defaults(config, provider_id).context(unchanged)?;
        self.apply(&mut discord)
            .map_err(|refused| anyhow!("{}", InterfaceError::from(refused).message))?;
        discord
            .write(config, provider_id)
            .map_err(|error| match error {
                WriteError::NotWritten(error) => error.context(unchanged),
                WriteError::Unsynced(error) => error,
            })
    }
}

fn in_order_of<T>(answered: Vec<T>, settings: &[T], same: impl Fn(&T, &T) -> bool) -> Vec<T> {
    let mut taken = vec![false; settings.len()];
    let mut placed: Vec<(usize, T)> = answered
        .into_iter()
        .map(|answer| {
            let place =
                (0..settings.len()).find(|&place| !taken[place] && same(&settings[place], &answer));
            if let Some(place) = place {
                taken[place] = true;
            }
            (place.unwrap_or(usize::MAX), answer)
        })
        .collect();
    placed.sort_by_key(|(place, _)| *place);
    placed.into_iter().map(|(_, answer)| answer).collect()
}

fn same_routes(left: &[DiscordRoute], right: &[DiscordRoute]) -> bool {
    fn sorted(routes: &[DiscordRoute]) -> Vec<&DiscordRoute> {
        let mut sorted: Vec<&DiscordRoute> = routes.iter().collect();
        sorted.sort_by(|one, other| one.channel_id.cmp(&other.channel_id));
        sorted
    }
    sorted(left) == sorted(right)
}

/// The provider's settings file, or the defaults before setup wrote one.
///
/// # Errors
///
/// Returns why an existing file does not read.
pub(crate) fn file_or_defaults(config: &LoadedConfig, provider_id: &str) -> Result<DiscordConfig> {
    Ok(if config.provider_config_path(provider_id)?.exists() {
        DiscordConfig::read(config, provider_id)?
    } else {
        DiscordConfig::with_defaults(String::new(), String::new(), Vec::new(), Vec::new())
    })
}

/// The settings file of one provider, changed through what it serves.
#[derive(Clone)]
pub(crate) struct Writer {
    settings: Settings,
    installation: Arc<LoadedConfig>,
    provider_id: Arc<str>,
}

/// Writes a change's settings while the module change it declares holds
/// the maintenance lock, before that module serves.
pub(crate) struct Commit {
    installation: Arc<LoadedConfig>,
    provider_id: Arc<str>,
    settings: DiscordConfig,
    /// Tells the writer the file is in place but not confirmed durable.
    unsynced: oneshot::Sender<anyhow::Error>,
}

impl Commit {
    /// Writes the settings. A file in place whose directory did not sync is
    /// written: the module it declares serves, and the writer reports it.
    ///
    /// # Errors
    ///
    /// Returns why the settings file is as it was.
    pub(crate) fn write(self) -> Result<()> {
        match self.settings.write(&self.installation, &self.provider_id) {
            Ok(()) => Ok(()),
            Err(WriteError::NotWritten(error)) => Err(error),
            Err(WriteError::Unsynced(error)) => {
                let _ = self.unsynced.send(error);
                Ok(())
            }
        }
    }
}

/// What a change wrote: the edit's own answer, the fields it changed, those
/// only a restart serves, and the module's change when it declared another.
#[derive(Debug)]
pub(crate) struct Written<T> {
    pub value: T,
    pub changed: Vec<String>,
    /// The file was written and nothing of it served: it changes fields only
    /// a restart applies.
    pub restart: Vec<String>,
    pub module: Option<Change>,
    /// The file is in place, and serves unless it waits for a restart, but
    /// its directory did not sync, so a system crash may bring back the
    /// previous file.
    pub unsynced: Option<anyhow::Error>,
}

/// Why a change left the file and what serves as they were.
#[derive(Debug)]
pub(crate) enum Refused {
    Unreadable(anyhow::Error),
    /// The file differs from what serves: edits nobody applied.
    Unapplied(Difference),
    /// The setting is no longer as the change was drawn.
    Stale,
    /// The change is not one the settings accept.
    Invalid(String),
    Module(ChangeError),
    Write(anyhow::Error),
    /// Another change held the file past the change's deadline.
    Busy,
}

impl From<Refused> for InterfaceError {
    fn from(refused: Refused) -> Self {
        let (status, message) = match refused {
            Refused::Unreadable(error) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("the settings file is not valid: {error:#}; nothing was changed"),
            ),
            // Not a conflict with the change itself, which a card shows as
            // changed meanwhile: the file it would apply to is not served.
            Refused::Unapplied(difference) => (
                StatusCode::PRECONDITION_FAILED,
                format!(
                    "the settings file has changes that are not served ({}); {} serves them. \
                     Nothing was changed",
                    difference
                        .live
                        .iter()
                        .chain(&difference.restart)
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(", "),
                    if difference.restart.is_empty() {
                        "`carillon discord settings apply`"
                    } else {
                        "`carillon restart`"
                    }
                ),
            ),
            Refused::Stale => (
                StatusCode::CONFLICT,
                "Changed meanwhile; nothing was changed".to_owned(),
            ),
            Refused::Invalid(reason) => (StatusCode::UNPROCESSABLE_ENTITY, reason),
            Refused::Module(failure) => module_refusal(failure),
            Refused::Write(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("could not write the settings file: {error:#}"),
            ),
            Refused::Busy => (
                StatusCode::GATEWAY_TIMEOUT,
                "another settings change is still in progress; nothing was changed".to_owned(),
            ),
        };
        Self { status, message }
    }
}

impl Writer {
    pub(crate) fn new(
        settings: Settings,
        installation: Arc<LoadedConfig>,
        provider_id: impl Into<Arc<str>>,
    ) -> Self {
        Self {
            settings,
            installation,
            provider_id: provider_id.into(),
        }
    }

    pub(crate) fn installation(&self) -> &LoadedConfig {
        &self.installation
    }

    pub(crate) fn current(&self) -> Arc<DiscordConfig> {
        self.settings.current()
    }

    /// Changes the file with `edit`, which never changes the module, and
    /// serves the result, unless another change holds the file until
    /// `until`.
    ///
    /// # Errors
    ///
    /// Returns why the file and what serves are unchanged.
    pub(crate) async fn change<T>(
        &self,
        until: Instant,
        edit: impl FnOnce(&mut DiscordConfig) -> Result<T, Refused>,
    ) -> Result<Written<T>, Refused> {
        let written = timeout_at(until, self.settings.0.applying.lock())
            .await
            .map_err(|_| Refused::Busy)?;
        // A lock that frees while the executor is held past `until` still
        // reads as acquired in time.
        if Instant::now() >= until {
            return Err(Refused::Busy);
        }
        self.write(
            written,
            edit,
            None::<fn(&DiscordConfig, Commit) -> NoModule>,
        )
        .await
    }

    /// Changes the file with `edit` and serves the result, first serving the
    /// module it declares through `module`, which writes the file with the
    /// [`Commit`] it is given before that module serves.
    ///
    /// # Errors
    ///
    /// Returns why the file and what serves are unchanged.
    pub(crate) async fn declare<T, F>(
        &self,
        edit: impl FnOnce(&mut DiscordConfig) -> Result<T, Refused>,
        module: impl FnOnce(&DiscordConfig, Commit) -> F,
    ) -> Result<Written<T>, Refused>
    where
        F: Future<Output = Result<Change, ChangeError>>,
    {
        let written = self.settings.0.applying.lock().await;
        self.write(written, edit, Some(module)).await
    }

    /// Changes the file with `edit` while `written` holds it.
    async fn write<T, F>(
        &self,
        mut written: MutexGuard<'_, Option<Arc<DiscordConfig>>>,
        edit: impl FnOnce(&mut DiscordConfig) -> Result<T, Refused>,
        module: Option<impl FnOnce(&DiscordConfig, Commit) -> F>,
    ) -> Result<Written<T>, Refused>
    where
        F: Future<Output = Result<Change, ChangeError>>,
    {
        let serving = self.settings.current();
        let file = DiscordConfig::load(&self.installation, &self.provider_id)
            .map_err(Refused::Unreadable)?;
        let differs = |from: &DiscordConfig| {
            let difference = Difference::between(from, &file);
            !difference.live.is_empty() || !difference.restart.is_empty()
        };
        if differs(&serving) && written.as_deref().is_none_or(differs) {
            return Err(Refused::Unapplied(Difference::between(&serving, &file)));
        }
        let mut next = file.clone();
        let value = edit(&mut next)?;
        next.validate(&self.installation, &self.provider_id)
            .map_err(|error| Refused::Invalid(format!("{error:#}; nothing was changed")))?;
        let mut difference = Difference::between(&serving, &next);
        // Only setup's own change waits for a restart; a route or module
        // change must serve, so it waits until the restart has.
        if !difference.restart.is_empty() && (module.is_none() || next.module != file.module) {
            return Err(Refused::Unapplied(Difference {
                live: Vec::new(),
                restart: difference.restart,
            }));
        }
        let (module, unsynced) = match module {
            Some(module) if difference.restart.is_empty() => {
                let (unsynced, mut committed) = oneshot::channel();
                let commit = Commit {
                    installation: Arc::clone(&self.installation),
                    provider_id: Arc::clone(&self.provider_id),
                    settings: next.clone(),
                    unsynced,
                };
                let module = module(&next, commit)
                    .await
                    .map_err(|failure| match failure {
                        ChangeError::Commit(error) => Refused::Write(error),
                        failure => Refused::Module(failure),
                    })?;
                // The module's file decides as well as its declaration, so
                // a module rebuilt at the same path serves too.
                if module.serving != module.previous
                    && !difference.live.iter().any(|field| field == "module")
                {
                    difference.live.push("module".to_owned());
                }
                (Some(module), committed.try_recv().ok())
            }
            _ => match next.write(&self.installation, &self.provider_id) {
                Ok(()) => (None, None),
                Err(WriteError::Unsynced(error)) => (None, Some(error)),
                Err(WriteError::NotWritten(error)) => return Err(Refused::Write(error)),
            },
        };
        if let Some(error) = &unsynced {
            warn!(
                error = format!("{error:#}"),
                "Discord settings written, but not confirmed durable"
            );
        }
        if difference.restart.is_empty() {
            if !difference.live.is_empty() {
                info!(
                    changed = difference.live.join(", "),
                    "Discord settings changed"
                );
                self.settings.0.current.send_replace(Arc::new(next));
            }
            *written = None;
        } else {
            *written = Some(Arc::new(next));
        }
        Ok(Written {
            value,
            changed: if difference.restart.is_empty() {
                difference.live
            } else {
                Vec::new()
            },
            restart: difference.restart,
            module,
            unsynced,
        })
    }
}

type NoModule = std::future::Ready<Result<Change, ChangeError>>;

pub(crate) fn module_refusal(failure: ChangeError) -> (StatusCode, String) {
    error!(error = %failure, "Discord module change refused");
    match failure {
        ChangeError::Busy(installation) => (
            StatusCode::CONFLICT,
            format!(
                "another Carillon maintenance operation is running for {}; nothing was applied. \
                 Apply again once it finishes",
                installation.display()
            ),
        ),
        ChangeError::Lock(failure) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("could not take the maintenance lock: {failure:#}; nothing was applied"),
        ),
        ChangeError::Unusable(failure) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("not a usable Discord module: {failure:#}; nothing was applied"),
        ),
        ChangeError::Record(failure) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "could not clear the command registration record ({failure}), so Discord was \
                 not contacted and nothing was applied. Check the provider's state directory, \
                 then apply again"
            ),
        ),
        ChangeError::Register(failure) if discord_failure_is_terminal(&failure) => (
            StatusCode::BAD_GATEWAY,
            format!(
                "Discord rejected the module's commands: {failure:#}. Nothing was applied; \
                 correct the module's command definitions and apply again"
            ),
        ),
        ChangeError::Commit(failure) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("could not write the settings file: {failure:#}; nothing was applied"),
        ),
        ChangeError::Register(failure) => (
            StatusCode::GATEWAY_TIMEOUT,
            format!(
                "Discord did not confirm the module's commands: {failure:#}. It may have \
                 applied them; nothing else was applied. Apply again once Discord answers"
            ),
        ),
    }
}
