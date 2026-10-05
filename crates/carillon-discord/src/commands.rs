//! Hosting of the replaceable command module: choosing the module to serve,
//! learning which commands it defines, registering those with Discord,
//! running each interaction through it with the native model operations it
//! may ask for, and installing or resetting the module while serving.
//!
//! Three facts have separate owners. Memory holds the generation serving now.
//! The disk holds what the next start serves: the operator-installed module
//! when one exists, otherwise the release's module. The registration record
//! holds what Discord last confirmed. Changes serialize on the installation's
//! maintenance lock, and every change converges these facts in one order:
//! validate, register when the record does not match, activate, then save.

use std::{
    fmt, fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};
use tokio::time::{Instant, timeout_at};

use anyhow::{Context as _, Result, bail};
use carillon_discord_commands::contract::{
    CONTRACT, Description, HostReply, HostRequest, Intent, Interaction, Invocation, Outcome, Reply,
};
use carillon_installation::{
    config::LoadedConfig,
    filesystem::{PublishError, atomic_write, publish, remove, sync_ancestors},
    maintenance::MaintenanceLock,
};
use carillon_protocol::model::{ModelQuery, ModelScope, ModelUpdate};
use carillon_wasm::{Host, Module, Native};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tracing::{info, warn};
use twilight_http::Client as DiscordHttp;
use twilight_model::{
    application::command::Command,
    channel::message::AllowedMentions,
    id::{
        Id,
        marker::{ApplicationMarker, ChannelMarker, GuildMarker},
    },
};

use crate::{
    client::CoreClient,
    config::DiscordConfig,
    conversations::{InteractionOrigin, submitted_event},
    delivery::{Feedback, admission_feedback, promptly, terminal_feedback},
};

/// The whole invocation, including every native call it makes.
const DEADLINE: Duration = Duration::from_secs(10);
/// Description is pure computation; a module that needs longer is broken.
const DESCRIBE_DEADLINE: Duration = Duration::from_secs(2);
/// The largest module an operator may install.
pub const MODULE_LIMIT: usize = 16 << 20;

/// The Discord application the provider acts as, once the token is known,
/// and the guild whose commands it owns.
pub struct DiscordApp {
    pub http: Arc<DiscordHttp>,
    pub application_id: Id<ApplicationMarker>,
    pub guild_id: Id<GuildMarker>,
}

/// The native operations one interaction may perform: model state and
/// selection for the agent and, inside a conversation, for that audience;
/// the runtime's commands there; and, with a Discord connection, submitting
/// text to the conversation as the person who invoked it.
pub struct InteractionCalls {
    core: CoreClient,
    query: ModelQuery,
    submissions: Option<Submissions>,
}

/// What submitting needs: the conversation's channel to anchor the text in,
/// and Discord to post that anchor and its feedback.
pub struct Submissions {
    pub http: Arc<DiscordHttp>,
    pub config: DiscordConfig,
    pub origin: InteractionOrigin,
}

impl InteractionCalls {
    pub const fn new(core: CoreClient, query: ModelQuery) -> Self {
        Self {
            core,
            query,
            submissions: None,
        }
    }

    #[must_use]
    pub fn with_submissions(mut self, submissions: Submissions) -> Self {
        self.submissions = Some(submissions);
        self
    }
}

/// Native calls for invocations that must not perform any.
enum Calls {
    Refused,
    Interaction(Box<InteractionCalls>),
}

impl Native for Calls {
    async fn call(&mut self, request: Vec<u8>) -> Vec<u8> {
        let reply = match self {
            Self::Refused => HostReply::Error("native calls are not available".to_owned()),
            Self::Interaction(calls) => calls.answer(&request).await,
        };
        serde_json::to_vec(&reply).unwrap_or_default()
    }
}

impl InteractionCalls {
    async fn answer(&self, request: &[u8]) -> HostReply {
        let outcome = match serde_json::from_slice::<HostRequest>(request) {
            Ok(HostRequest::Commands) => {
                return match self.core.commands(&self.query).await {
                    Ok(catalog) => HostReply::Commands(catalog),
                    Err(message) => HostReply::Error(message),
                };
            }
            Ok(HostRequest::Submit { text, intent }) => {
                return match (&self.submissions, &self.query.audience_id) {
                    (Some(submissions), Some(audience_id)) => {
                        match submit(
                            &self.core,
                            submissions,
                            &self.query.agent_id,
                            audience_id,
                            &text,
                            intent,
                        )
                        .await
                        {
                            Ok(()) => HostReply::Submitted,
                            Err(message) => HostReply::Error(message),
                        }
                    }
                    (_, None) => {
                        HostReply::Error("There is no conversation here to send it to.".to_owned())
                    }
                    (None, Some(_)) => HostReply::Error(
                        "Sending to the conversation needs the Discord connection.".to_owned(),
                    ),
                };
            }
            Ok(HostRequest::ModelState) => self.core.model_state(&self.query).await,
            Ok(HostRequest::SetModel {
                scope,
                selection,
                expected,
            }) => {
                if scope == ModelScope::Conversation && self.query.audience_id.is_none() {
                    return HostReply::Error(
                        "There is no conversation here to set a model for.".to_owned(),
                    );
                }
                self.core
                    .set_model(&ModelUpdate {
                        agent_id: self.query.agent_id.clone(),
                        audience_id: self.query.audience_id.clone(),
                        scope,
                        selection,
                        expected,
                    })
                    .await
            }
            Err(error) => Err(format!("invalid host request: {error}")),
        };
        match outcome {
            Ok(state) => HostReply::ModelState(state),
            Err(message) => HostReply::Error(message),
        }
    }
}

/// How much of the module's deadline a submission may take, leaving the
/// module time to answer with its outcome.
const SUBMIT_LIMIT: Duration = Duration::from_secs(7);
/// How long posting the marker may take inside the submission's limit.
const MARKER_LIMIT: Duration = Duration::from_secs(4);
/// Attempts to hand a submission to core while core is briefly unreachable.
const SUBMIT_ATTEMPTS: u32 = 3;

/// Sends text to the conversation as the invoking person's: a silent marker
/// message in the conversation shows what was sent and by whom, and is the
/// event's identity and feedback target; core then accepts the event like
/// any message, and the answer is delivered to the conversation.
///
/// Everything happens within [`SUBMIT_LIMIT`], so the module can still say
/// what happened: feedback is bounded and never delays admission, and an
/// attempt core has not answered in time may have been accepted, which the
/// answer says; the marker's ID makes a repeated submission the same event.
async fn submit(
    core: &CoreClient,
    submissions: &Submissions,
    agent_id: &str,
    audience_id: &str,
    text: &str,
    intent: Intent,
) -> Result<(), String> {
    let Submissions {
        http,
        config,
        origin,
    } = submissions;
    let channel: Id<ChannelMarker> = origin
        .channel_id
        .parse()
        .map_err(|_| "The conversation's channel is not valid.".to_owned())?;
    let deadline = Instant::now() + SUBMIT_LIMIT;
    let shown = text.replace('`', "ˋ");
    let shown: String = shown.chars().take(1900).collect();
    let silent = AllowedMentions::default();
    // The marker's answer, body included, is bounded: it is the event's identity.
    let marker = tokio::time::timeout(MARKER_LIMIT, async {
        http.create_message(channel)
            .allowed_mentions(Some(&silent))
            .content(&format!("<@{}> ran `{shown}`", origin.user_id))
            .await
            .map_err(|error| error.to_string())?
            .model()
            .await
            .map_err(|error| error.to_string())
    })
    .await
    .unwrap_or_else(|_| Err("Discord did not answer in time".to_owned()))
    .map_err(|error| {
        warn!(%error, "command marker message could not be posted");
        "Discord did not accept the message that carries it, so it was not sent.".to_owned()
    })?;
    let event = submitted_event(
        agent_id,
        audience_id,
        origin,
        &marker.id.to_string(),
        text,
        intent,
    );
    admission_feedback(http, config, &event).await;
    let retry = Duration::from_millis(500);
    let mut attempt = 1;
    let (feedback, refusal) = loop {
        let failure = match timeout_at(deadline, core.submit_event(&event)).await {
            Ok(Ok(_)) => return Ok(()),
            Ok(Err(failure)) if !failure.is_transient() => {
                warn!(error = %failure.message, event_id = %event.event_id, "core refused a command");
                break (Feedback::Failed, "Carillon could not accept it.");
            }
            Ok(Err(failure)) => failure.message,
            Err(_) => "core did not answer in time".to_owned(),
        };
        if attempt == SUBMIT_ATTEMPTS || Instant::now() + retry >= deadline {
            warn!(error = %failure, event_id = %event.event_id, "command submission unconfirmed");
            break (
                Feedback::Uncertain,
                "Carillon did not confirm that it accepted it; if it did, the answer follows in this conversation.",
            );
        }
        warn!(error = %failure, event_id = %event.event_id, "waiting to submit a command");
        attempt += 1;
        tokio::time::sleep(retry).await;
    };
    promptly(
        terminal_feedback(http, config, &event.feedback_target_id, feedback),
        "command feedback",
    )
    .await;
    Err(refusal.to_owned())
}

/// Where a module comes from.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// The module shipped with the release, served when none is installed.
    Release,
    /// The module an operator installed, which takes precedence.
    Installed,
}

impl fmt::Display for Source {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Release => "release",
            Self::Installed => "installed",
        })
    }
}

/// The files of one provider instance's commands.
#[derive(Clone, Debug)]
pub struct CommandFiles {
    /// The release's module; release updates replace it.
    pub release: PathBuf,
    /// The operator-installed module, outside the release's files.
    pub installed: PathBuf,
    /// What Discord last confirmed as registered.
    pub record: PathBuf,
    /// The installation whose maintenance lock serializes changes.
    pub installation: PathBuf,
}

impl CommandFiles {
    /// The files of a configured instance whose release module is `release`.
    pub fn of(config: &LoadedConfig, provider_id: &str, release: PathBuf) -> Self {
        let state = crate::config::state_directory(config, provider_id);
        Self {
            release,
            installed: state.join("command-module.wasm"),
            record: state.join("commands.json"),
            installation: config.root.clone(),
        }
    }

    /// The module the next start serves: the installed one when it exists.
    ///
    /// # Errors
    ///
    /// Returns an error when the chosen file cannot be read.
    pub fn selected(&self) -> Result<(Source, Vec<u8>)> {
        match fs::read(&self.installed) {
            Ok(bytes) => Ok((Source::Installed, bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let bytes = fs::read(&self.release).with_context(|| {
                    format!("read release command module {}", self.release.display())
                })?;
                Ok((Source::Release, bytes))
            }
            Err(error) => Err(error).with_context(|| {
                format!("read installed command module {}", self.installed.display())
            }),
        }
    }
}

/// One immutable module and the definitions it described. In-flight calls
/// retain their generation when a replacement starts serving.
struct Generation {
    module: Module,
    definitions: Vec<Value>,
    source: Source,
    digest: String,
}

impl Generation {
    fn summary(&self) -> ModuleSummary {
        ModuleSummary {
            source: self.source,
            commands: self
                .definitions
                .iter()
                .filter_map(|definition| definition["name"].as_str())
                .map(str::to_owned)
                .collect(),
            digest: self.digest.clone(),
        }
    }
}

/// A module as the operator sees it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ModuleSummary {
    pub source: Source,
    /// Command names, without the leading slash.
    pub commands: Vec<String>,
    /// SHA-256 of the module bytes.
    pub digest: String,
}

/// What a change needed from Discord.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Registration {
    /// The record already showed these definitions registered.
    Unchanged,
    /// Discord confirmed the definitions.
    Registered,
    /// Deterministic mode: there is no Discord application.
    NotConnected,
}

/// Whether a change also holds for the next start.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Saved {
    /// The next start serves the new module, even after a system crash.
    Durable,
    /// The saved file was not changed. The next start selects whatever was
    /// saved before, which may already be this module or may be unusable.
    Failed { error: String },
    /// The saved file was changed, but a system crash may still undo that.
    Unsynced { error: String },
}

impl Saved {
    /// The outcome of publishing or removing `path`.
    fn of(path: &Path, result: Result<(), PublishError>) -> Self {
        match result {
            Ok(()) => Self::Durable,
            Err(PublishError::NotPublished(error)) => Self::Failed {
                error: format!("{}: {error}", path.display()),
            },
            Err(PublishError::Unsynced(error)) => Self::Unsynced {
                error: format!("sync the directory of {}: {error}", path.display()),
            },
        }
    }
}

/// A completed install or reset: the new module serves.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Change {
    pub serving: ModuleSummary,
    pub previous: ModuleSummary,
    pub discord: Registration,
    pub saved: Saved,
}

/// What serves now, and what the next start would serve.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Status {
    pub serving: ModuleSummary,
    pub next_start: NextStart,
    /// Whether the record shows the serving definitions registered for this
    /// application and guild; absent without a Discord application.
    pub registered: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum NextStart {
    /// The module on disk is the one serving.
    Same,
    /// The next start would serve another module from this source.
    Different { source: Source },
    /// The module on disk could not be read.
    Unknown { error: String },
}

/// The operator's answer to a refused install or reset: why, with what to
/// do next, and the module that still serves.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Refusal {
    pub error: String,
    pub serving: ModuleSummary,
}

/// Why an install or reset left the serving module in place.
#[derive(Debug)]
pub enum ChangeError {
    /// Another maintenance operation holds the installation's lock.
    Busy(PathBuf),
    /// The maintenance lock could not be taken.
    Lock(anyhow::Error),
    /// The candidate is not a usable command module.
    Unusable(anyhow::Error),
    /// The registration record could not be cleared durably, so Discord was
    /// not contacted.
    Record(PublishError),
    /// Discord refused the definitions or did not confirm them.
    Register(anyhow::Error),
}

impl fmt::Display for ChangeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy(installation) => write!(
                formatter,
                "another Carillon maintenance operation is running for {}",
                installation.display()
            ),
            Self::Lock(error) => write!(formatter, "take the maintenance lock: {error:#}"),
            Self::Unusable(error) => write!(formatter, "{error:#}"),
            Self::Record(error) => {
                write!(formatter, "clear the command registration record: {error}")
            }
            Self::Register(error) => {
                write!(formatter, "register Discord guild commands: {error:#}")
            }
        }
    }
}

impl std::error::Error for ChangeError {}

/// What Discord last confirmed, bound to the application and guild it was
/// registered with; a record for another target does not count.
#[derive(Deserialize, Serialize)]
struct Record {
    application_id: Id<ApplicationMarker>,
    guild_id: Id<GuildMarker>,
    commands: Vec<Value>,
}

/// The module in use, its command definitions, and the files that decide
/// what the next start serves and what Discord holds.
pub struct Commands {
    host: Host<Calls>,
    files: CommandFiles,
    current: Mutex<Arc<Generation>>,
    /// Queues changes within this process; the maintenance lock refuses
    /// changes from other processes.
    changes: tokio::sync::Mutex<()>,
}

impl Commands {
    /// Loads the module the next start serves and learns its commands.
    ///
    /// # Errors
    ///
    /// Returns an error when the selected module is unusable; an unusable
    /// installed module is named so the operator can remove it.
    pub async fn load(files: CommandFiles) -> Result<Self> {
        let host = Host::new()?;
        let (source, bytes) = files.selected()?;
        let generation = prepare(&host, &bytes, source).await.map_err(|error| {
            if source == Source::Installed {
                error.context(format!(
                    "installed command module {} is unusable; remove it to serve the release's module",
                    files.installed.display()
                ))
            } else {
                error.context(format!(
                    "release command module {} is unusable",
                    files.release.display()
                ))
            }
        })?;
        Ok(Self {
            host,
            files,
            current: Mutex::new(Arc::new(generation)),
            changes: tokio::sync::Mutex::new(()),
        })
    }

    /// Serves `bytes` as the installed module and saves it for later starts.
    /// Repeating an install of the serving module still reconciles the
    /// registration and completes the save.
    ///
    /// # Errors
    ///
    /// Returns why the serving module was left in place.
    pub async fn install(
        &self,
        bytes: &[u8],
        app: Option<&DiscordApp>,
    ) -> Result<Change, ChangeError> {
        let _exclusive = self.exclusive().await?;
        self.apply(Source::Installed, bytes, app).await
    }

    /// Serves the release's module again and removes the installed one.
    ///
    /// # Errors
    ///
    /// Returns why the serving module was left in place.
    pub async fn reset(&self, app: Option<&DiscordApp>) -> Result<Change, ChangeError> {
        let _exclusive = self.exclusive().await?;
        let bytes = fs::read(&self.files.release)
            .with_context(|| {
                format!(
                    "read release command module {}",
                    self.files.release.display()
                )
            })
            .map_err(ChangeError::Unusable)?;
        self.apply(Source::Release, &bytes, app).await
    }

    /// Registers the serving definitions unless the record shows Discord
    /// already holds them.
    ///
    /// # Errors
    ///
    /// Returns why the definitions could not be registered.
    pub async fn reconcile(&self, app: &DiscordApp) -> Result<Registration, ChangeError> {
        let _changes = self.changes.lock().await;
        let definitions = self.current().definitions.clone();
        self.reconcile_definitions(Some(app), &definitions).await
    }

    /// The serving module, the one the next start would serve, and whether
    /// the record shows the serving definitions registered.
    pub fn status(&self, app: Option<&DiscordApp>) -> Status {
        let serving = self.current();
        let next_start = match self.files.selected() {
            Ok((source, bytes)) if source == serving.source && digest(&bytes) == serving.digest => {
                NextStart::Same
            }
            Ok((source, _)) => NextStart::Different { source },
            Err(error) => NextStart::Unknown {
                error: format!("{error:#}"),
            },
        };
        Status {
            serving: serving.summary(),
            next_start,
            registered: app.map(|app| self.registered(app, &serving.definitions)),
        }
    }

    pub fn serving(&self) -> ModuleSummary {
        self.current().summary()
    }

    /// Runs one interaction through the module.
    pub async fn invoke(&self, calls: InteractionCalls, interaction: Interaction) -> Result<Reply> {
        let generation = self.current();
        let payload = serde_json::to_vec(&Invocation::Interaction(interaction))?;
        match outcome(
            &self.host,
            &generation.module,
            Calls::Interaction(Box::new(calls)),
            &payload,
            DEADLINE,
        )
        .await?
        {
            Outcome::Reply(reply) => Ok(reply),
            Outcome::Failure(message) => bail!("command module failed: {message}"),
            Outcome::Description(_) => {
                bail!("command module answered an interaction with a description")
            }
        }
    }

    async fn exclusive(
        &self,
    ) -> Result<(tokio::sync::MutexGuard<'_, ()>, MaintenanceLock), ChangeError> {
        let changes = self.changes.lock().await;
        let lock = MaintenanceLock::try_acquire(&self.files.installation)
            .map_err(ChangeError::Lock)?
            .ok_or_else(|| ChangeError::Busy(self.files.installation.clone()))?;
        Ok((changes, lock))
    }

    /// Validate, register when needed, activate, then save. The caller holds
    /// both locks.
    async fn apply(
        &self,
        source: Source,
        bytes: &[u8],
        app: Option<&DiscordApp>,
    ) -> Result<Change, ChangeError> {
        let candidate = prepare(&self.host, bytes, source)
            .await
            .map_err(ChangeError::Unusable)?;
        let discord = self
            .reconcile_definitions(app, &candidate.definitions)
            .await?;
        let serving = candidate.summary();
        let previous = self.activate(candidate).summary();
        let saved = match source {
            Source::Installed => publish_directory(&self.files.installed)
                .map_err(PublishError::NotPublished)
                .and_then(|()| publish(&self.files.installed, bytes, 0o600)),
            Source::Release => publish_directory(&self.files.installed)
                .map_err(PublishError::NotPublished)
                .and_then(|()| remove(&self.files.installed)),
        };
        let saved = Saved::of(&self.files.installed, saved);
        if saved != Saved::Durable {
            warn!(?saved, path = %self.files.installed.display(), "command module serves but its saved state is not confirmed");
        }
        Ok(Change {
            serving,
            previous,
            discord,
            saved,
        })
    }

    async fn reconcile_definitions(
        &self,
        app: Option<&DiscordApp>,
        definitions: &[Value],
    ) -> Result<Registration, ChangeError> {
        let Some(app) = app else {
            return Ok(Registration::NotConnected);
        };
        if self.registered(app, definitions) {
            return Ok(Registration::Unchanged);
        }
        self.register(app, definitions).await?;
        Ok(Registration::Registered)
    }

    /// Replaces the module in use with the candidate, returning the previous one.
    fn activate(&self, candidate: Generation) -> Arc<Generation> {
        let source = candidate.source;
        let previous = std::mem::replace(
            &mut *self.current.lock().unwrap_or_else(PoisonError::into_inner),
            Arc::new(candidate),
        );
        info!(%source, "command module activated");
        previous
    }

    /// Whether Discord already holds `definitions` for this application and
    /// guild.
    fn registered(&self, app: &DiscordApp, definitions: &[Value]) -> bool {
        fs::read(&self.files.record)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Record>(&bytes).ok())
            .is_some_and(|record| {
                record.application_id == app.application_id
                    && record.guild_id == app.guild_id
                    && record.commands == definitions
            })
    }

    /// Replaces the guild's commands with `definitions`, durably clearing the
    /// record first and recording only Discord's confirmation. Recording
    /// cannot veto an already-confirmed registration.
    async fn register(&self, app: &DiscordApp, definitions: &[Value]) -> Result<(), ChangeError> {
        let commands = definitions
            .iter()
            .map(command)
            .collect::<Result<Vec<_>>>()
            .map_err(ChangeError::Unusable)?;
        // The record must not attest to the old definitions, even after a
        // system crash, if Discord applies the new ones and recording its
        // confirmation fails.
        publish_directory(&self.files.record)
            .map_err(PublishError::NotPublished)
            .and_then(|()| remove(&self.files.record))
            .map_err(ChangeError::Record)?;
        app.http
            .interaction(app.application_id)
            .set_guild_commands(app.guild_id, &commands)
            .await
            .map_err(|error| ChangeError::Register(error.into()))?;
        let record = Record {
            application_id: app.application_id,
            guild_id: app.guild_id,
            commands: definitions.to_vec(),
        };
        let recorded = serde_json::to_vec(&record)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| Ok(atomic_write(&self.files.record, &bytes, 0o600)?));
        if let Err(error) = recorded {
            // Discord confirmed the candidate. A record failure cannot undo
            // that mutation or justify keeping the previous module active.
            warn!(%error, path = %self.files.record.display(), "commands registered but the record could not be saved; registration will be repeated");
        }
        info!(
            commands = commands.len(),
            "Discord guild commands registered"
        );
        Ok(())
    }

    fn current(&self) -> Arc<Generation> {
        Arc::clone(&self.current.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// Validates `bytes` as a command module this provider can serve, without
/// serving it: the commands it defines, or why it is unusable.
///
/// # Errors
///
/// Returns why the module is unusable.
pub async fn check(bytes: &[u8]) -> Result<ModuleSummary> {
    Ok(prepare(&Host::new()?, bytes, Source::Installed)
        .await?
        .summary())
}

/// Creates the directory holding `path` when needed, then syncs it and its
/// ancestors. This runs on every change: an existing directory does not show
/// that its entry survives a system crash, and repeating a change must finish
/// a sync an earlier attempt could not.
fn publish_directory(path: &Path) -> std::io::Result<()> {
    let Some(directory) = path.parent() else {
        return Ok(());
    };
    fs::create_dir_all(directory)?;
    sync_ancestors(directory)
}

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Validates a module for serving. Every path applies the same checks,
/// including the size an operator may install, so `check` never accepts a
/// module that installation or startup would refuse.
async fn prepare(host: &Host<Calls>, bytes: &[u8], source: Source) -> Result<Generation> {
    if bytes.len() > MODULE_LIMIT {
        bail!(
            "the module has {} bytes; a command module may have at most {MODULE_LIMIT} bytes (16 MiB)",
            bytes.len()
        );
    }
    let module = host.load(bytes)?;
    let payload = serde_json::to_vec(&Invocation::Describe)?;
    let description = match outcome(host, &module, Calls::Refused, &payload, DESCRIBE_DEADLINE)
        .await
        .context("describe command module")?
    {
        Outcome::Description(description) => description,
        Outcome::Reply(_) | Outcome::Failure(_) => bail!("command module did not describe itself"),
    };
    let Description { contract, commands } = description;
    if contract != CONTRACT {
        bail!("command module speaks contract {contract}; this provider speaks {CONTRACT}");
    }
    for definition in &commands {
        command(definition)?;
    }
    Ok(Generation {
        module,
        definitions: commands,
        source,
        digest: digest(bytes),
    })
}

async fn outcome(
    host: &Host<Calls>,
    module: &Module,
    calls: Calls,
    payload: &[u8],
    deadline: Duration,
) -> Result<Outcome> {
    let output = host
        .invoke(module, calls, payload, deadline)
        .await
        .map_err(anyhow::Error::from)?;
    serde_json::from_slice(&output).context("decode command module outcome")
}

/// A command definition as Discord's registration API expects it. Modules
/// describe commands without registration metadata, which Discord assigns.
fn command(definition: &Value) -> Result<Command> {
    let Value::Object(fields) = definition else {
        bail!("command definition must be an object");
    };
    let mut fields = fields.clone();
    fields.entry("version").or_insert(json!("1"));
    serde_json::from_value(Value::Object(fields)).context("invalid command definition")
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{
        path::Path,
        sync::{
            Arc,
            atomic::{AtomicU16, AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use axum::{Router, http::StatusCode, routing::put};
    use carillon_discord_commands::contract::{Interaction, InteractionKind};
    use carillon_installation::maintenance::MaintenanceLock;
    use serde_json::{Value, json};

    use super::{
        ChangeError, CommandFiles, Commands, DiscordApp, MODULE_LIMIT, NextStart, Registration,
        Saved, Source,
    };

    /// A module that answers `Describe` with `description` and every other
    /// invocation with `reply`, in binary form.
    pub(crate) fn module(description: &str, reply: &str) -> anyhow::Result<Vec<u8>> {
        let escape = |text: &str| text.replace('\\', "\\\\").replace('"', "\\\"");
        let describe_length =
            serde_json::to_vec(&carillon_discord_commands::contract::Invocation::Describe)?.len();
        let text = format!(
            r#"(module
  (memory (export "memory") 1)
  (global $length (mut i32) (i32.const 0))
  (global $output (mut i32) (i32.const 0))
  (data (i32.const 1024) "{}")
  (data (i32.const 4096) "{}")
  (func (export "input") (param $length i32) (result i32)
    (global.set $length (local.get $length))
    (i32.const 8192))
  (func (export "invoke") (result i32)
    (if (result i32) (i32.eq (global.get $length) (i32.const {describe_length}))
      (then (global.set $output (i32.const 1024)) (i32.const {}))
      (else (global.set $output (i32.const 4096)) (i32.const {}))))
  (func (export "output") (result i32) (global.get $output)))"#,
            escape(description),
            escape(reply),
            description.len(),
            reply.len(),
        );
        Ok(wat::parse_str(text)?)
    }

    /// [`module`], written to `path`.
    pub(crate) fn write_module(path: &Path, description: &str, reply: &str) -> anyhow::Result<()> {
        std::fs::write(path, module(description, reply)?)?;
        Ok(())
    }

    pub(crate) fn describing(commands: &[Value]) -> String {
        json!({"Description": {"contract": carillon_discord_commands::contract::CONTRACT, "commands": commands}}).to_string()
    }

    pub(crate) fn replying(content: &str, components: &[Value]) -> String {
        json!({"Reply": {"content": content, "components": components}}).to_string()
    }

    pub(crate) fn definition(name: &str) -> Value {
        json!({"type": 1, "name": name, "description": "A test command"})
    }

    /// Command files in `root`: the release module at `release.wasm`, state
    /// under `state/`, and `root` as the installation.
    pub(crate) fn files(root: &Path) -> CommandFiles {
        CommandFiles {
            release: root.join("release.wasm"),
            installed: root.join("state/command-module.wasm"),
            record: root.join("state/commands.json"),
            installation: root.to_path_buf(),
        }
    }

    /// Commands whose release module defines `names` and replies `reply`.
    pub(crate) async fn release_commands(
        root: &Path,
        names: &[&str],
        reply: &str,
    ) -> anyhow::Result<Commands> {
        let definitions: Vec<Value> = names.iter().map(|name| definition(name)).collect();
        write_module(
            &root.join("release.wasm"),
            &describing(&definitions),
            &replying(reply, &[]),
        )?;
        Commands::load(files(root)).await
    }

    pub(crate) fn variant(names: &[&str], reply: &str) -> anyhow::Result<Vec<u8>> {
        let definitions: Vec<Value> = names.iter().map(|name| definition(name)).collect();
        module(&describing(&definitions), &replying(reply, &[]))
    }

    /// The application and guild a provider would act for, talking to the
    /// Discord API at `address`.
    pub(crate) fn app(address: &str, application_id: u64, guild_id: u64) -> DiscordApp {
        DiscordApp {
            http: Arc::new(
                twilight_http::Client::builder()
                    .token("test-only".to_owned())
                    .proxy(address.to_owned(), true)
                    .ratelimiter(None)
                    .build(),
            ),
            application_id: twilight_model::id::Id::new(application_id),
            guild_id: twilight_model::id::Id::new(guild_id),
        }
    }

    /// A Discord API that answers command registration with whatever status
    /// `status` holds when the request arrives, with an error body in
    /// Discord's shape for failures, counting the requests it receives.
    pub(crate) async fn registration_server(
        status: Arc<AtomicU16>,
    ) -> anyhow::Result<(
        std::net::SocketAddr,
        tokio::task::JoinHandle<std::io::Result<()>>,
    )> {
        let (address, server, _) = counting_registration_server(status).await?;
        Ok((address, server))
    }

    async fn counting_registration_server(
        status: Arc<AtomicU16>,
    ) -> anyhow::Result<(
        std::net::SocketAddr,
        tokio::task::JoinHandle<std::io::Result<()>>,
        Arc<AtomicUsize>,
    )> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let requests = Arc::new(AtomicUsize::new(0));
        let counted = requests.clone();
        let router = Router::new().route(
            "/api/v10/applications/{application}/guilds/{guild}/commands",
            put(move || async move {
                counted.fetch_add(1, Ordering::SeqCst);
                let status = StatusCode::from_u16(status.load(Ordering::SeqCst))
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                let body = if status.is_success() {
                    json!([])
                } else {
                    json!({"code": 50035, "message": "Invalid Form Body"})
                };
                (status, axum::Json(body))
            }),
        );
        Ok((
            address,
            tokio::spawn(async move { axum::serve(listener, router).await }),
            requests,
        ))
    }

    fn calls() -> super::InteractionCalls {
        super::InteractionCalls::new(
            crate::client::test_client("http://127.0.0.1:1".to_owned()),
            carillon_protocol::model::ModelQuery {
                agent_id: "default".to_owned(),
                audience_id: None,
            },
        )
    }

    fn interaction() -> Interaction {
        Interaction {
            kind: InteractionKind::Command,
            name: "model".to_owned(),
            values: Vec::new(),
            options: Value::Null,
            conversation: false,
        }
    }

    async fn reply(commands: &Commands) -> anyhow::Result<String> {
        Ok(commands.invoke(calls(), interaction()).await?.content)
    }

    #[tokio::test]
    async fn loading_checks_the_contract_and_definitions() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let release = directory.path().join("release.wasm");
        let reply = replying("unused", &[]);
        for description in [
            json!({"Description": {"contract": carillon_discord_commands::contract::CONTRACT + 1, "commands": []}}).to_string(),
            describing(&[json!({"name": "missing-type"})]),
            reply.clone(),
            "not json".to_owned(),
        ] {
            let bytes = module(&description, &reply)?;
            assert!(super::check(&bytes).await.is_err(), "accepted {description}");
            std::fs::write(&release, bytes)?;
            assert!(
                Commands::load(files(directory.path())).await.is_err(),
                "accepted {description}"
            );
        }
        assert!(super::check(b"\0asm broken").await.is_err());
        let summary = super::check(&variant(&["model"], "unused")?).await?;
        assert_eq!(summary.commands, ["model"]);
        let commands = release_commands(directory.path(), &["model"], "unused").await?;
        assert_eq!(commands.serving().source, Source::Release);
        assert_eq!(commands.serving().commands, ["model"]);
        assert!(!commands.registered(&app("127.0.0.1:1", 1, 100), &commands.current().definitions));
        Ok(())
    }

    #[tokio::test]
    async fn installs_serve_only_usable_modules_and_start_from_the_saved_one() -> anyhow::Result<()>
    {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let commands = release_commands(root, &["model"], "release").await?;
        let installed = files(root).installed;

        // A refused candidate changes neither memory nor disk.
        assert!(matches!(
            commands.install(b"\0asm broken", None).await,
            Err(ChangeError::Unusable(_))
        ));
        assert_eq!(reply(&commands).await?, "release");
        assert!(!installed.exists());
        assert!(
            !root.join(".maintenance.lock").exists()
                || MaintenanceLock::try_acquire(root)?.is_some()
        );

        // Behaviour-only and definition changes both serve and save.
        let behaviour = variant(&["model"], "installed")?;
        let change = commands.install(&behaviour, None).await?;
        assert_eq!(change.previous.source, Source::Release);
        assert_eq!(change.serving.source, Source::Installed);
        assert_eq!(change.serving.commands, ["model"]);
        assert_ne!(change.serving.digest, change.previous.digest);
        assert_eq!(change.discord, Registration::NotConnected);
        assert_eq!(change.saved, Saved::Durable);
        assert_eq!(reply(&commands).await?, "installed");
        assert_eq!(std::fs::read(&installed)?, behaviour);
        assert_eq!(commands.status(None).next_start, NextStart::Same);

        let wider = variant(&["model", "other"], "wider")?;
        let change = commands.install(&wider, None).await?;
        assert_eq!(change.serving.commands, ["model", "other"]);
        assert_eq!(reply(&commands).await?, "wider");

        // A restart serves the saved module, even though the release differs.
        let restarted = Commands::load(files(root)).await?;
        assert_eq!(restarted.serving(), change.serving);
        assert_eq!(reply(&restarted).await?, "wider");

        // An unusable installed module refuses startup, naming the file.
        std::fs::write(&installed, b"\0asm broken")?;
        let failure = Commands::load(files(root))
            .await
            .err()
            .map(|error| format!("{error:#}"))
            .unwrap_or_default();
        assert!(
            failure.contains("command-module.wasm is unusable; remove it"),
            "{failure}"
        );

        // Reset serves the release and removes the installed module.
        let change = commands.reset(None).await?;
        assert_eq!(change.serving.source, Source::Release);
        assert_eq!(change.saved, Saved::Durable);
        assert!(!installed.exists());
        assert_eq!(reply(&commands).await?, "release");
        assert_eq!(Commands::load(files(root)).await?.serving(), change.serving);
        Ok(())
    }

    #[tokio::test]
    async fn changes_wait_for_other_maintenance_and_change_nothing() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let commands = release_commands(root, &["model"], "release").await?;
        let status = Arc::new(AtomicU16::new(200));
        let (address, server, requests) = counting_registration_server(status).await?;
        let target = app(&address.to_string(), 1, 100);
        let held = MaintenanceLock::acquire(root)?;
        let candidate = variant(&["other"], "installed")?;
        assert!(matches!(
            commands.install(&candidate, Some(&target)).await,
            Err(ChangeError::Busy(_))
        ));
        assert!(matches!(
            commands.reset(Some(&target)).await,
            Err(ChangeError::Busy(_))
        ));
        assert_eq!(requests.load(Ordering::SeqCst), 0);
        assert!(!files(root).installed.exists());
        assert_eq!(reply(&commands).await?, "release");
        drop(held);
        commands.install(&candidate, Some(&target)).await?;
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        // The provider releases the lock once its change is complete.
        assert!(MaintenanceLock::try_acquire(root)?.is_some());
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn refused_registration_keeps_the_module_and_forgets_the_record() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let commands = release_commands(root, &["model"], "release").await?;
        let status = Arc::new(AtomicU16::new(200));
        let (address, server, requests) = counting_registration_server(status.clone()).await?;
        let target = app(&address.to_string(), 1, 100);
        assert_eq!(commands.reconcile(&target).await?, Registration::Registered);
        assert_eq!(commands.reconcile(&target).await?, Registration::Unchanged);
        assert_eq!(commands.status(Some(&target)).registered, Some(true));

        let candidate = variant(&["model", "other"], "installed")?;
        for code in [400, 500] {
            status.store(code, Ordering::SeqCst);
            assert!(matches!(
                commands.install(&candidate, Some(&target)).await,
                Err(ChangeError::Register(_))
            ));
            assert_eq!(reply(&commands).await?, "release");
            assert!(!files(root).installed.exists());
            // Discord may hold either definition set now, so the record
            // attests to neither and the next change or start registers.
            assert!(!files(root).record.exists());
            assert_eq!(commands.status(Some(&target)).registered, Some(false));
        }

        // Repeating a change reconciles Discord even when the requested
        // module is already serving.
        status.store(200, Ordering::SeqCst);
        let before = requests.load(Ordering::SeqCst);
        let change = commands.reset(Some(&target)).await?;
        assert_eq!(change.previous, change.serving);
        assert_eq!(change.discord, Registration::Registered);
        assert_eq!(requests.load(Ordering::SeqCst), before + 1);
        assert_eq!(commands.status(Some(&target)).registered, Some(true));
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn a_record_that_cannot_be_cleared_stops_before_discord() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let commands = release_commands(root, &["model"], "release").await?;
        let (address, server, requests) =
            counting_registration_server(Arc::new(AtomicU16::new(200))).await?;
        let target = app(&address.to_string(), 1, 100);
        // A directory where the record belongs cannot be removed as a file.
        std::fs::create_dir_all(files(root).record.join("occupied"))?;
        assert!(matches!(
            commands
                .install(&variant(&["other"], "installed")?, Some(&target))
                .await,
            Err(ChangeError::Record(
                carillon_installation::filesystem::PublishError::NotPublished(_)
            ))
        ));
        assert_eq!(requests.load(Ordering::SeqCst), 0);
        assert_eq!(reply(&commands).await?, "release");
        assert!(!files(root).installed.exists());
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn an_unsaved_module_serves_and_a_repeat_finishes_the_save() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let commands = release_commands(root, &["model"], "release").await?;
        let installed = files(root).installed;
        // A directory where the module belongs cannot be replaced by a file.
        std::fs::create_dir_all(installed.join("occupied"))?;
        let candidate = variant(&["model"], "installed")?;
        let change = commands.install(&candidate, None).await?;
        assert!(matches!(change.saved, Saved::Failed { .. }), "{change:?}");
        assert_eq!(reply(&commands).await?, "installed");
        // Memory serves the installed module; the next start would not.
        assert!(matches!(
            commands.status(None).next_start,
            NextStart::Unknown { .. }
        ));

        std::fs::remove_dir_all(&installed)?;
        assert_eq!(
            commands.status(None).next_start,
            NextStart::Different {
                source: Source::Release
            }
        );
        let change = commands.install(&candidate, None).await?;
        assert_eq!(change.previous, change.serving);
        assert_eq!(change.saved, Saved::Durable);
        assert_eq!(std::fs::read(&installed)?, candidate);
        assert_eq!(commands.status(None).next_start, NextStart::Same);
        Ok(())
    }

    /// A Discord API that reports each registration on `sent` and confirms it
    /// only once `gate` grants a permit.
    pub(crate) fn gated_registration(
        sent: tokio::sync::mpsc::UnboundedSender<Vec<Value>>,
        gate: Arc<tokio::sync::Semaphore>,
    ) -> Router {
        Router::new().route(
            "/api/v10/applications/{application}/guilds/{guild}/commands",
            put(move |axum::Json(definitions): axum::Json<Vec<Value>>| {
                let sent = sent.clone();
                let gate = gate.clone();
                async move {
                    sent.send(definitions)
                        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                    gate.acquire()
                        .await
                        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
                        .forget();
                    Ok::<_, StatusCode>(axum::Json(Vec::<Value>::new()))
                }
            }),
        )
    }

    #[tokio::test]
    async fn overlapping_installs_register_and_activate_in_order() -> anyhow::Result<()> {
        use std::time::Duration;
        use tokio::sync::{Semaphore, mpsc};

        let directory = tempfile::tempdir()?;
        let commands = release_commands(directory.path(), &["initial"], "initial").await?;
        let retained = commands.current();
        let (sent, mut received) = mpsc::unbounded_channel();
        let release = Arc::new(Semaphore::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let target = app(&listener.local_addr()?.to_string(), 1, 100);
        let router = gated_registration(sent, release.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await });
        let first_module = variant(&["first"], "first")?;
        let first = commands.install(&first_module, Some(&target));
        tokio::pin!(first);
        let registration = tokio::select! {
            _ = &mut first => anyhow::bail!("install completed before confirmation"),
            request = received.recv() => request.ok_or_else(|| anyhow::anyhow!("registration closed"))?,
        };
        assert_eq!(registration[0]["name"], "first");
        let second_module = variant(&["second"], "second")?;
        let second = commands.install(&second_module, Some(&target));
        tokio::pin!(second);
        // Poll both operations while registration is blocked. The second
        // must not send a request or activate; ordinary invocations still work.
        tokio::select! {
            _ = &mut first => anyhow::bail!("first activated before confirmation"),
            _ = &mut second => anyhow::bail!("second bypassed the first install"),
            request = received.recv() => anyhow::bail!("overlapping registration: {request:?}"),
            () = tokio::time::sleep(Duration::from_millis(100)) => {},
        }
        assert_eq!(reply(&commands).await?, "initial");
        release.add_permits(1);
        assert!(first.await.is_ok());
        assert_eq!(commands.serving().commands, ["first"]);
        assert_eq!(reply(&commands).await?, "first");
        let registration = tokio::select! {
            _ = &mut second => anyhow::bail!("second completed before confirmation"),
            request = received.recv() => request.ok_or_else(|| anyhow::anyhow!("registration closed"))?,
        };
        assert_eq!(registration[0]["name"], "second");
        release.add_permits(1);
        assert!(second.await.is_ok());
        assert_eq!(commands.serving().commands, ["second"]);
        assert!(commands.registered(&target, &commands.current().definitions));
        assert_eq!(reply(&commands).await?, "second");
        assert_eq!(
            std::fs::read(files(directory.path()).installed)?,
            second_module
        );
        // A previously captured generation keeps both its code and metadata.
        assert_eq!(retained.definitions, vec![definition("initial")]);
        let payload = serde_json::to_vec(
            &carillon_discord_commands::contract::Invocation::Interaction(interaction()),
        )?;
        let old = super::outcome(
            &commands.host,
            &retained.module,
            super::Calls::Refused,
            &payload,
            super::DEADLINE,
        )
        .await?;
        assert_eq!(
            old,
            carillon_discord_commands::contract::Outcome::Reply(
                carillon_discord_commands::contract::Reply {
                    content: "initial".to_owned(),
                    components: vec![],
                }
            )
        );
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn registration_is_recorded_for_its_target_once_confirmed() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let commands = release_commands(directory.path(), &["model"], "unused").await?;
        let record = files(directory.path()).record;
        let definitions = commands.current().definitions.clone();
        let status = Arc::new(AtomicU16::new(200));
        let (address, server) = registration_server(status.clone()).await?;
        let target = app(&address.to_string(), 1, 100);

        // A record in the shape an earlier provider wrote names no target.
        std::fs::create_dir_all(record.parent().unwrap_or(directory.path()))?;
        std::fs::write(&record, serde_json::to_vec(&definitions)?)?;
        assert!(!commands.registered(&target, &definitions));

        commands.register(&target, &definitions).await?;
        assert!(commands.registered(&target, &definitions));
        assert!(!commands.registered(&target, &[definition("other")]));
        assert!(!commands.registered(&app(&address.to_string(), 1, 101), &definitions));
        assert!(!commands.registered(&app(&address.to_string(), 2, 100), &definitions));

        // Neither a rejection nor an unanswered request counts as confirmed.
        let other = vec![definition("other")];
        for code in [400, 500] {
            status.store(code, Ordering::SeqCst);
            assert!(commands.register(&target, &other).await.is_err(), "{code}");
            assert!(!commands.registered(&target, &definitions));
            assert!(!commands.registered(&target, &other));
            assert!(!record.exists());
        }
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn confirmed_install_activates_even_when_recording_fails() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let commands = release_commands(directory.path(), &["old"], "old").await?;
        let blocked = files(directory.path()).record;
        let router = Router::new().route(
            "/api/v10/applications/{application}/guilds/{guild}/commands",
            put(move || {
                let blocked = blocked.clone();
                async move {
                    // The remote mutation succeeded; the local destination
                    // becomes unwritable before its confirmation is recorded.
                    std::fs::create_dir(&blocked).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                    Ok::<_, StatusCode>(axum::Json(Vec::<Value>::new()))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let target = app(&listener.local_addr()?.to_string(), 1, 100);
        let server = tokio::spawn(async move { axum::serve(listener, router).await });
        let change = commands
            .install(&variant(&["new"], "new")?, Some(&target))
            .await?;
        assert_eq!(change.discord, Registration::Registered);
        assert_eq!(change.saved, Saved::Durable);
        assert_eq!(reply(&commands).await?, "new");
        assert!(!commands.registered(&target, &commands.current().definitions));
        server.abort();
        Ok(())
    }

    /// `bytes` with a custom section appended so the module has exactly
    /// `total` bytes. Custom sections do not change what a module does.
    pub(crate) fn padded(mut bytes: Vec<u8>, total: usize) -> anyhow::Result<Vec<u8>> {
        fn leb128(mut value: usize, into: &mut Vec<u8>) {
            loop {
                let byte = u8::try_from(value & 0x7f).unwrap_or_default();
                value >>= 7;
                if value == 0 {
                    into.push(byte);
                    return;
                }
                into.push(byte | 0x80);
            }
        }
        let name = b"padding";
        for width in 1..=5 {
            let size = total
                .checked_sub(bytes.len() + 1 + width)
                .ok_or_else(|| anyhow::anyhow!("module already exceeds {total} bytes"))?;
            let mut header = vec![0];
            leb128(size, &mut header);
            if header.len() != 1 + width {
                continue;
            }
            let payload = size - 1 - name.len();
            bytes.extend(header);
            bytes.push(u8::try_from(name.len())?);
            bytes.extend(name);
            bytes.resize(bytes.len() + payload, 0);
            anyhow::ensure!(bytes.len() == total);
            return Ok(bytes);
        }
        anyhow::bail!("cannot pad to {total} bytes")
    }

    #[tokio::test]
    async fn checking_installing_and_starting_share_one_size_limit() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let commands = release_commands(root, &["model"], "release").await?;
        let base = variant(&["model"], "padded")?;
        let at_limit = padded(base.clone(), MODULE_LIMIT)?;
        let over = padded(base, MODULE_LIMIT + 1)?;

        let refusal = super::check(&over)
            .await
            .err()
            .map(|error| format!("{error:#}"))
            .unwrap_or_default();
        assert!(
            refusal.contains(&format!("has {} bytes", MODULE_LIMIT + 1))
                && refusal.contains(&format!("at most {MODULE_LIMIT} bytes")),
            "{refusal}"
        );
        assert!(matches!(
            commands.install(&over, None).await,
            Err(ChangeError::Unusable(_))
        ));
        assert!(!files(root).installed.exists());
        assert_eq!(reply(&commands).await?, "release");

        assert_eq!(super::check(&at_limit).await?.commands, ["model"]);
        assert_eq!(
            commands.install(&at_limit, None).await?.saved,
            Saved::Durable
        );
        assert_eq!(reply(&commands).await?, "padded");
        assert_eq!(
            Commands::load(files(root)).await?.serving().commands,
            ["model"]
        );

        // A module placed on disk by other means obeys the same limit at startup.
        std::fs::write(files(root).installed, &over)?;
        assert!(Commands::load(files(root)).await.is_err());
        Ok(())
    }

    /// Run under the persistence suite's syscall fault injection: the first
    /// save creates the state directory, and a repeat must sync that
    /// directory and its ancestors again rather than trust that it exists.
    #[tokio::test]
    async fn a_new_state_directory_is_synced_again_by_a_repeat() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let commands = release_commands(root, &["model"], "release").await?;
        assert!(!root.join("state").exists());
        let candidate = variant(&["model"], "installed")?;
        let first = match commands.install(&candidate, None).await?.saved {
            Saved::Durable => "durable".to_owned(),
            Saved::Failed { error } => format!("failed ({error})"),
            Saved::Unsynced { error } => format!("unsynced ({error})"),
        };
        eprintln!("first save: {first}");
        let change = commands.install(&candidate, None).await?;
        assert_eq!(change.saved, Saved::Durable);
        assert_eq!(std::fs::read(files(root).installed)?, candidate);
        assert_eq!(commands.status(None).next_start, NextStart::Same);
        eprintln!("repeat saved: durable");
        Ok(())
    }

    /// Run under the persistence suite's syscall fault injection: one change
    /// reports where its save stopped, then a repeat must confirm it.
    #[tokio::test]
    async fn saving_reports_its_boundary_and_a_repeat_confirms_it() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let commands = release_commands(root, &["model"], "release").await?;
        // An existing state directory: the first sync of it clears the record.
        std::fs::create_dir(root.join("state"))?;
        let (address, server, requests) =
            counting_registration_server(Arc::new(AtomicU16::new(200))).await?;
        let target = app(&address.to_string(), 1, 100);
        let candidate = variant(&["model", "other"], "installed")?;
        let first = match commands.install(&candidate, Some(&target)).await {
            Ok(change) => match change.saved {
                Saved::Durable => "saved: durable".to_owned(),
                Saved::Failed { error } => format!("saved: failed ({error})"),
                Saved::Unsynced { error } => format!("saved: unsynced ({error})"),
            },
            Err(ChangeError::Record(error)) => format!("refused: record ({error})"),
            Err(error) => anyhow::bail!("unexpected refusal: {error}"),
        };
        eprintln!(
            "first change {first}; Discord requests {}",
            requests.load(Ordering::SeqCst)
        );
        let change = commands.install(&candidate, Some(&target)).await?;
        assert_eq!(change.saved, Saved::Durable);
        assert_eq!(std::fs::read(files(root).installed)?, candidate);
        assert!(commands.registered(&target, &commands.current().definitions));
        assert_eq!(commands.status(Some(&target)).next_start, NextStart::Same);
        assert_eq!(reply(&commands).await?, "installed");
        eprintln!("repeat saved: durable");
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn model_calls_preserve_the_conditional_write() -> anyhow::Result<()> {
        use axum::Json;
        use carillon_discord_commands::contract::{HostReply, HostRequest};
        use carillon_protocol::model::{ModelQuery, ModelUpdate};
        use carillon_protocol::{model::ModelScope as Scope, models::Selection};
        let selection = Selection {
            provider: "fixture".to_owned(),
            model: "A".to_owned(),
            options: std::collections::BTreeMap::new(),
        };
        let expected = selection.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let router = Router::new().route(
            "/v1/model",
            put(move |Json(update): Json<ModelUpdate>| {
                let expected = expected.clone();
                async move {
                    assert_eq!(update.expected, Some(expected));
                    (
                        StatusCode::CONFLICT,
                        Json(json!({"error": "changed while applying"})),
                    )
                }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, router).await });
        let calls = super::InteractionCalls::new(
            crate::client::test_client(format!("http://{address}")),
            ModelQuery {
                agent_id: "default".to_owned(),
                audience_id: None,
            },
        );
        let reply = calls
            .answer(&serde_json::to_vec(&HostRequest::SetModel {
                scope: Scope::Agent,
                selection: Some(selection.clone()),
                expected: Some(selection),
            })?)
            .await;
        assert_eq!(reply, HostReply::Error("changed while applying".to_owned()));
        server.abort();
        Ok(())
    }

    /// Core and Discord in one fake: lists `catalog`, accepts every event
    /// after `admission` and shows feedback after `feedback`, posts every
    /// message as `901`, and traces the requests in order as they arrive.
    fn submission_server(
        catalog: carillon_protocol::model::CommandCatalog,
        trace: Arc<tokio::sync::Mutex<Vec<String>>>,
        submitted: Arc<tokio::sync::Mutex<Vec<carillon_protocol::model::ExternalEvent>>>,
        (admission, feedback): (Duration, Duration),
    ) -> Router {
        use axum::{
            Json,
            extract::Query,
            http::{Method, Uri},
            routing::{get, post},
        };
        use carillon_protocol::model::{EventReceipt, ExternalEvent, TurnState};
        use std::collections::HashMap;
        let (messages, events) = (trace.clone(), trace.clone());
        Router::new()
            .route(
                "/v1/commands",
                get(move |Query(query): Query<HashMap<String, String>>| {
                    let catalog = catalog.clone();
                    async move {
                        assert_eq!(
                            query.get("audience_id").map(String::as_str),
                            Some("discord-100-400")
                        );
                        Json(catalog)
                    }
                }),
            )
            .route(
                "/v1/events",
                post(move |Json(event): Json<ExternalEvent>| {
                    let (events, submitted) = (events.clone(), submitted.clone());
                    async move {
                        events
                            .lock()
                            .await
                            .push(format!("event {}", event.event_id));
                        tokio::time::sleep(admission).await;
                        submitted.lock().await.push(event.clone());
                        Json(EventReceipt {
                            event_id: event.event_id,
                            turn_id: "turn".to_owned(),
                            duplicate: false,
                            state: TurnState::Queued,
                        })
                    }
                }),
            )
            .route(
                "/api/v10/channels/{channel}/messages",
                post(move |Json(body): Json<Value>| {
                    let messages = messages.clone();
                    async move {
                        messages.lock().await.push(format!(
                            "message {} mentions {}",
                            body["content"].as_str().unwrap_or_default(),
                            body["allowed_mentions"]
                        ));
                        Json(json!({
                            "id": "901",
                            "channel_id": "400",
                            "author": {"id": "201", "username": "carillon", "discriminator": "0"},
                            "content": body["content"],
                            "attachments": [],
                            "embeds": [],
                            "mention_everyone": false,
                            "mention_roles": [],
                            "mentions": [],
                            "pinned": false,
                            "timestamp": "2026-10-05T00:00:00.000000+00:00",
                            "tts": false,
                            "type": 0,
                        }))
                    }
                }),
            )
            .fallback(move |method: Method, uri: Uri| {
                let trace = trace.clone();
                async move {
                    trace.lock().await.push(format!("{method} {}", uri.path()));
                    tokio::time::sleep(feedback).await;
                    StatusCode::NO_CONTENT
                }
            })
    }

    #[tokio::test]
    async fn commands_are_listed_and_submissions_anchored_in_the_conversation() -> anyhow::Result<()>
    {
        use carillon_discord_commands::contract::{HostReply, HostRequest, Intent};
        use carillon_protocol::model::{CommandCatalog, ModelQuery, RuntimeCommand};
        let catalog = CommandCatalog {
            commands: vec![RuntimeCommand {
                name: "compact".to_owned(),
                description: None,
                hint: None,
            }],
        };
        let trace = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let submitted = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let router = submission_server(
            catalog.clone(),
            trace.clone(),
            submitted.clone(),
            (Duration::ZERO, Duration::ZERO),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move { axum::serve(listener, router).await });
        let core = crate::client::test_client(format!("http://{address}"));
        let query = ModelQuery {
            agent_id: "default".to_owned(),
            audience_id: Some("discord-100-400".to_owned()),
        };
        let submit = serde_json::to_vec(&HostRequest::Submit {
            text: "/compact keep `this`".to_owned(),
            intent: Intent::Command,
        })?;

        // Without a Discord connection there is nowhere to anchor it.
        let unanchored = super::InteractionCalls::new(core.clone(), query.clone());
        assert_eq!(
            unanchored
                .answer(&serde_json::to_vec(&HostRequest::Commands)?)
                .await,
            HostReply::Commands(catalog)
        );
        assert!(matches!(
            unanchored.answer(&submit).await,
            HostReply::Error(_)
        ));
        assert!(submitted.lock().await.is_empty());

        let mut config = crate::conversations::tests::discord_config(
            crate::config::DiscordActivation::Always,
            crate::config::DiscordConversation::Thread,
        );
        config.typing = false;
        let calls =
            super::InteractionCalls::new(core, query).with_submissions(super::Submissions {
                http: Arc::new(
                    twilight_http::Client::builder()
                        .token("test-only".to_owned())
                        .proxy(address.to_string(), true)
                        .ratelimiter(None)
                        .build(),
                ),
                config,
                origin: crate::conversations::InteractionOrigin {
                    guild_id: "100".to_owned(),
                    user_id: "200".to_owned(),
                    channel_id: "400".to_owned(),
                    route_channel_id: "300".to_owned(),
                },
            });
        assert_eq!(calls.answer(&submit).await, HostReply::Submitted);
        server.abort();
        let trace = trace.lock().await;
        // The marker is posted silently first, marked processing, then submitted.
        assert_eq!(
            trace[0],
            "message <@200> ran `/compact keep ˋthisˋ` mentions {\"parse\":[]}"
        );
        assert!(trace[1].starts_with("PUT /api/v10/channels/400/messages/901/reactions/"));
        assert_eq!(trace[2], "event 901");
        let event = submitted.lock().await.remove(0);
        assert_eq!(event.content, "/compact keep `this`");
        assert_eq!(event.feedback_target_id, "400:901");
        assert_eq!(event.delivery_target_id, "400");
        assert_eq!(event.audience_id, "discord-100-400");
        assert_eq!(event.native["intent"], "command");
        assert_eq!(event.native["route_channel_id"], "300");
        assert_eq!(event.native["origin"]["parent_channel_id"], "300");
        Ok(())
    }

    #[tokio::test]
    async fn interactions_receive_the_module_reply_or_a_failure() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let button = json!({"type": 1, "components": [{"type": 2, "style": 2, "custom_id": "x", "label": "X"}]});
        write_module(
            &directory.path().join("release.wasm"),
            &describing(&[]),
            &replying("hello", std::slice::from_ref(&button)),
        )?;
        let commands = Commands::load(files(directory.path())).await?;
        let reply = commands.invoke(calls(), interaction()).await?;
        assert_eq!(reply.content, "hello");
        assert_eq!(reply.components, vec![button]);
        for output in [
            json!({"Failure": "no such command"}).to_string(),
            describing(&[]),
            "{broken".to_owned(),
        ] {
            commands
                .install(&module(&describing(&[]), &output)?, None)
                .await?;
            assert!(
                commands.invoke(calls(), interaction()).await.is_err(),
                "accepted {output}"
            );
        }
        Ok(())
    }

    /// Calls that submit through `server`, as the person in channel `400`.
    async fn submitting(
        router: Router,
        configure: impl FnOnce(&mut crate::config::DiscordConfig),
    ) -> anyhow::Result<(
        super::InteractionCalls,
        tokio::task::JoinHandle<std::io::Result<()>>,
    )> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move { axum::serve(listener, router).await });
        let mut config = crate::conversations::tests::discord_config(
            crate::config::DiscordActivation::Always,
            crate::config::DiscordConversation::Thread,
        );
        configure(&mut config);
        let calls = super::InteractionCalls::new(
            crate::client::test_client(format!("http://{address}")),
            carillon_protocol::model::ModelQuery {
                agent_id: "default".to_owned(),
                audience_id: Some("discord-100-400".to_owned()),
            },
        )
        .with_submissions(super::Submissions {
            http: Arc::new(
                twilight_http::Client::builder()
                    .token("test-only".to_owned())
                    .proxy(address.to_string(), true)
                    .ratelimiter(None)
                    .build(),
            ),
            config,
            origin: crate::conversations::InteractionOrigin {
                guild_id: "100".to_owned(),
                user_id: "200".to_owned(),
                channel_id: "400".to_owned(),
                route_channel_id: "300".to_owned(),
            },
        });
        Ok((calls, server))
    }

    fn submit_request() -> anyhow::Result<Vec<u8>> {
        Ok(serde_json::to_vec(
            &carillon_discord_commands::contract::HostRequest::Submit {
                text: "/compact keep decisions".to_owned(),
                intent: carillon_discord_commands::contract::Intent::Command,
            },
        )?)
    }

    #[tokio::test]
    async fn slow_feedback_never_costs_a_command_its_admission() -> anyhow::Result<()> {
        use carillon_discord_commands::contract::HostReply;
        // Discord takes longer than the module's whole deadline to show the
        // processing reaction and typing.
        let trace = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let submitted = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let router = submission_server(
            carillon_protocol::model::CommandCatalog {
                commands: Vec::new(),
            },
            trace.clone(),
            submitted.clone(),
            (Duration::ZERO, Duration::from_secs(30)),
        );
        let (calls, server) = submitting(router, |config| config.typing = true).await?;
        let started = std::time::Instant::now();
        let reply = calls.answer(&submit_request()?).await;
        let elapsed = started.elapsed();
        server.abort();
        assert_eq!(reply, HostReply::Submitted);
        assert!(
            elapsed < crate::delivery::PROMPT_FEEDBACK + Duration::from_secs(1),
            "admission waited {elapsed:?} for feedback"
        );
        assert_eq!(submitted.lock().await.len(), 1);
        // The marker, then the feedback that was asked for, then admission.
        let trace = trace.lock().await;
        assert!(trace[0].starts_with("message <@200> ran"));
        let feedback = &trace[1..3];
        assert!(
            feedback
                .iter()
                .any(|request| request.contains("/reactions/"))
        );
        assert!(feedback.iter().any(|request| request.ends_with("/typing")));
        assert_eq!(trace[3], "event 901");
        Ok(())
    }

    #[tokio::test]
    async fn a_command_core_does_not_answer_in_time_says_so_within_the_module_deadline()
    -> anyhow::Result<()> {
        use carillon_discord_commands::contract::HostReply;
        let trace = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let router = submission_server(
            carillon_protocol::model::CommandCatalog {
                commands: Vec::new(),
            },
            trace.clone(),
            Arc::new(tokio::sync::Mutex::new(Vec::new())),
            (Duration::from_secs(30), Duration::ZERO),
        );
        let (calls, server) = submitting(router, |config| config.typing = false).await?;
        let started = std::time::Instant::now();
        let reply = calls.answer(&submit_request()?).await;
        let elapsed = started.elapsed();
        server.abort();
        // Core may have accepted it: the answer says so, in time to be shown.
        assert_eq!(
            reply,
            HostReply::Error(
                "Carillon did not confirm that it accepted it; if it did, the answer follows in this conversation."
                    .to_owned()
            )
        );
        assert!(elapsed < super::DEADLINE, "answered after {elapsed:?}");
        let trace = trace.lock().await;
        assert_eq!(
            trace
                .iter()
                .filter(|request| request.as_str() == "event 901")
                .count(),
            1
        );
        // The uncertain reaction replaces the processing one.
        assert!(
            trace.last().is_some_and(
                |request| request.starts_with("PUT ") && request.contains("/reactions/")
            )
        );
        Ok(())
    }
}
