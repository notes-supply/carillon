//! Hosting of the replaceable Discord module: choosing the module to serve,
//! learning which commands it defines, registering those with Discord,
//! running each interaction through it with the native operations it may ask
//! for, rendering work cards through it without any, and switching modules
//! while serving.
//!
//! Three facts have separate owners. The provider's settings file declares
//! the module to serve: a custom one it enables, otherwise the release's
//! bundled module. Memory holds the generation serving now. The registration
//! record holds what Discord last confirmed. Changes serialize on the
//! installation's maintenance lock, and every change converges these facts in
//! one order: validate, register when the record does not match, then
//! activate.

use std::{
    fmt, fs,
    io::{self, Read as _},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};
use tokio::time::{Instant, timeout_at};

use anyhow::{Context as _, Result, bail};
use carillon_discord_module::contract::{
    ActivityMarkers, CONTRACT, Card, CardRequest, Description, HostReply, HostRequest, Intent,
    Interaction, Invocation, Outcome, Reply,
};
use carillon_installation::{
    config::LoadedConfig,
    filesystem::{PublishError, atomic_write, remove, sync_ancestors},
    maintenance::MaintenanceLock,
};
use carillon_protocol::model::{
    ActivityPhase, DeliveryState, Job, JobTurn, ModelQuery, ModelScope, ModelUpdate, Origin,
    TurnActivity, TurnState,
};
use carillon_wasm::{Host, Module as Compiled, Native};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tracing::{info, warn};
use twilight_http::Client as DiscordHttp;
use twilight_model::{
    application::command::Command,
    channel::message::{AllowedMentions, Embed},
    id::{
        Id,
        marker::{ApplicationMarker, ChannelMarker, GuildMarker},
    },
};

use crate::{
    client::{CoreClient, RUNTIME_LATE},
    config::DiscordConfig,
    conversations::{InteractionOrigin, submitted_event},
    delivery::{Feedback, Reactions},
    inbound::lanes::Lanes,
};

/// The whole invocation, including every native call it makes.
const DEADLINE: Duration = Duration::from_secs(10);
/// Description is pure computation; a module that needs longer is broken.
const DESCRIBE_DEADLINE: Duration = Duration::from_secs(2);
/// A card is pure computation too; one that takes longer is not shown.
const CARD_DEADLINE: Duration = Duration::from_secs(1);
/// The largest card request sent to the module, well within its memory. A
/// job core reads is far smaller unless a runtime's snapshot is not bounded.
const CARD_REQUEST_LIMIT: usize = 1 << 20;
/// The largest module the provider serves.
pub(crate) const MODULE_LIMIT: usize = 16 << 20;

/// The Discord application the provider acts as, once the token is known,
/// and the guild whose commands it owns.
pub(crate) struct DiscordApp {
    pub http: Arc<DiscordHttp>,
    pub application_id: Id<ApplicationMarker>,
    pub guild_id: Id<GuildMarker>,
}

/// The native operations one interaction may perform: model state and
/// selection for the agent and, inside a conversation, for that audience;
/// the runtime's commands there; and, with a Discord connection, submitting
/// text to the conversation as the person who invoked it.
pub(crate) struct InteractionCalls {
    core: CoreClient,
    query: ModelQuery,
    submissions: Option<Submissions>,
}

/// What submitting needs: the conversation's channel to anchor the text in,
/// Discord to post that anchor, the reactions that show its progress, the
/// lanes in which it takes its place among the conversation's messages, and
/// the settings that admitted the interaction, whose feedback it shows.
pub(crate) struct Submissions {
    pub http: Arc<DiscordHttp>,
    pub reactions: Reactions,
    pub lanes: Lanes,
    pub origin: InteractionOrigin,
    pub settings: Arc<DiscordConfig>,
}

impl InteractionCalls {
    pub(crate) const fn new(core: CoreClient, query: ModelQuery) -> Self {
        Self {
            core,
            query,
            submissions: None,
        }
    }

    #[must_use]
    pub(crate) fn with_submissions(mut self, submissions: Submissions) -> Self {
        self.submissions = Some(submissions);
        self
    }
}

/// Native calls for invocations that must not perform any (descriptions and
/// cards), or those of one interaction, which all end by `until`.
enum Calls {
    Refused,
    Interaction {
        calls: Box<InteractionCalls>,
        until: Instant,
    },
}

impl Native for Calls {
    async fn call(&mut self, request: Vec<u8>) -> Vec<u8> {
        let reply = match self {
            Self::Refused => HostReply::Error("native calls are not available".to_owned()),
            Self::Interaction { calls, until } => calls.answer(&request, *until).await,
        };
        serde_json::to_vec(&reply).unwrap_or_default()
    }
}

/// A runtime call of an interaction, ended by `until` if it has not ended.
async fn within<T>(
    until: Instant,
    call: impl Future<Output = Result<T, String>>,
) -> Result<T, String> {
    timeout_at(until, call)
        .await
        .unwrap_or_else(|_| Err(RUNTIME_LATE.to_owned()))
}

impl InteractionCalls {
    /// Answers one of the interaction's native requests by `until`.
    async fn answer(&self, request: &[u8], until: Instant) -> HostReply {
        let outcome = match serde_json::from_slice::<HostRequest>(request) {
            Ok(HostRequest::Commands) => {
                return match within(until, self.core.commands(&self.query)).await {
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
                            until,
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
            Ok(HostRequest::ModelState) => within(until, self.core.model_state(&self.query)).await,
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
                within(
                    until,
                    self.core.set_model(&ModelUpdate {
                        agent_id: self.query.agent_id.clone(),
                        audience_id: self.query.audience_id.clone(),
                        scope,
                        selection,
                        expected,
                    }),
                )
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

/// How much of the module's ten-second deadline an interaction's native
/// calls may take together, such as a Run click's catalog lookup and its
/// submission, leaving the module time to answer with their outcome.
const CALLS_LIMIT: Duration = Duration::from_secs(7);
/// How long posting the marker may take inside the calls' limit.
const MARKER_LIMIT: Duration = Duration::from_secs(4);
/// Attempts to hand a submission to core while core is briefly unreachable.
const SUBMIT_ATTEMPTS: u32 = 3;

/// Sends text to the conversation as the invoking person's: a silent marker
/// message in the conversation shows what was asked and by whom, and is the
/// event's identity and feedback target; core then accepts the event like
/// any message, and the answer is delivered to the conversation. A marker
/// Discord does not confirm in time may still have been posted, but without
/// its ID there is no event: the command is not sent to core, and the
/// marker's wording claims nothing ran.
///
/// The submission first waits for its turn in the conversation's lane, after
/// the messages before it. It returns by `deadline`, the end of its
/// interaction's calls, so the module can still say what happened. Before
/// it, posting the marker takes at most [`MARKER_LIMIT`], and core's attempts
/// take the rest,
/// beside the wait of at most
/// [`PROMPT_FEEDBACK`](crate::delivery::PROMPT_FEEDBACK) for the
/// processing reaction and typing, which never holds them back. An attempt
/// core has not answered in time may have been accepted, which the answer
/// says, and the marker's ID makes a repeated submission the same event. A
/// refused submission's reaction is then shown without the answer waiting
/// for it. An unconfirmed one is left to the conversation's lane, which
/// submits it again under its own identity until core accepts or refuses
/// it, as for a message, before the conversation goes on. Once the provider
/// is stopping, neither starts an attempt after the first: the attempt under
/// way ends, and a command core then has not answered for is left
/// unconfirmed.
async fn submit(
    core: &CoreClient,
    submissions: &Submissions,
    agent_id: &str,
    audience_id: &str,
    text: &str,
    intent: Intent,
    deadline: Instant,
) -> Result<(), String> {
    let Submissions {
        http,
        reactions,
        lanes,
        origin,
        settings,
    } = submissions;
    let channel: Id<ChannelMarker> = origin
        .channel_id
        .parse()
        .map_err(|_| "The conversation's channel is not valid.".to_owned())?;
    // The command takes its place among the conversation's messages, so
    // core accepts it after the messages before it and before those after.
    // The lane waits for it while it holds the turn, and settles it if core's
    // answer does not come by the deadline.
    let turn = match timeout_at(deadline, lanes.turn(&origin.channel_id)).await {
        Ok(Some(turn)) => turn,
        Ok(None) => {
            return Err(
                "Carillon is too busy or restarting, so it was not sent; please run it again shortly."
                    .to_owned(),
            );
        }
        Err(_) => {
            return Err("Earlier messages in this conversation are still being prepared, so it was not sent; please run it again once they are answered.".to_owned());
        }
    };
    let shown = text.replace('`', "ˋ");
    let shown: String = shown.chars().take(1900).collect();
    let silent = AllowedMentions::default();
    // The marker's answer, body included, is bounded: it is the event's identity.
    let marker = timeout_at(deadline.min(Instant::now() + MARKER_LIMIT), async {
        http.create_message(channel)
            .allowed_mentions(Some(&silent))
            .content(&format!("<@{}> asked to run `{shown}`", origin.user_id))
            .await
            .map_err(|error| error.to_string())?
            .model()
            .await
            .map_err(|error| error.to_string())
    })
    .await
    .unwrap_or_else(|_| Err("Discord did not answer in time".to_owned()))
    .map_err(|error| {
        warn!(%error, "command marker message was not confirmed");
        "Discord did not confirm the marker message, so the command was not sent to Carillon."
            .to_owned()
    })?;
    let event = submitted_event(
        agent_id,
        audience_id,
        origin,
        &marker.id.to_string(),
        text,
        intent,
    );
    // Admission registers the processing request at once, so the outcome
    // waits for it. Core's attempts then run beside the optional wait for the
    // feedback, which never takes time they need.
    let shown = timeout_at(deadline, reactions.admission(&event, settings));
    let stopping = turn.stopping();
    let attempts = async {
        let retry = Duration::from_millis(500);
        let mut attempt = 1;
        loop {
            let failure = match timeout_at(deadline, core.submit_event(&event)).await {
                Ok(Ok(_)) => return Ok(()),
                Ok(Err(failure)) if !failure.is_transient() => {
                    warn!(error = %failure.message, event_id = %event.event_id, "core refused a command");
                    return Err((Feedback::Failed, "Carillon could not accept it."));
                }
                Ok(Err(failure)) => failure.message,
                Err(_) => "core did not answer in time".to_owned(),
            };
            if attempt < SUBMIT_ATTEMPTS && Instant::now() + retry < deadline {
                warn!(error = %failure, event_id = %event.event_id, "waiting to submit a command");
                attempt += 1;
                // However the wait ends, a provider stopping by then starts
                // no other attempt.
                stopping
                    .run_until_cancelled(tokio::time::sleep(retry))
                    .await;
                if !stopping.is_cancelled() {
                    continue;
                }
            }
            warn!(error = %failure, event_id = %event.event_id, "command submission unconfirmed");
            return Err((
                Feedback::Uncertain,
                "Carillon did not confirm that it accepted it; if it did, the answer follows in this conversation.",
            ));
        }
    };
    let (_, submitted) = tokio::join!(shown, attempts);
    let Err((feedback, refusal)) = submitted else {
        return Ok(());
    };
    match feedback {
        // Core may still be accepting it: the lane settles it before the
        // conversation goes on, and shows a refusal if one comes.
        Feedback::Uncertain => turn.settle(event),
        refused => reactions.show_outcome(event.feedback_target_id, refused),
    }
    Err(refusal.to_owned())
}

/// Where a module comes from.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum Source {
    /// The module shipped with the release, served unless the provider's
    /// settings enable a custom one.
    Bundled,
    /// The module at `path`, which the provider's settings enable.
    Custom { path: PathBuf },
}

impl fmt::Display for Source {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bundled => formatter.write_str("the bundled Discord module"),
            Self::Custom { path } => {
                write!(formatter, "the custom Discord module {}", path.display())
            }
        }
    }
}

/// The files of one provider instance's module.
#[derive(Clone, Debug)]
pub(crate) struct ModuleFiles {
    /// The release's module; release updates replace it.
    pub bundled: PathBuf,
    /// What Discord last confirmed as registered.
    pub record: PathBuf,
    /// The installation whose maintenance lock serializes changes.
    pub installation: PathBuf,
}

impl ModuleFiles {
    /// The files of a configured instance whose bundled module is `bundled`.
    pub(crate) fn of(config: &LoadedConfig, provider_id: &str, bundled: PathBuf) -> Self {
        Self {
            bundled,
            record: crate::config::state_directory(config, provider_id).join("commands.json"),
            installation: config.root.clone(),
        }
    }

    /// The bytes of the module `source` names, read only one byte past the
    /// size [`prepare`] refuses.
    pub(crate) fn read(&self, source: &Source) -> Result<Vec<u8>> {
        let path = match source {
            Source::Bundled => &self.bundled,
            Source::Custom { path } => path,
        };
        read_module(path).with_context(|| match source {
            Source::Bundled => format!("read the bundled Discord module {}", path.display()),
            Source::Custom { .. } => format!("read the custom Discord module {}", path.display()),
        })
    }
}

/// A module file's bytes, read only one byte past the size [`prepare`]
/// refuses. Only a regular file, or a link to one, is opened, since a FIFO
/// or device could block or never end; the opened file is checked again.
///
/// # Errors
///
/// Returns an error when the path is not a regular file or the file cannot
/// be read.
pub(crate) fn read_module(path: &Path) -> io::Result<Vec<u8>> {
    let regular = |metadata: fs::Metadata| {
        if metadata.is_file() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a regular file",
            ))
        }
    };
    regular(fs::metadata(path)?)?;
    let file = fs::File::open(path)?;
    regular(file.metadata()?)?;
    let mut bytes = Vec::new();
    file.take(MODULE_LIMIT as u64 + 1).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// One immutable module and the definitions it described. In-flight calls
/// retain their generation when a replacement starts serving.
struct Generation {
    module: Compiled,
    definitions: Vec<Value>,
    source: Source,
    digest: String,
}

impl Generation {
    fn summary(&self) -> ModuleSummary {
        ModuleSummary {
            source: self.source.clone(),
            commands: command_names(&self.definitions),
            digest: self.digest.clone(),
        }
    }
}

/// A module as the operator sees it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ModuleSummary {
    #[serde(flatten)]
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

/// The module serving after an apply, the one before it, and what the
/// change needed from Discord.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Change {
    pub serving: ModuleSummary,
    pub previous: ModuleSummary,
    pub discord: Registration,
}

/// What serves, how the module the settings file declares compares, and
/// whether the record shows the serving definitions registered for this
/// application and guild (absent without a Discord application).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct Status {
    pub serving: ModuleSummary,
    pub declared: Declared,
    pub registered: Option<bool>,
}

/// The module the settings file declares, as an apply would read it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(crate) enum Declared {
    /// It is the module serving, byte for byte.
    Serving,
    /// It is another module, or the serving one's file has changed.
    Other {
        #[serde(flatten)]
        source: Source,
    },
    /// The settings file or the module's file could not be read.
    Unknown { error: String },
}

/// The operator's answer to a refused apply: why, and the module that still
/// serves.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct Refusal {
    pub error: String,
    pub serving: ModuleSummary,
    /// When the file was refused for changing what only a restart applies,
    /// those fields; the rest of it, its module included, was not tried.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub restart: Vec<String>,
}

/// Why an apply left the serving module in place.
#[derive(Debug)]
pub enum ChangeError {
    /// Another maintenance operation holds the installation's lock.
    Busy(PathBuf),
    /// The maintenance lock could not be taken.
    Lock(anyhow::Error),
    /// The declared module is not a usable Discord module.
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

/// The module in use, its command definitions, and the files that hold the
/// bundled module and what Discord holds.
pub(crate) struct Module {
    host: Host<Calls>,
    files: ModuleFiles,
    current: Mutex<Arc<Generation>>,
    /// Queues changes within this process; the maintenance lock refuses
    /// changes from other processes.
    changes: tokio::sync::Mutex<()>,
}

impl Module {
    /// Loads the module `source` names and learns its commands.
    ///
    /// # Errors
    ///
    /// Returns an error when the module is unusable; an unusable custom
    /// module is named, with how to serve the bundled one instead.
    pub(crate) async fn load(files: ModuleFiles, source: Source) -> Result<Self> {
        let host = Host::new()?;
        let bytes = files.read(&source)?;
        let generation = prepare(&host, &bytes, source.clone())
            .await
            .map_err(|error| {
                error.context(match &source {
                    Source::Custom { path } => format!(
                        "custom Discord module {} is unusable; fix it, or disable it in the \
                         provider's settings to serve the bundled module",
                        path.display()
                    ),
                    Source::Bundled => format!(
                        "bundled Discord module {} is unusable",
                        files.bundled.display()
                    ),
                })
            })?;
        Ok(Self {
            host,
            files,
            current: Mutex::new(Arc::new(generation)),
            changes: tokio::sync::Mutex::new(()),
        })
    }

    /// Serves the module `source` names unless it already serves, byte for
    /// byte: validate, register when the record does not show its
    /// definitions, then activate. A module that already serves has its
    /// registration repeated when the record does not show it. Only a change
    /// takes the maintenance lock.
    pub(crate) async fn apply(
        &self,
        source: Source,
        app: Option<&DiscordApp>,
    ) -> Result<Change, ChangeError> {
        let _changes = self.changes.lock().await;
        let bytes = self.files.read(&source).map_err(ChangeError::Unusable)?;
        let serving = self.current();
        let unchanged = serving.source == source && serving.digest == digest(&bytes);
        if unchanged && app.is_none_or(|app| self.registered(app, &serving.definitions)) {
            return Ok(Change {
                serving: serving.summary(),
                previous: serving.summary(),
                discord: if app.is_some() {
                    Registration::Unchanged
                } else {
                    Registration::NotConnected
                },
            });
        }
        let _lock = MaintenanceLock::try_acquire(&self.files.installation)
            .map_err(ChangeError::Lock)?
            .ok_or_else(|| ChangeError::Busy(self.files.installation.clone()))?;
        if unchanged {
            let discord = self
                .reconcile_definitions(app, &serving.definitions)
                .await?;
            return Ok(Change {
                serving: serving.summary(),
                previous: serving.summary(),
                discord,
            });
        }
        let candidate = prepare(&self.host, &bytes, source)
            .await
            .map_err(ChangeError::Unusable)?;
        let discord = self
            .reconcile_definitions(app, &candidate.definitions)
            .await?;
        let serving = candidate.summary();
        let previous = self.activate(candidate).summary();
        Ok(Change {
            serving,
            previous,
            discord,
        })
    }

    /// Registers the serving definitions unless the record shows Discord
    /// already holds them.
    pub(crate) async fn reconcile(&self, app: &DiscordApp) -> Result<Registration, ChangeError> {
        let _changes = self.changes.lock().await;
        let definitions = self.current().definitions.clone();
        self.reconcile_definitions(Some(app), &definitions).await
    }

    /// How the module `source` names compares with the one serving.
    pub(crate) fn declared(&self, source: Source) -> Declared {
        match self.files.read(&source) {
            Ok(bytes) => {
                let serving = self.current();
                if serving.source == source && serving.digest == digest(&bytes) {
                    Declared::Serving
                } else {
                    Declared::Other { source }
                }
            }
            Err(error) => Declared::Unknown {
                error: format!("{error:#}"),
            },
        }
    }

    /// The serving module, the declared one, and whether the record shows
    /// the serving definitions registered.
    pub(crate) fn status(&self, app: Option<&DiscordApp>, declared: Declared) -> Status {
        let serving = self.current();
        Status {
            serving: serving.summary(),
            declared,
            registered: app.map(|app| self.registered(app, &serving.definitions)),
        }
    }

    pub(crate) fn serving(&self) -> ModuleSummary {
        self.current().summary()
    }

    pub(crate) async fn invoke(
        &self,
        calls: InteractionCalls,
        interaction: Interaction,
    ) -> Result<Reply> {
        let generation = self.current();
        let payload = serde_json::to_vec(&Invocation::Interaction(interaction))?;
        let calls = Calls::Interaction {
            calls: Box::new(calls),
            until: Instant::now() + CALLS_LIMIT,
        };
        match outcome(&self.host, &generation.module, calls, &payload, DEADLINE).await? {
            Outcome::Reply(reply) => Ok(reply),
            Outcome::Failure(message) => bail!("Discord module failed: {message}"),
            Outcome::Description(_) | Outcome::Card(_) => {
                bail!("Discord module answered an interaction with something else")
            }
        }
    }

    /// Renders a job's card through the module serving now: the embed to
    /// show, or `None` when a settled job's card goes. The module makes no
    /// native call, and what it answers is checked against Discord's embed
    /// limits. A failure shows nothing new; the card keeps what it showed.
    pub(crate) async fn render_card(&self, request: &CardRequest) -> Result<Option<Embed>> {
        let generation = self.current();
        card(&self.host, &generation.module, request).await
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
        let source = candidate.source.clone();
        let previous = std::mem::replace(
            &mut *self.current.lock().unwrap_or_else(PoisonError::into_inner),
            Arc::new(candidate),
        );
        info!(%source, "Discord module activated");
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

/// Validates `bytes` as a Discord module this provider can serve, without
/// serving it: the names of the commands it defines, or why it is unusable.
pub(crate) async fn check(bytes: &[u8]) -> Result<Vec<String>> {
    let generation = prepare(&Host::new()?, bytes, Source::Bundled).await?;
    Ok(command_names(&generation.definitions))
}

fn command_names(definitions: &[Value]) -> Vec<String> {
    definitions
        .iter()
        .filter_map(|definition| definition["name"].as_str())
        .map(str::to_owned)
        .collect()
}

/// Creates the directory holding `path` when needed, then syncs it and its
/// ancestors. This runs on every change: an existing directory does not show
/// that its entry survives a system crash, and repeating a change must finish
/// a sync an earlier attempt could not.
fn publish_directory(path: &Path) -> io::Result<()> {
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
/// including the size limit, so `check` never accepts a module that an
/// apply or startup would refuse.
async fn prepare(host: &Host<Calls>, bytes: &[u8], source: Source) -> Result<Generation> {
    if bytes.len() > MODULE_LIMIT {
        bail!(
            "the module has more than {MODULE_LIMIT} bytes; a Discord module may have at most \
             16 MiB"
        );
    }
    let module = host.load(bytes)?;
    let payload = serde_json::to_vec(&Invocation::Describe)?;
    let description = match outcome(host, &module, Calls::Refused, &payload, DESCRIBE_DEADLINE)
        .await
        .context("describe Discord module")?
    {
        Outcome::Description(description) => description,
        Outcome::Reply(_) | Outcome::Card(_) | Outcome::Failure(_) => {
            bail!("Discord module did not describe itself")
        }
    };
    let Description { contract, commands } = description;
    if contract != CONTRACT {
        bail!("Discord module speaks contract {contract}; this provider speaks {CONTRACT}");
    }
    for definition in &commands {
        command(definition)?;
    }
    // A module that cannot show a job at work, nor end a settled job's card,
    // would leave every card stale.
    for (case, request) in smoke_cards() {
        card(host, &module, &request)
            .await
            .with_context(|| format!("render a card of {case}"))?;
    }
    Ok(Generation {
        module,
        definitions: commands,
        source,
        digest: digest(bytes),
    })
}

/// One card through `module`, which makes no native call: the embed it
/// shows, valid as Discord takes it, or `None` for a settled job whose card
/// goes.
async fn card(
    host: &Host<Calls>,
    module: &Compiled,
    request: &CardRequest,
) -> Result<Option<Embed>> {
    let payload = serde_json::to_vec(&InvocationRef::Card(request))?;
    if payload.len() > CARD_REQUEST_LIMIT {
        bail!(
            "the job's card request has {} bytes, more than the {CARD_REQUEST_LIMIT} a module is given",
            payload.len()
        );
    }
    let card = match outcome(host, module, Calls::Refused, &payload, CARD_DEADLINE).await? {
        Outcome::Card(card) => card,
        Outcome::Failure(message) => bail!("Discord module failed: {message}"),
        Outcome::Description(_) | Outcome::Reply(_) => {
            bail!("Discord module answered a card with something else")
        }
    };
    match card {
        Card::Show(embed) => {
            let embed: Embed = serde_json::from_value(embed).context("decode the card's embed")?;
            twilight_validate::embed::embed(&embed).context("the card's embed is unusable")?;
            Ok(Some(embed))
        }
        Card::Remove if request.job.settled().is_some() => Ok(None),
        Card::Remove => bail!("Discord module removed the card of a job that has not settled"),
    }
}

#[derive(Serialize)]
enum InvocationRef<'a> {
    Card(&'a CardRequest),
}

/// Jobs a module must show on a card before it serves: one at work, and
/// settled ones with and without a summary to keep.
fn smoke_cards() -> [(&'static str, CardRequest); 3] {
    let turn = |state, activity: Option<TurnActivity>, delivery| JobTurn {
        turn_id: "turn".to_owned(),
        state,
        accepted_at: 1_790_000_000,
        finished_at: (state != TurnState::Running).then_some(1_790_000_045),
        activity,
        delivery,
    };
    let request = |turn| CardRequest {
        job: Job {
            origin: Origin {
                turn_id: "turn".to_owned(),
                accepted_at: 1_790_000_000,
            },
            target_id: "1".to_owned(),
            turns: vec![turn],
            earlier: false,
        },
        seen: None,
        markers: ActivityMarkers::default(),
    };
    let answer = TurnActivity {
        phase: ActivityPhase::Model,
        headline: Some("Answering.".to_owned()),
        running: Vec::new(),
        recent: Vec::new(),
        tool_calls: 0,
        work: None,
        notes: Vec::new(),
        agent: None,
        partial: false,
    };
    [
        (
            "a job at work",
            request(turn(TurnState::Running, Some(answer.clone()), None)),
        ),
        (
            "a stopped job",
            request(turn(
                TurnState::Interrupted,
                None,
                Some(DeliveryState::Delivered),
            )),
        ),
        (
            "an answered job",
            request(turn(
                TurnState::Completed,
                Some(answer),
                Some(DeliveryState::Delivered),
            )),
        ),
    ]
}

async fn outcome(
    host: &Host<Calls>,
    module: &Compiled,
    calls: Calls,
    payload: &[u8],
    deadline: Duration,
) -> Result<Outcome> {
    let output = host
        .invoke(module, calls, payload, deadline)
        .await
        .map_err(anyhow::Error::from)?;
    serde_json::from_slice(&output).context("decode Discord module outcome")
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
mod measurement;

#[cfg(test)]
pub(crate) mod tests {
    use std::{
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicU16, AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use anyhow::Context as _;
    use axum::{Router, http::StatusCode, routing::put};
    use carillon_discord_module::contract::{CardRequest, Interaction, InteractionKind};
    use carillon_installation::maintenance::MaintenanceLock;
    use serde_json::{Value, json};

    use super::{
        Change, ChangeError, Declared, DiscordApp, MODULE_LIMIT, Module, ModuleFiles, Registration,
        Source,
    };

    /// `text` as the contents of a WAT string.
    fn escape(text: &str) -> String {
        text.replace('\\', "\\\\").replace('"', "\\\"")
    }

    /// A module is read from a regular file or a link to one. A directory,
    /// a device or a FIFO is refused without waiting on it.
    #[test]
    fn only_a_regular_module_file_is_read() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let file = directory.path().join("module.wasm");
        std::fs::write(&file, b"module")?;
        let link = directory.path().join("link.wasm");
        std::os::unix::fs::symlink(&file, &link)?;
        assert_eq!(super::read_module(&file)?, b"module");
        assert_eq!(super::read_module(&link)?, b"module");
        let fifo = directory.path().join("fifo.wasm");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()?
                .success()
        );
        for path in [
            fifo,
            directory.path().to_owned(),
            PathBuf::from("/dev/null"),
        ] {
            // Bounded from outside: a read that waited would fail the test,
            // not hold up the gate.
            let (sent, received) = std::sync::mpsc::channel();
            let read = path.clone();
            std::thread::spawn(move || {
                sent.send(super::read_module(&read).map_err(|error| error.kind()))
            });
            let outcome = received
                .recv_timeout(Duration::from_secs(5))
                .with_context(|| format!("reading {} did not return", path.display()))?;
            assert_eq!(
                outcome,
                Err(std::io::ErrorKind::InvalidInput),
                "{}",
                path.display()
            );
        }
        Ok(())
    }

    /// A module that answers `Describe` with `description`, a card with an
    /// embed that says so, and every other invocation with `reply`, in binary
    /// form.
    pub(crate) fn module(description: &str, reply: &str) -> anyhow::Result<Vec<u8>> {
        module_with_card(description, reply, &showing("A test card"))
    }

    /// [`module`], answering every card with `card`.
    pub(crate) fn module_with_card(
        description: &str,
        reply: &str,
        card: &str,
    ) -> anyhow::Result<Vec<u8>> {
        let describe_length =
            serde_json::to_vec(&carillon_discord_module::contract::Invocation::Describe)?.len();
        // A card request begins `{"Card"`; an interaction `{"Interaction"`.
        let text = format!(
            r#"(module
  (memory (export "memory") 1)
  (global $length (mut i32) (i32.const 0))
  (global $output (mut i32) (i32.const 0))
  (data (i32.const 1024) "{}")
  (data (i32.const 4096) "{}")
  (data (i32.const 16384) "{}")
  (func (export "input") (param $length i32) (result i32)
    (global.set $length (local.get $length))
    (i32.const 32768))
  (func (export "invoke") (result i32)
    (if (result i32) (i32.eq (global.get $length) (i32.const {describe_length}))
      (then (global.set $output (i32.const 1024)) (i32.const {}))
      (else
        (if (result i32) (i32.eq (i32.load8_u (i32.const 32770)) (i32.const 67))
          (then (global.set $output (i32.const 16384)) (i32.const {}))
          (else (global.set $output (i32.const 4096)) (i32.const {}))))))
  (func (export "output") (result i32) (global.get $output)))"#,
            escape(description),
            escape(reply),
            escape(card),
            description.len(),
            card.len(),
            reply.len(),
        );
        Ok(wat::parse_str(text)?)
    }

    /// The release module as built for this tree: the file
    /// `CARILLON_DISCORD_MODULE` names, or `mise run build:module`'s output.
    pub(crate) fn release_module_bytes() -> anyhow::Result<Vec<u8>> {
        let path = std::env::var_os("CARILLON_DISCORD_MODULE").map_or_else(
            || {
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../target/wasm32-unknown-unknown/wasm/carillon_discord_bundled.wasm")
            },
            PathBuf::from,
        );
        std::fs::read(&path).with_context(|| {
            format!(
                "read the built Discord module {}; build it with `mise run build:module`",
                path.display()
            )
        })
    }

    /// The example module as built for this tree: the file
    /// `CARILLON_EXAMPLE_MODULE` names, or `mise run build:module`'s output.
    pub(crate) fn example_module_bytes() -> anyhow::Result<Vec<u8>> {
        let path = std::env::var_os("CARILLON_EXAMPLE_MODULE").map_or_else(
            || {
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../target/wasm32-unknown-unknown/wasm/carillon_card_colours.wasm")
            },
            PathBuf::from,
        );
        std::fs::read(&path).with_context(|| {
            format!(
                "read the built example module {}; build it with `mise run build:module`",
                path.display()
            )
        })
    }

    /// The release module, compiled once for the tests that draw real cards.
    pub(crate) async fn release_module() -> anyhow::Result<Arc<Module>> {
        static RELEASE: tokio::sync::OnceCell<Arc<Module>> = tokio::sync::OnceCell::const_new();
        RELEASE
            .get_or_try_init(|| async {
                let directory = tempfile::tempdir()?;
                std::fs::write(
                    directory.path().join("release.wasm"),
                    release_module_bytes()?,
                )?;
                Ok::<_, anyhow::Error>(Arc::new(
                    Module::load(files(directory.path()), Source::Bundled).await?,
                ))
            })
            .await
            .cloned()
    }

    /// A card answer showing an embed described by `text`.
    pub(crate) fn showing(text: &str) -> String {
        json!({"Card": {"Show": {"type": "rich", "description": text}}}).to_string()
    }

    /// [`module`], written to `path`.
    pub(crate) fn write_module(path: &Path, description: &str, reply: &str) -> anyhow::Result<()> {
        std::fs::write(path, module(description, reply)?)?;
        Ok(())
    }

    pub(crate) fn describing(commands: &[Value]) -> String {
        json!({"Description": {"contract": carillon_discord_module::contract::CONTRACT, "commands": commands}}).to_string()
    }

    pub(crate) fn replying(content: &str, components: &[Value]) -> String {
        json!({"Reply": {"content": content, "components": components}}).to_string()
    }

    pub(crate) fn definition(name: &str) -> Value {
        json!({"type": 1, "name": name, "description": "A test command"})
    }

    /// Command files in `root`: the bundled module at `release.wasm`, the
    /// record under `state/`, and `root` as the installation.
    pub(crate) fn files(root: &Path) -> ModuleFiles {
        ModuleFiles {
            bundled: root.join("release.wasm"),
            record: root.join("state/commands.json"),
            installation: root.to_path_buf(),
        }
    }

    /// The custom module `name` in `root`.
    pub(crate) fn custom(root: &Path, name: &str) -> Source {
        Source::Custom {
            path: root.join(name),
        }
    }

    /// What an apply that changed no module reports, for tests of settings
    /// alone.
    pub(crate) fn unchanged() -> Change {
        let summary = super::ModuleSummary {
            source: Source::Bundled,
            commands: Vec::new(),
            digest: String::new(),
        };
        Change {
            serving: summary.clone(),
            previous: summary,
            discord: Registration::NotConnected,
        }
    }

    pub(crate) async fn release_commands(
        root: &Path,
        names: &[&str],
        reply: &str,
    ) -> anyhow::Result<Module> {
        let definitions: Vec<Value> = names.iter().map(|name| definition(name)).collect();
        write_module(
            &root.join("release.wasm"),
            &describing(&definitions),
            &replying(reply, &[]),
        )?;
        Module::load(files(root), Source::Bundled).await
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
    /// Discord's shape for failures.
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

    fn calls() -> anyhow::Result<super::InteractionCalls> {
        Ok(super::InteractionCalls::new(
            crate::client::test_client("http://127.0.0.1:1".to_owned())?,
            carillon_protocol::model::ModelQuery {
                agent_id: "default".to_owned(),
                audience_id: None,
            },
        ))
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

    async fn reply(commands: &Module) -> anyhow::Result<String> {
        Ok(commands.invoke(calls()?, interaction()).await?.content)
    }

    #[tokio::test]
    async fn loading_checks_the_contract_and_definitions() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let release = directory.path().join("release.wasm");
        let reply = replying("unused", &[]);
        let contract = carillon_discord_module::contract::CONTRACT;
        for (description, refused) in [
            (
                json!({"Description": {"contract": contract + 1, "commands": []}}).to_string(),
                format!(
                    "Discord module speaks contract {}; this provider speaks {contract}",
                    contract + 1
                ),
            ),
            (
                describing(&[json!({"name": "missing-type"})]),
                "invalid command definition: missing field `description`".to_owned(),
            ),
            (
                reply.clone(),
                "Discord module did not describe itself".to_owned(),
            ),
            (
                "not json".to_owned(),
                "describe Discord module: decode Discord module outcome: expected value at line 1 column 1"
                    .to_owned(),
            ),
        ] {
            let bytes = module(&description, &reply)?;
            let checked = super::check(&bytes).await.err().context("check accepted")?;
            assert_eq!(format!("{checked:#}"), refused);
            std::fs::write(&release, bytes)?;
            let loaded = Module::load(files(directory.path()), Source::Bundled)
                .await
                .err()
                .context("load accepted")?;
            assert_eq!(
                format!("{loaded:#}"),
                format!(
                    "bundled Discord module {} is unusable: {refused}",
                    release.display()
                )
            );
        }
        let broken = super::check(b"\0asm broken")
            .await
            .err()
            .context("a broken module was accepted")?;
        assert!(
            format!("{broken:#}").starts_with("compile guest module: "),
            "{broken:#}"
        );
        assert_eq!(
            super::check(&variant(&["model"], "unused")?).await?,
            ["model"]
        );
        let commands = release_commands(directory.path(), &["model"], "unused").await?;
        assert_eq!(commands.serving().source, Source::Bundled);
        assert_eq!(commands.serving().commands, ["model"]);
        Ok(())
    }

    #[tokio::test]
    async fn applying_serves_only_usable_modules_and_rereads_the_declared_file()
    -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let commands = release_commands(root, &["model"], "release").await?;
        let path = root.join("custom.wasm");
        let declared = custom(root, "custom.wasm");

        // A missing or unusable candidate changes nothing.
        let unread: fn(&Declared) -> bool = |declared| matches!(declared, Declared::Unknown { .. });
        let other: fn(&Declared) -> bool = |declared| {
            matches!(
                declared,
                Declared::Other {
                    source: Source::Custom { .. }
                }
            )
        };
        for (bytes, compared) in [(None, unread), (Some(b"\0asm broken".as_slice()), other)] {
            if let Some(bytes) = bytes {
                std::fs::write(&path, bytes)?;
            }
            assert!(matches!(
                commands.apply(declared.clone(), None).await,
                Err(ChangeError::Unusable(_))
            ));
            assert_eq!(reply(&commands).await?, "release");
            let found = commands.declared(declared.clone());
            assert!(compared(&found), "{found:?}");
        }
        assert!(MaintenanceLock::try_acquire(root)?.is_some());

        let first = variant(&["model"], "custom")?;
        std::fs::write(&path, &first)?;
        let change = commands.apply(declared.clone(), None).await?;
        assert_eq!(change.previous.source, Source::Bundled);
        assert_eq!(change.serving.source, declared);
        assert_eq!(change.discord, Registration::NotConnected);
        assert_eq!(reply(&commands).await?, "custom");
        assert_eq!(commands.declared(declared.clone()), Declared::Serving);
        let again = commands.apply(declared.clone(), None).await?;
        assert_eq!(again.previous, again.serving);

        // A rebuild at the same path is another module until it is applied.
        std::fs::write(&path, variant(&["model", "other"], "rebuilt")?)?;
        assert_eq!(
            commands.declared(declared.clone()),
            Declared::Other {
                source: declared.clone()
            }
        );
        assert_eq!(reply(&commands).await?, "custom");
        let change = commands.apply(declared.clone(), None).await?;
        assert_eq!(change.serving.commands, ["model", "other"]);
        assert_ne!(change.serving.digest, change.previous.digest);
        assert_eq!(reply(&commands).await?, "rebuilt");

        // A start serves the declared module.
        let restarted = Module::load(files(root), declared.clone()).await?;
        assert_eq!(restarted.serving(), change.serving);
        assert_eq!(reply(&restarted).await?, "rebuilt");

        // A rebuild that breaks keeps the module serving, and refuses a start,
        // naming the file and how to serve the bundled module instead.
        std::fs::write(&path, b"\0asm broken")?;
        assert!(matches!(
            commands.apply(declared.clone(), None).await,
            Err(ChangeError::Unusable(_))
        ));
        assert_eq!(reply(&commands).await?, "rebuilt");
        let failure = Module::load(files(root), declared.clone())
            .await
            .err()
            .map(|error| format!("{error:#}"))
            .unwrap_or_default();
        assert!(
            failure.contains(&format!("custom Discord module {}", path.display()))
                && failure.contains("disable it"),
            "{failure}"
        );

        // Disabled, the bundled module serves.
        let change = commands.apply(Source::Bundled, None).await?;
        assert_eq!(change.serving.source, Source::Bundled);
        assert_eq!(reply(&commands).await?, "release");
        Ok(())
    }

    #[tokio::test]
    async fn changes_refused_during_other_maintenance_change_nothing() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let commands = release_commands(root, &["model"], "release").await?;
        let status = Arc::new(AtomicU16::new(200));
        let (address, server, requests) = counting_registration_server(status).await?;
        let target = app(&address.to_string(), 1, 100);
        std::fs::write(root.join("custom.wasm"), variant(&["other"], "custom")?)?;
        let held = MaintenanceLock::acquire(root)?;
        // An apply that changes nothing needs no lock.
        let unchanged = commands.apply(Source::Bundled, None).await?;
        assert_eq!(unchanged.previous, unchanged.serving);
        for source in [custom(root, "custom.wasm"), Source::Bundled] {
            assert!(matches!(
                commands.apply(source, Some(&target)).await,
                Err(ChangeError::Busy(_))
            ));
        }
        assert_eq!(requests.load(Ordering::SeqCst), 0);
        assert_eq!(reply(&commands).await?, "release");
        drop(held);
        commands
            .apply(custom(root, "custom.wasm"), Some(&target))
            .await?;
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
        let registered =
            |commands: &Module| commands.status(Some(&target), Declared::Serving).registered;
        assert_eq!(registered(&commands), Some(true));

        std::fs::write(
            root.join("custom.wasm"),
            variant(&["model", "other"], "custom")?,
        )?;
        for code in [400, 500] {
            status.store(code, Ordering::SeqCst);
            assert!(matches!(
                commands
                    .apply(custom(root, "custom.wasm"), Some(&target))
                    .await,
                Err(ChangeError::Register(_))
            ));
            assert_eq!(reply(&commands).await?, "release");
            // Discord may hold either definition set now, so the record
            // attests to neither and the next change or start registers.
            assert!(!files(root).record.exists());
            assert_eq!(registered(&commands), Some(false));
        }

        // Applying the serving module again reconciles Discord.
        status.store(200, Ordering::SeqCst);
        let before = requests.load(Ordering::SeqCst);
        let change = commands.apply(Source::Bundled, Some(&target)).await?;
        assert_eq!(change.previous, change.serving);
        assert_eq!(change.discord, Registration::Registered);
        assert_eq!(requests.load(Ordering::SeqCst), before + 1);
        assert_eq!(registered(&commands), Some(true));
        let change = commands.apply(Source::Bundled, Some(&target)).await?;
        assert_eq!(change.discord, Registration::Unchanged);
        assert_eq!(requests.load(Ordering::SeqCst), before + 1);
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
        std::fs::write(root.join("custom.wasm"), variant(&["other"], "custom")?)?;
        // A directory where the record belongs cannot be removed as a file.
        std::fs::create_dir_all(files(root).record.join("occupied"))?;
        assert!(matches!(
            commands
                .apply(custom(root, "custom.wasm"), Some(&target))
                .await,
            Err(ChangeError::Record(
                carillon_installation::filesystem::PublishError::NotPublished(_)
            ))
        ));
        assert_eq!(requests.load(Ordering::SeqCst), 0);
        assert_eq!(reply(&commands).await?, "release");
        server.abort();
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
    async fn overlapping_applies_register_and_activate_in_order() -> anyhow::Result<()> {
        use tokio::sync::{Semaphore, mpsc};

        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let commands = release_commands(root, &["initial"], "initial").await?;
        let (sent, mut received) = mpsc::unbounded_channel();
        let release = Arc::new(Semaphore::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let target = app(&listener.local_addr()?.to_string(), 1, 100);
        let router = gated_registration(sent, release.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await });
        std::fs::write(root.join("first.wasm"), variant(&["first"], "first")?)?;
        std::fs::write(root.join("second.wasm"), variant(&["second"], "second")?)?;
        let first = commands.apply(custom(root, "first.wasm"), Some(&target));
        tokio::pin!(first);
        let registration = tokio::select! {
            _ = &mut first => anyhow::bail!("apply completed before confirmation"),
            request = received.recv() => request.ok_or_else(|| anyhow::anyhow!("registration closed"))?,
        };
        assert_eq!(registration[0]["name"], "first");
        let second = commands.apply(custom(root, "second.wasm"), Some(&target));
        tokio::pin!(second);
        // Poll both operations while registration is blocked. The second
        // must not send a request or activate; ordinary invocations still work.
        tokio::select! {
            _ = &mut first => anyhow::bail!("first activated before confirmation"),
            _ = &mut second => anyhow::bail!("second bypassed the first apply"),
            request = received.recv() => anyhow::bail!("overlapping registration: {request:?}"),
            () = tokio::time::sleep(Duration::from_millis(100)) => {},
        }
        assert_eq!(reply(&commands).await?, "initial");
        release.add_permits(1);
        assert_eq!(first.await?.discord, Registration::Registered);
        assert_eq!(commands.serving().commands, ["first"]);
        assert_eq!(reply(&commands).await?, "first");
        let registration = tokio::select! {
            _ = &mut second => anyhow::bail!("second completed before confirmation"),
            request = received.recv() => request.ok_or_else(|| anyhow::anyhow!("registration closed"))?,
        };
        assert_eq!(registration[0]["name"], "second");
        release.add_permits(1);
        assert_eq!(second.await?.discord, Registration::Registered);
        assert_eq!(commands.serving().commands, ["second"]);
        assert_eq!(
            commands.status(Some(&target), Declared::Serving).registered,
            Some(true)
        );
        assert_eq!(reply(&commands).await?, "second");
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn an_interaction_under_way_finishes_with_the_module_it_began_with() -> anyhow::Result<()>
    {
        use axum::routing::get;
        use tokio::sync::{Semaphore, mpsc};

        let directory = tempfile::tempdir()?;
        let root = directory.path();
        std::fs::write(root.join("release.wasm"), running_module()?)?;
        let commands = Arc::new(Module::load(files(root), Source::Bundled).await?);
        let (asked, mut asking) = mpsc::unbounded_channel();
        let release = Arc::new(Semaphore::new(0));
        let gate = release.clone();
        let router = Router::new().route(
            "/v1/commands",
            get(move || {
                let (asked, release) = (asked.clone(), gate.clone());
                async move {
                    let _ = asked.send(());
                    let _permit = release.acquire().await;
                    StatusCode::SERVICE_UNAVAILABLE
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let core = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(async move { axum::serve(listener, router).await });
        let calls = super::InteractionCalls::new(
            crate::client::test_client(core)?,
            carillon_protocol::model::ModelQuery {
                agent_id: "default".to_owned(),
                audience_id: None,
            },
        );
        let under_way = tokio::spawn({
            let commands = commands.clone();
            async move { commands.invoke(calls, interaction()).await }
        });
        asking
            .recv()
            .await
            .context("the interaction never asked core")?;
        std::fs::write(
            root.join("custom.wasm"),
            variant(&["model"], "replacement")?,
        )?;
        commands.apply(custom(root, "custom.wasm"), None).await?;
        assert_eq!(reply(&commands).await?, "replacement");
        release.add_permits(1);
        assert_eq!(under_way.await??.content, "ran");
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn registration_is_recorded_for_its_target_once_confirmed() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let commands = release_commands(directory.path(), &["model"], "unused").await?;
        let record = files(directory.path()).record;
        let definitions = commands.current().definitions.clone();
        let (address, server, requests) =
            counting_registration_server(Arc::new(AtomicU16::new(200))).await?;
        let target = app(&address.to_string(), 1, 100);

        // A record in the shape an earlier provider wrote names no target.
        std::fs::create_dir_all(record.parent().unwrap_or_else(|| directory.path()))?;
        std::fs::write(&record, serde_json::to_vec(&definitions)?)?;
        assert_eq!(commands.reconcile(&target).await?, Registration::Registered);
        assert_eq!(commands.reconcile(&target).await?, Registration::Unchanged);
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        for other in [
            app(&address.to_string(), 1, 101),
            app(&address.to_string(), 2, 100),
        ] {
            assert_eq!(commands.reconcile(&other).await?, Registration::Registered);
        }
        assert_eq!(requests.load(Ordering::SeqCst), 3);
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn a_confirmed_module_activates_even_when_recording_fails() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let commands = release_commands(root, &["old"], "old").await?;
        std::fs::write(root.join("new.wasm"), variant(&["new"], "new")?)?;
        let blocked = files(root).record;
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
            .apply(custom(root, "new.wasm"), Some(&target))
            .await?;
        assert_eq!(change.discord, Registration::Registered);
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
    async fn checking_applying_and_starting_share_one_size_limit() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let commands = release_commands(root, &["model"], "release").await?;
        let base = variant(&["model"], "padded")?;
        let at_limit = padded(base.clone(), MODULE_LIMIT)?;
        let over = padded(base, MODULE_LIMIT + 1)?;
        let declared = custom(root, "custom.wasm");

        let refusal = super::check(&over)
            .await
            .err()
            .map(|error| format!("{error:#}"))
            .unwrap_or_default();
        assert!(
            refusal.contains(&format!("more than {MODULE_LIMIT} bytes")),
            "{refusal}"
        );
        std::fs::write(root.join("custom.wasm"), &over)?;
        assert!(matches!(
            commands.apply(declared.clone(), None).await,
            Err(ChangeError::Unusable(_))
        ));
        let unstarted = Module::load(files(root), declared.clone())
            .await
            .err()
            .context("an oversized module started")?;
        assert!(
            format!("{unstarted:#}").contains(&format!("more than {MODULE_LIMIT} bytes")),
            "{unstarted:#}"
        );
        assert_eq!(reply(&commands).await?, "release");

        assert_eq!(super::check(&at_limit).await?, ["model"]);
        std::fs::write(root.join("custom.wasm"), &at_limit)?;
        commands.apply(declared.clone(), None).await?;
        assert_eq!(reply(&commands).await?, "padded");
        assert_eq!(
            Module::load(files(root), declared)
                .await?
                .serving()
                .commands,
            ["model"]
        );
        Ok(())
    }

    /// Run under the persistence suite's syscall fault injection: the first
    /// registration creates the state directory for its record, and a repeat
    /// must sync that directory and its ancestors again rather than trust
    /// that it exists.
    #[tokio::test]
    async fn a_new_state_directory_is_synced_again_by_a_repeat() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let commands = release_commands(root, &["model"], "release").await?;
        assert!(!root.join("state").exists());
        let (address, server) = registration_server(Arc::new(AtomicU16::new(200))).await?;
        let target = app(&address.to_string(), 1, 100);
        std::fs::write(
            root.join("custom.wasm"),
            variant(&["model", "other"], "custom")?,
        )?;
        let declared = custom(root, "custom.wasm");
        let first = match commands.apply(declared.clone(), Some(&target)).await {
            Ok(change) => format!("{:?}", change.discord),
            Err(ChangeError::Record(error)) => format!("refused: record ({error})"),
            Err(error) => anyhow::bail!("unexpected refusal: {error}"),
        };
        eprintln!("first change: {first}");
        commands.apply(declared, Some(&target)).await?;
        assert!(commands.registered(&target, &commands.current().definitions));
        assert_eq!(reply(&commands).await?, "custom");
        eprintln!("repeat: registered");
        server.abort();
        Ok(())
    }

    /// Run under the persistence suite's syscall fault injection: a change
    /// clears the record durably before it registers, reports where that
    /// stopped, and a repeat must register.
    #[tokio::test]
    async fn clearing_the_record_reports_its_boundary_and_a_repeat_registers() -> anyhow::Result<()>
    {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let commands = release_commands(root, &["model"], "release").await?;
        // An existing state directory: the first sync of it clears the record.
        std::fs::create_dir(root.join("state"))?;
        let (address, server, requests) =
            counting_registration_server(Arc::new(AtomicU16::new(200))).await?;
        let target = app(&address.to_string(), 1, 100);
        std::fs::write(
            root.join("custom.wasm"),
            variant(&["model", "other"], "custom")?,
        )?;
        let declared = custom(root, "custom.wasm");
        let first = match commands.apply(declared.clone(), Some(&target)).await {
            Ok(change) => format!("{:?}", change.discord),
            Err(ChangeError::Record(error)) => format!("refused: record ({error})"),
            Err(error) => anyhow::bail!("unexpected refusal: {error}"),
        };
        eprintln!(
            "first change {first}; Discord requests {}",
            requests.load(Ordering::SeqCst)
        );
        commands.apply(declared, Some(&target)).await?;
        assert!(commands.registered(&target, &commands.current().definitions));
        assert_eq!(reply(&commands).await?, "custom");
        eprintln!("repeat: registered");
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn model_calls_preserve_the_conditional_write() -> anyhow::Result<()> {
        use axum::Json;
        use carillon_discord_module::contract::{HostReply, HostRequest};
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
            crate::client::test_client(format!("http://{address}"))?,
            ModelQuery {
                agent_id: "default".to_owned(),
                audience_id: None,
            },
        );
        let reply = calls
            .answer(
                &serde_json::to_vec(&HostRequest::SetModel {
                    scope: Scope::Agent,
                    selection: Some(selection.clone()),
                    expected: Some(selection),
                })?,
                calls_until(),
            )
            .await;
        assert_eq!(reply, HostReply::Error("changed while applying".to_owned()));
        server.abort();
        Ok(())
    }

    /// Core and Discord in one fake: lists `catalog` after `listing`, accepts
    /// every event after `admission` and shows feedback after `feedback`,
    /// posts every message as `901`, and traces the requests in order as they
    /// arrive.
    fn submission_server(
        catalog: carillon_protocol::model::CommandCatalog,
        trace: Arc<tokio::sync::Mutex<Vec<String>>>,
        submitted: Arc<tokio::sync::Mutex<Vec<carillon_protocol::model::ExternalEvent>>>,
        (listing, admission, feedback): (Duration, Duration, Duration),
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
                        tokio::time::sleep(listing).await;
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
        use carillon_discord_module::contract::{HostReply, HostRequest, Intent};
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
            (Duration::ZERO, Duration::ZERO, Duration::ZERO),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move { axum::serve(listener, router).await });
        let core = crate::client::test_client(format!("http://{address}"))?;
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
                .answer(&serde_json::to_vec(&HostRequest::Commands)?, calls_until())
                .await,
            HostReply::Commands(catalog)
        );
        assert_eq!(
            unanchored.answer(&submit, calls_until()).await,
            HostReply::Error(
                "Sending to the conversation needs the Discord connection.".to_owned()
            )
        );
        assert!(submitted.lock().await.is_empty());

        let mut config = crate::conversations::tests::discord_config(
            crate::config::DiscordActivation::Always,
            crate::config::DiscordConversation::Thread,
        );
        config.typing = false;
        let http = Arc::new(
            twilight_http::Client::builder()
                .token("test-only".to_owned())
                .proxy(address.to_string(), true)
                .ratelimiter(None)
                .build(),
        );
        let settings = crate::config::Settings::new(config);
        let calls = super::InteractionCalls::new(core.clone(), query).with_submissions(
            super::Submissions {
                reactions: crate::delivery::Reactions::new(http.clone(), settings.clone()),
                lanes: serving_lanes(core, http.clone())?,
                http,
                origin: crate::conversations::InteractionOrigin {
                    guild_id: "100".to_owned(),
                    user_id: "200".to_owned(),
                    channel_id: "400".to_owned(),
                    route_channel_id: "300".to_owned(),
                },
                settings: settings.current(),
            },
        );
        assert_eq!(
            calls.answer(&submit, calls_until()).await,
            HostReply::Submitted
        );
        server.abort();
        let trace = trace.lock().await;
        // The marker is posted silently first, then marked processing and
        // submitted.
        assert_eq!(
            trace[0],
            "message <@200> asked to run `/compact keep ˋthisˋ` mentions {\"parse\":[]}"
        );
        assert!(trace[1..3].iter().any(|request| {
            request.starts_with("PUT /api/v10/channels/400/messages/901/reactions/")
        }));
        assert!(trace[1..3].iter().any(|request| request == "event 901"));
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
        let commands = Module::load(files(directory.path()), Source::Bundled).await?;
        let reply = commands.invoke(calls()?, interaction()).await?;
        assert_eq!(reply.content, "hello");
        assert_eq!(reply.components, vec![button]);
        for (output, refused) in [
            (
                json!({"Failure": "no such command"}).to_string(),
                "Discord module failed: no such command",
            ),
            (
                describing(&[]),
                "Discord module answered an interaction with something else",
            ),
            ("{broken".to_owned(), "decode Discord module outcome: "),
        ] {
            std::fs::write(
                directory.path().join("custom.wasm"),
                module(&describing(&[]), &output)?,
            )?;
            commands
                .apply(custom(directory.path(), "custom.wasm"), None)
                .await?;
            let failure = commands
                .invoke(calls()?, interaction())
                .await
                .err()
                .with_context(|| format!("accepted {output}"))?;
            assert!(format!("{failure:#}").starts_with(refused), "{failure:#}");
        }
        Ok(())
    }

    /// Calls that submit through `router`, as the person in channel `400`,
    /// with the reactions they show.
    async fn submitting(
        router: Router,
        configure: impl FnOnce(&mut crate::config::DiscordConfig),
    ) -> anyhow::Result<(
        super::InteractionCalls,
        crate::delivery::Reactions,
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
        let http = Arc::new(
            twilight_http::Client::builder()
                .token("test-only".to_owned())
                .proxy(address.to_string(), true)
                .ratelimiter(None)
                .build(),
        );
        let reactions =
            crate::delivery::Reactions::new(http.clone(), crate::config::Settings::new(config));
        let core = crate::client::test_client(format!("http://{address}"))?;
        let calls = super::InteractionCalls::new(
            core.clone(),
            carillon_protocol::model::ModelQuery {
                agent_id: "default".to_owned(),
                audience_id: Some("discord-100-400".to_owned()),
            },
        )
        .with_submissions(super::Submissions {
            lanes: serving_lanes(core, http.clone())?,
            http,
            reactions: reactions.clone(),
            origin: crate::conversations::InteractionOrigin {
                guild_id: "100".to_owned(),
                user_id: "200".to_owned(),
                channel_id: "400".to_owned(),
                route_channel_id: "300".to_owned(),
            },
            settings: reactions.settings().current(),
        });
        Ok((calls, reactions, server))
    }

    /// Lanes served by the provider's own dispatcher, in which no message of
    /// the conversation is waiting.
    fn serving_lanes(
        core: crate::client::CoreClient,
        http: Arc<twilight_http::Client>,
    ) -> anyhow::Result<crate::inbound::lanes::Lanes> {
        let directory = tempfile::tempdir()?;
        let participation = Arc::new(crate::conversations::Participation::load(
            directory.path().join("participation.json"),
        )?);
        let ingress = crate::inbound::tests::ingress(
            core,
            crate::conversations::tests::discord_config(
                crate::config::DiscordActivation::Always,
                crate::config::DiscordConversation::Thread,
            ),
            participation,
            http,
            directory.path(),
        )?;
        let (sender, receiver) = tokio::sync::mpsc::channel(crate::inbound::lanes::MAX_PENDING);
        tokio::spawn(async move {
            let _directory = directory;
            crate::inbound::lanes::dispatch(ingress, receiver).await;
        });
        Ok(crate::inbound::lanes::Lanes::new(sender))
    }

    /// The end of an interaction's native calls, were it starting now.
    fn calls_until() -> tokio::time::Instant {
        tokio::time::Instant::now() + super::CALLS_LIMIT
    }

    fn submit_request() -> anyhow::Result<Vec<u8>> {
        Ok(serde_json::to_vec(
            &carillon_discord_module::contract::HostRequest::Submit {
                text: "/compact keep decisions".to_owned(),
                intent: carillon_discord_module::contract::Intent::Command,
            },
        )?)
    }

    #[tokio::test]
    async fn slow_feedback_never_costs_a_command_its_admission() -> anyhow::Result<()> {
        use carillon_discord_module::contract::HostReply;
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
            (Duration::ZERO, Duration::ZERO, Duration::from_secs(30)),
        );
        let (calls, _reactions, server) = submitting(router, |config| config.typing = true).await?;
        let started = std::time::Instant::now();
        let reply = calls.answer(&submit_request()?, calls_until()).await;
        let elapsed = started.elapsed();
        server.abort();
        assert_eq!(reply, HostReply::Submitted);
        assert!(
            elapsed < crate::delivery::PROMPT_FEEDBACK + Duration::from_secs(1),
            "admission waited {elapsed:?} for feedback"
        );
        assert_eq!(submitted.lock().await.len(), 1);
        // The marker, then the feedback that was asked for, with admission
        // beside it.
        let trace = trace.lock().await;
        assert!(trace[0].starts_with("message <@200> asked to run"));
        let after = &trace[1..4];
        assert!(after.iter().any(|request| request.contains("/reactions/")));
        assert!(after.iter().any(|request| request.ends_with("/typing")));
        assert!(after.iter().any(|request| request == "event 901"));
        Ok(())
    }

    #[tokio::test]
    async fn a_command_core_does_not_answer_in_time_says_so_within_the_module_deadline()
    -> anyhow::Result<()> {
        use carillon_discord_module::contract::HostReply;
        let trace = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let router = submission_server(
            carillon_protocol::model::CommandCatalog {
                commands: Vec::new(),
            },
            trace.clone(),
            Arc::new(tokio::sync::Mutex::new(Vec::new())),
            (Duration::ZERO, Duration::from_secs(30), Duration::ZERO),
        );
        let (calls, reactions, server) = submitting(router, |config| config.typing = false).await?;
        let started = std::time::Instant::now();
        let reply = calls.answer(&submit_request()?, calls_until()).await;
        let elapsed = started.elapsed();
        // Core may have accepted it: the answer says so, in time to be shown.
        assert_eq!(
            reply,
            HostReply::Error(
                "Carillon did not confirm that it accepted it; if it did, the answer follows in this conversation."
                    .to_owned()
            )
        );
        assert!(elapsed < super::DEADLINE, "answered after {elapsed:?}");
        assert_eq!(reactions.stop(Duration::from_secs(10)).await, 0);
        server.abort();
        // Core received it, and its conversation's lane goes on settling it:
        // the marker keeps its processing reaction, with no outcome yet.
        let trace = trace.lock().await;
        assert!(trace.contains(&"event 901".to_owned()), "{trace:?}");
        assert_eq!(
            trace
                .iter()
                .filter(|request| request.contains("/reactions/"))
                .collect::<Vec<_>>(),
            ["PUT /api/v10/channels/400/messages/901/reactions/%F0%9F%91%80/@me"]
        );
        Ok(())
    }

    /// A module that answers every interaction as a Run click does: it asks
    /// for the runtime's commands, then submits `/compact keep decisions`,
    /// then replies `ran`.
    fn running_module() -> anyhow::Result<Vec<u8>> {
        use carillon_discord_module::contract::{HostRequest, Invocation};
        let describe = describing(&[definition("command")]);
        let describe_length = serde_json::to_vec(&Invocation::Describe)?.len();
        let commands = serde_json::to_string(&HostRequest::Commands)?;
        let submit = String::from_utf8(submit_request()?)?;
        let reply = replying("ran", &[]);
        let card = showing("A test card");
        let text = format!(
            r#"(module
  (import "carillon" "call" (func $call (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (global $length (mut i32) (i32.const 0))
  (global $output (mut i32) (i32.const 0))
  (data (i32.const 1024) "{}")
  (data (i32.const 2048) "{}")
  (data (i32.const 3072) "{}")
  (data (i32.const 4096) "{}")
  (data (i32.const 6144) "{}")
  (func (export "input") (param $length i32) (result i32)
    (global.set $length (local.get $length))
    (i32.const 8192))
  (func (export "invoke") (result i32)
    (if (result i32) (i32.eq (global.get $length) (i32.const {describe_length}))
      (then (global.set $output (i32.const 1024)) (i32.const {}))
      (else
        ;; A card request begins {{"Card"
        (if (result i32) (i32.eq (i32.load8_u (i32.const 8194)) (i32.const 67))
          (then (global.set $output (i32.const 6144)) (i32.const {}))
          (else
            (drop (call $call (i32.const 2048) (i32.const {})))
            (drop (call $call (i32.const 3072) (i32.const {})))
            (global.set $output (i32.const 4096))
            (i32.const {}))))))
  (func (export "output") (result i32) (global.get $output)))"#,
            escape(&describe),
            escape(&commands),
            escape(&submit),
            escape(&reply),
            escape(&card),
            describe.len(),
            card.len(),
            commands.len(),
            submit.len(),
            reply.len(),
        );
        Ok(wat::parse_str(text)?)
    }

    #[tokio::test]
    async fn a_run_click_answers_within_the_module_deadline_however_slow_its_calls()
    -> anyhow::Result<()> {
        // The runtime takes five seconds to list its commands, then core
        // does not answer the submission at all: separately bounded, the two
        // would outlast the module's deadline.
        let trace = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let router = submission_server(
            carillon_protocol::model::CommandCatalog {
                commands: vec![carillon_protocol::model::RuntimeCommand {
                    name: "compact".to_owned(),
                    description: None,
                    hint: None,
                }],
            },
            trace.clone(),
            Arc::new(tokio::sync::Mutex::new(Vec::new())),
            (
                Duration::from_secs(5),
                Duration::from_secs(30),
                Duration::ZERO,
            ),
        );
        let (calls, reactions, server) = submitting(router, |config| config.typing = false).await?;
        let directory = tempfile::tempdir()?;
        std::fs::write(directory.path().join("release.wasm"), running_module()?)?;
        let commands = Module::load(files(directory.path()), Source::Bundled).await?;
        let started = std::time::Instant::now();
        let reply = commands.invoke(calls, interaction()).await?;
        let elapsed = started.elapsed();
        // The module answered in time, after both calls had ended.
        assert_eq!(reply.content, "ran");
        assert!(elapsed < super::DEADLINE, "answered after {elapsed:?}");
        // Core received the command; its silence is left to the
        // conversation's lane to settle, so no outcome is shown yet.
        assert_eq!(reactions.stop(Duration::from_secs(10)).await, 0);
        server.abort();
        let trace = trace.lock().await;
        assert!(
            trace[0].starts_with("message <@200> asked to run `/compact keep decisions`"),
            "{trace:?}"
        );
        assert!(trace.contains(&"event 901".to_owned()), "{trace:?}");
        assert!(
            !trace
                .iter()
                .any(|request| request.contains("/reactions/%E2%9A%A0")),
            "{trace:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_run_click_hands_the_command_to_core_however_slow_its_feedback() -> anyhow::Result<()>
    {
        // The runtime lists its commands just inside its own limit, leaving
        // less than the feedback's wait; Discord then takes far longer to
        // show the processing reaction and typing, while core answers at once.
        let trace = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let submitted = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let router = submission_server(
            carillon_protocol::model::CommandCatalog {
                commands: vec![carillon_protocol::model::RuntimeCommand {
                    name: "compact".to_owned(),
                    description: None,
                    hint: None,
                }],
            },
            trace.clone(),
            submitted.clone(),
            (
                Duration::from_millis(5_500),
                Duration::ZERO,
                Duration::from_secs(30),
            ),
        );
        let (calls, reactions, server) = submitting(router, |config| config.typing = true).await?;
        let directory = tempfile::tempdir()?;
        std::fs::write(directory.path().join("release.wasm"), running_module()?)?;
        let commands = Module::load(files(directory.path()), Source::Bundled).await?;
        let started = std::time::Instant::now();
        let reply = commands.invoke(calls, interaction()).await?;
        let elapsed = started.elapsed();
        assert_eq!(reply.content, "ran");
        assert!(elapsed < super::DEADLINE, "answered after {elapsed:?}");
        // The optional feedback took none of core's time: it has the command,
        // once, and the processing request it waits behind is still owned.
        assert_eq!(submitted.lock().await.len(), 1);
        assert_eq!(reactions.stop(Duration::from_millis(100)).await, 1);
        server.abort();
        let trace = trace.lock().await;
        assert!(
            trace[0].starts_with("message <@200> asked to run `/compact keep decisions`"),
            "{trace:?}"
        );
        assert!(
            trace.iter().any(|request| request.contains("/reactions/")),
            "{trace:?}"
        );
        Ok(())
    }

    /// A conversation in channel 300 whose message 400 is still being
    /// prepared, core taking `status` to say whether it holds it. Beside it
    /// [`preparing`] returns calls of a person in that conversation, which
    /// take their place in the same lanes. `accepted` records the events
    /// core accepts, `markers` the marker messages posted.
    struct Preparing {
        ingress: Arc<crate::inbound::Ingress>,
        send: tokio::sync::mpsc::Sender<crate::inbound::lanes::Queued>,
        dispatcher: tokio::task::JoinHandle<()>,
        accepted: Arc<tokio::sync::Mutex<Vec<String>>>,
        markers: Arc<AtomicUsize>,
        server: tokio::task::JoinHandle<std::io::Result<()>>,
        _directory: tempfile::TempDir,
    }

    impl Preparing {
        /// Closes the lanes and waits, at most ten seconds, for the
        /// dispatcher to settle what it holds; the calls must be gone, as
        /// they hold the lanes too. Returns the events core accepted.
        async fn settled(self) -> anyhow::Result<Vec<String>> {
            drop(self.send);
            tokio::time::timeout(Duration::from_secs(10), self.dispatcher)
                .await
                .map_err(|_| anyhow::anyhow!("the lanes did not settle"))??;
            self.server.abort();
            Ok(self.accepted.lock().await.clone())
        }

        /// Stops the provider in the order its shutdown does: the lanes
        /// settle what they hold, at most ten seconds, then the reactions
        /// get `grace`. Returns the events core accepted and the reactions
        /// the stop cancelled.
        async fn stopped(self, grace: Duration) -> anyhow::Result<(Vec<String>, usize)> {
            self.ingress.stopping.cancel();
            drop(self.send);
            tokio::time::timeout(Duration::from_secs(10), self.dispatcher)
                .await
                .map_err(|_| anyhow::anyhow!("the lanes did not settle"))??;
            let cancelled = self.ingress.reactions.stop(grace).await;
            self.server.abort();
            Ok((self.accepted.lock().await.clone(), cancelled))
        }
    }

    /// Core and Discord for [`preparing`]: core is slow by `status` to say
    /// whether it holds message 400, and records what it accepts and the
    /// marker messages posted.
    fn preparing_server(
        status: Duration,
        asked: Arc<tokio::sync::Notify>,
        accepted: Arc<tokio::sync::Mutex<Vec<String>>>,
        markers: Arc<AtomicUsize>,
    ) -> Router {
        use axum::{
            Json,
            extract::Path as Segment,
            routing::{get, post},
        };
        use carillon_protocol::model::{EventReceipt, EventStatus, ExternalEvent, TurnState};
        Router::new()
            .route(
                "/v1/events/{event_id}",
                get(move |Segment(event_id): Segment<String>| async move {
                    if event_id == "400" {
                        asked.notify_one();
                        tokio::time::sleep(status).await;
                    }
                    Json(EventStatus::Unknown)
                }),
            )
            .route(
                "/v1/events",
                post(move |Json(event): Json<ExternalEvent>| {
                    let accepted = accepted.clone();
                    async move {
                        accepted.lock().await.push(event.event_id.clone());
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
                "/v1/commands",
                get(|| async { Json(json!({"commands": [{"name": "compact"}]})) }),
            )
            .merge(marking(markers))
            .fallback(|| async { StatusCode::NO_CONTENT })
    }

    /// Discord posting every marker in channel 300 as `901`, counting them.
    fn marking(markers: Arc<AtomicUsize>) -> Router {
        use axum::{Json, routing::post};
        Router::new().route(
            "/api/v10/channels/{channel}/messages",
            post(move || {
                markers.fetch_add(1, Ordering::SeqCst);
                async {
                    Json(json!({
                        "id": "901",
                        "channel_id": "300",
                        "author": {"id": "201", "username": "carillon", "discriminator": "0"},
                        "content": "marker",
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
    }

    /// Core whose acceptance of command 901 takes `admission`, in work of
    /// its own that goes on whatever its caller does, as core's storage work
    /// does: a request for an event already being accepted waits for that
    /// and is answered as its duplicate, as core's transaction answers.
    /// Other events are accepted at once. `accepted` records the events in
    /// the order core commits them, and `posts` counts the submissions.
    fn admitting_server(
        admission: Duration,
        accepted: Arc<tokio::sync::Mutex<Vec<String>>>,
        markers: Arc<AtomicUsize>,
        posts: Arc<AtomicUsize>,
    ) -> Router {
        use axum::{
            Json,
            extract::Path as Segment,
            routing::{get, post},
        };
        use carillon_protocol::model::{EventReceipt, EventStatus, ExternalEvent, TurnState};
        use std::collections::HashMap;
        let jobs = Arc::new(std::sync::Mutex::new(HashMap::<
            String,
            tokio::sync::watch::Receiver<bool>,
        >::new()));
        let committed = accepted.clone();
        Router::new()
            .route(
                "/v1/events/{event_id}",
                get(move |Segment(event_id): Segment<String>| {
                    let committed = committed.clone();
                    async move {
                        if committed.lock().await.contains(&event_id) {
                            Json(EventStatus::Accepted(EventReceipt {
                                event_id,
                                turn_id: "turn".to_owned(),
                                duplicate: false,
                                state: TurnState::Queued,
                            }))
                        } else {
                            Json(EventStatus::Unknown)
                        }
                    }
                }),
            )
            .route(
                "/v1/events",
                post(move |Json(event): Json<ExternalEvent>| {
                    posts.fetch_add(1, Ordering::SeqCst);
                    let (mut done, first) = {
                        let mut jobs = jobs
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if let Some(job) = jobs.get(&event.event_id) {
                            (job.clone(), false)
                        } else {
                            let (commit, done) = tokio::sync::watch::channel(false);
                            jobs.insert(event.event_id.clone(), done.clone());
                            let (accepted, id) = (accepted.clone(), event.event_id.clone());
                            let delay = if id == "901" {
                                admission
                            } else {
                                Duration::ZERO
                            };
                            tokio::spawn(async move {
                                tokio::time::sleep(delay).await;
                                accepted.lock().await.push(id);
                                let _ = commit.send(true);
                            });
                            (done, true)
                        }
                    };
                    async move {
                        let _ = done.wait_for(|committed| *committed).await;
                        Json(EventReceipt {
                            event_id: event.event_id,
                            turn_id: "turn".to_owned(),
                            duplicate: !first,
                            state: TurnState::Queued,
                        })
                    }
                }),
            )
            .merge(marking(markers))
            .fallback(|| async { StatusCode::NO_CONTENT })
    }

    async fn preparing(status: Duration) -> anyhow::Result<(super::InteractionCalls, Preparing)> {
        let (accepted, markers) = (
            Arc::new(tokio::sync::Mutex::new(Vec::new())),
            Arc::new(AtomicUsize::new(0)),
        );
        let asked = Arc::new(tokio::sync::Notify::new());
        let router = preparing_server(status, asked.clone(), accepted.clone(), markers.clone());
        let (calls, preparing) = conversation(router, accepted, markers).await?;
        // Message 400 arrives first, through the gateway's lanes.
        preparing
            .send
            .send(crate::inbound::tests::hello(&preparing.ingress).into())
            .await?;
        tokio::time::timeout(Duration::from_secs(10), asked.notified())
            .await
            .context("message 400 was never prepared")?;
        Ok((calls, preparing))
    }

    /// Conversation 300's lanes, served by `router` as core and Discord, and
    /// calls of a person in that conversation, which take their place in the
    /// same lanes.
    async fn conversation(
        router: Router,
        accepted: Arc<tokio::sync::Mutex<Vec<String>>>,
        markers: Arc<AtomicUsize>,
    ) -> anyhow::Result<(super::InteractionCalls, Preparing)> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move { axum::serve(listener, router).await });
        let mut config = crate::conversations::tests::discord_config(
            crate::config::DiscordActivation::Always,
            crate::config::DiscordConversation::Channel,
        );
        config.typing = false;
        let http = Arc::new(
            twilight_http::Client::builder()
                .token("test-only".to_owned())
                .proxy(address.to_string(), true)
                .ratelimiter(None)
                .build(),
        );
        let core = crate::client::test_client(format!("http://{address}"))?;
        let directory = tempfile::tempdir()?;
        let participation = Arc::new(crate::conversations::Participation::load(
            directory.path().join("participation.json"),
        )?);
        let ingress = crate::inbound::tests::ingress(
            core.clone(),
            config,
            participation,
            http.clone(),
            directory.path(),
        )?;
        let (send, receive) = tokio::sync::mpsc::channel(4);
        let dispatcher = tokio::spawn(crate::inbound::lanes::dispatch(ingress.clone(), receive));
        let calls = super::InteractionCalls::new(
            core,
            carillon_protocol::model::ModelQuery {
                agent_id: "default".to_owned(),
                audience_id: Some("discord-100-300".to_owned()),
            },
        )
        .with_submissions(super::Submissions {
            http,
            reactions: ingress.reactions.clone(),
            lanes: crate::inbound::lanes::Lanes::new(send.clone()),
            origin: crate::conversations::InteractionOrigin {
                guild_id: "100".to_owned(),
                user_id: "200".to_owned(),
                channel_id: "300".to_owned(),
                route_channel_id: "300".to_owned(),
            },
            settings: ingress.reactions.settings().current(),
        });
        let preparing = Preparing {
            ingress,
            send,
            dispatcher,
            accepted,
            markers,
            server,
            _directory: directory,
        };
        Ok((calls, preparing))
    }

    #[tokio::test]
    async fn a_command_is_accepted_after_the_messages_before_it_in_its_conversation()
    -> anyhow::Result<()> {
        // Core takes two seconds to say whether it holds message 400 when a
        // Run click comes in the same conversation.
        let (calls, preparing) = preparing(Duration::from_secs(2)).await?;
        let directory = tempfile::tempdir()?;
        std::fs::write(directory.path().join("release.wasm"), running_module()?)?;
        let commands = Module::load(files(directory.path()), Source::Bundled).await?;
        assert_eq!(commands.invoke(calls, interaction()).await?.content, "ran");
        // Core accepted the message before the command that followed it.
        assert_eq!(preparing.settled().await?, ["400", "901"]);
        Ok(())
    }

    #[tokio::test]
    async fn a_command_still_waiting_at_its_deadline_is_not_sent_and_its_conversation_goes_on()
    -> anyhow::Result<()> {
        use carillon_discord_module::contract::HostReply;
        let (calls, preparing) = preparing(Duration::from_secs(3)).await?;
        let started = std::time::Instant::now();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        let reply = calls.answer(&submit_request()?, deadline).await;
        drop(calls);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(
            reply,
            HostReply::Error("Earlier messages in this conversation are still being prepared, so it was not sent; please run it again once they are answered.".to_owned())
        );
        let markers = preparing.markers.clone();
        // The message was accepted; nothing of the command was posted or sent.
        assert_eq!(preparing.settled().await?, ["400"]);
        assert_eq!(markers.load(Ordering::SeqCst), 0);
        Ok(())
    }

    /// Core accepting every event at once, and Discord that records each
    /// marker it is asked to post, counts it, and never answers.
    fn unanswered_marker_server(
        accepted: Arc<tokio::sync::Mutex<Vec<String>>>,
        markers: Arc<AtomicUsize>,
        posted: Arc<tokio::sync::Mutex<Vec<String>>>,
    ) -> Router {
        use axum::{
            Json,
            routing::{get, post},
        };
        use carillon_protocol::model::{EventReceipt, EventStatus, ExternalEvent, TurnState};
        Router::new()
            .route(
                "/v1/events/{event_id}",
                get(|| async { Json(EventStatus::Unknown) }),
            )
            .route(
                "/v1/events",
                post(move |Json(event): Json<ExternalEvent>| {
                    let accepted = accepted.clone();
                    async move {
                        accepted.lock().await.push(event.event_id.clone());
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
                    let (markers, posted) = (markers.clone(), posted.clone());
                    async move {
                        markers.fetch_add(1, Ordering::SeqCst);
                        posted
                            .lock()
                            .await
                            .push(body["content"].as_str().unwrap_or_default().to_owned());
                        std::future::pending::<StatusCode>().await
                    }
                }),
            )
            .fallback(|| async { StatusCode::NO_CONTENT })
    }

    #[tokio::test]
    async fn a_command_whose_marker_discord_does_not_confirm_is_not_sent_and_its_conversation_goes_on()
    -> anyhow::Result<()> {
        use carillon_discord_module::contract::HostReply;
        let (accepted, markers, posted) = (
            Arc::new(tokio::sync::Mutex::new(Vec::new())),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(tokio::sync::Mutex::new(Vec::new())),
        );
        let router = unanswered_marker_server(accepted.clone(), markers.clone(), posted.clone());
        let (calls, preparing) = conversation(router, accepted.clone(), markers).await?;
        let started = std::time::Instant::now();
        let submit = submit_request()?;
        let answering = tokio::spawn(async move { calls.answer(&submit, calls_until()).await });
        // The person's next message arrives while Discord holds the marker's
        // answer: it waits behind the command.
        tokio::time::timeout(Duration::from_secs(5), async {
            while preparing.markers.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .context("the marker was never posted")?;
        preparing
            .send
            .send(crate::inbound::tests::hello(&preparing.ingress).into())
            .await?;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(accepted.lock().await.is_empty());
        let reply = answering.await?;
        let elapsed = started.elapsed();
        // Discord may have posted the marker, but without its answer the
        // command has no identity: nothing of it reaches core, and the reply
        // claims no more than that.
        assert_eq!(
            reply,
            HostReply::Error(
                "Discord did not confirm the marker message, so the command was not sent to Carillon."
                    .to_owned()
            )
        );
        assert!(
            elapsed >= super::MARKER_LIMIT && elapsed < super::DEADLINE,
            "answered after {elapsed:?}"
        );
        // The marker asks; it never says the command ran.
        assert_eq!(
            *posted.lock().await,
            ["<@200> asked to run `/compact keep decisions`"]
        );
        // The conversation goes on with the message, and only the message.
        assert_eq!(preparing.settled().await?, ["400"]);
        Ok(())
    }

    #[tokio::test]
    async fn a_command_waiting_when_the_provider_stops_is_not_sent() -> anyhow::Result<()> {
        use carillon_discord_module::contract::HostReply;
        let (calls, preparing) = preparing(Duration::from_secs(30)).await?;
        let submit = submit_request()?;
        let waiting = tokio::spawn(async move { calls.answer(&submit, calls_until()).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        preparing.ingress.stopping.cancel();
        let reply = tokio::time::timeout(Duration::from_secs(2), waiting).await??;
        assert_eq!(
            reply,
            HostReply::Error("Carillon is too busy or restarting, so it was not sent; please run it again shortly.".to_owned())
        );
        let markers = preparing.markers.clone();
        assert!(preparing.settled().await?.is_empty());
        assert_eq!(markers.load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[tokio::test]
    async fn listing_commands_never_waits_for_the_conversation() -> anyhow::Result<()> {
        use carillon_discord_module::contract::{HostReply, HostRequest};
        // The conversation's message is being prepared for half a minute.
        let (calls, preparing) = preparing(Duration::from_secs(30)).await?;
        let started = std::time::Instant::now();
        let reply = calls
            .answer(&serde_json::to_vec(&HostRequest::Commands)?, calls_until())
            .await;
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(matches!(reply, HostReply::Commands(catalog) if catalog.commands.len() == 1));
        preparing.server.abort();
        Ok(())
    }

    /// Calls of the conversation of [`admitting_server`], whose core takes
    /// three seconds to accept command 901, the reply to its submission,
    /// due one second in, and the count of core's submissions.
    async fn admitting() -> anyhow::Result<(
        Preparing,
        Arc<tokio::sync::Mutex<Vec<String>>>,
        Arc<AtomicUsize>,
    )> {
        let posts = Arc::new(AtomicUsize::new(0));
        let counted = posts.clone();
        let (preparing, accepted) = unconfirmed(|accepted, markers| {
            admitting_server(Duration::from_secs(3), accepted, markers, counted)
        })
        .await?;
        Ok((preparing, accepted, posts))
    }

    /// Calls of the conversation served by `serve` as core and Discord, and
    /// the reply to a submission of command 901, due one second in, before
    /// core has answered for it.
    async fn unconfirmed(
        serve: impl FnOnce(Arc<tokio::sync::Mutex<Vec<String>>>, Arc<AtomicUsize>) -> Router,
    ) -> anyhow::Result<(Preparing, Arc<tokio::sync::Mutex<Vec<String>>>)> {
        use carillon_discord_module::contract::HostReply;
        let (accepted, markers) = (
            Arc::new(tokio::sync::Mutex::new(Vec::new())),
            Arc::new(AtomicUsize::new(0)),
        );
        let router = serve(accepted.clone(), markers.clone());
        let (calls, preparing) = conversation(router, accepted.clone(), markers).await?;
        let started = std::time::Instant::now();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        let reply = calls.answer(&submit_request()?, deadline).await;
        // The reply is due before core has answered: it says core may have
        // accepted the command, in time.
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(
            reply,
            HostReply::Error("Carillon did not confirm that it accepted it; if it did, the answer follows in this conversation.".to_owned())
        );
        assert_eq!(preparing.markers.load(Ordering::SeqCst), 1);
        Ok((preparing, accepted))
    }

    #[tokio::test]
    async fn a_command_core_is_still_accepting_keeps_its_conversation_until_core_answers()
    -> anyhow::Result<()> {
        let (preparing, accepted, _) = admitting().await?;
        // The person's next message arrives while core is still accepting
        // the command.
        let mut next = crate::inbound::tests::hello(&preparing.ingress);
        next.request.event_id = "402".to_owned();
        preparing.send.send(next.into()).await?;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(accepted.lock().await.is_empty());
        // Core accepts the command before the message that followed it.
        assert_eq!(preparing.settled().await?, ["901", "402"]);
        Ok(())
    }

    #[tokio::test]
    async fn a_stopping_provider_waits_for_a_command_core_is_still_accepting() -> anyhow::Result<()>
    {
        let (preparing, accepted, posts) = admitting().await?;
        // The lane asks core again at once, and the provider stops while
        // that attempt waits for core to accept the command.
        tokio::time::timeout(Duration::from_secs(2), async {
            while posts.load(Ordering::SeqCst) < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        preparing.ingress.stopping.cancel();
        // The lanes settle only once core has answered that attempt, and no
        // other is made.
        let started = std::time::Instant::now();
        assert_eq!(preparing.settled().await?, ["901"]);
        assert!(started.elapsed() >= Duration::from_secs(1));
        assert_eq!(*accepted.lock().await, ["901"]);
        assert_eq!(posts.load(Ordering::SeqCst), 2);
        Ok(())
    }

    /// Core that cannot take command 901, answering every attempt as
    /// unavailable after `answer`, and accepts other events at once, beside
    /// a Discord that answers at once. `trace` records core's requests and
    /// Discord's reactions.
    fn unavailable_server(
        trace: Arc<tokio::sync::Mutex<Vec<String>>>,
        accepted: Arc<tokio::sync::Mutex<Vec<String>>>,
        markers: Arc<AtomicUsize>,
        answer: Duration,
    ) -> Router {
        use axum::{
            Json,
            extract::Path as Segment,
            http::{Method, Uri},
            response::IntoResponse,
            routing::{get, post},
        };
        use carillon_protocol::model::{EventReceipt, EventStatus, ExternalEvent, TurnState};
        let (asked, attempted, reacted) = (trace.clone(), trace.clone(), trace);
        Router::new()
            .route(
                "/v1/events/{event_id}",
                get(move |Segment(event_id): Segment<String>| async move {
                    asked.lock().await.push(format!("status {event_id}"));
                    Json(EventStatus::Unknown)
                }),
            )
            .route(
                "/v1/events",
                post(move |Json(event): Json<ExternalEvent>| async move {
                    attempted
                        .lock()
                        .await
                        .push(format!("event {}", event.event_id));
                    if event.event_id == "901" {
                        tokio::time::sleep(answer).await;
                        return StatusCode::SERVICE_UNAVAILABLE.into_response();
                    }
                    accepted.lock().await.push(event.event_id.clone());
                    Json(EventReceipt {
                        event_id: event.event_id,
                        turn_id: "turn".to_owned(),
                        duplicate: false,
                        state: TurnState::Queued,
                    })
                    .into_response()
                }),
            )
            .merge(marking(markers))
            .fallback(move |method: Method, uri: Uri| async move {
                reacted
                    .lock()
                    .await
                    .push(format!("{method} {}", uri.path()));
                StatusCode::NO_CONTENT
            })
    }

    #[tokio::test]
    async fn a_stopping_provider_leaves_a_command_core_cannot_confirm_unconfirmed()
    -> anyhow::Result<()> {
        let trace = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let served = trace.clone();
        let (preparing, _) = unconfirmed(|accepted, markers| {
            unavailable_server(served, accepted, markers, Duration::ZERO)
        })
        .await?;
        let attempts = async || {
            trace
                .lock()
                .await
                .iter()
                .filter(|request| *request == "event 901")
                .count()
        };
        // The person's next message waits while the lane goes on asking
        // core, which still cannot take the command.
        let mut next = crate::inbound::tests::hello(&preparing.ingress);
        next.request.event_id = "402".to_owned();
        preparing.send.send(next.into()).await?;
        let before = attempts().await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while attempts().await <= before + 1 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .context("the lane stopped asking core")?;
        // The provider stops: the lanes settle without core's answer, and
        // the message waiting behind the command is dropped unprepared.
        let started = std::time::Instant::now();
        let (accepted, cancelled) = preparing.stopped(crate::delivery::PROMPT_FEEDBACK).await?;
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(accepted.is_empty());
        assert_eq!(cancelled, 0);
        let trace = trace.lock().await;
        assert!(
            !trace.iter().any(|request| request.ends_with("402")),
            "{trace:?}"
        );
        // The command's marker says it is unconfirmed, not refused.
        let marker = "/api/v10/channels/300/messages/901/reactions";
        assert_eq!(
            trace
                .iter()
                .filter(|request| request.contains("/reactions/"))
                .collect::<Vec<_>>(),
            [
                &format!("PUT {marker}/%F0%9F%91%80/@me"),
                &format!("DELETE {marker}/%F0%9F%91%80/@me"),
                &format!("PUT {marker}/%E2%9A%A0%EF%B8%8F/@me"),
            ]
        );
        Ok(())
    }

    /// A Run click's command in conversation 300 while core cannot take it,
    /// core taking `answer` to say so for each attempt. Once core has
    /// received the command's first attempt, message 402 arrives behind it,
    /// and the provider stops `after` that. Returns the reply and, in order, core's
    /// requests and Discord's reactions.
    async fn stopped_while_submitting(
        answer: Duration,
        after: Duration,
    ) -> anyhow::Result<(carillon_discord_module::contract::HostReply, Vec<String>)> {
        let trace = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let (accepted, markers) = (
            Arc::new(tokio::sync::Mutex::new(Vec::new())),
            Arc::new(AtomicUsize::new(0)),
        );
        let router = unavailable_server(trace.clone(), accepted.clone(), markers.clone(), answer);
        let (calls, preparing) = conversation(router, accepted, markers).await?;
        let submit = submit_request()?;
        let reply = tokio::spawn(async move { calls.answer(&submit, calls_until()).await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !trace.lock().await.contains(&"event 901".to_owned()) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        // The command holds its conversation's turn; the message waits.
        let mut next = crate::inbound::tests::hello(&preparing.ingress);
        next.request.event_id = "402".to_owned();
        preparing.send.send(next.into()).await?;
        tokio::time::sleep(after).await;
        let started = std::time::Instant::now();
        let (accepted, cancelled) = preparing.stopped(crate::delivery::PROMPT_FEEDBACK).await?;
        assert!(
            started.elapsed() < answer + Duration::from_secs(1),
            "the stop took {:?}",
            started.elapsed()
        );
        assert!(accepted.is_empty());
        assert_eq!(cancelled, 0);
        let reply = tokio::time::timeout(Duration::from_secs(2), reply).await??;
        let trace = trace.lock().await.clone();
        Ok((reply, trace))
    }

    /// What a command core never confirmed shows once the provider stopped:
    /// the reply says it may have been accepted, core got one attempt, the
    /// message behind it was never prepared, and the marker shows the
    /// uncertain reaction.
    fn assert_unconfirmed(reply: &carillon_discord_module::contract::HostReply, trace: &[String]) {
        use carillon_discord_module::contract::HostReply;
        assert_eq!(
            *reply,
            HostReply::Error("Carillon did not confirm that it accepted it; if it did, the answer follows in this conversation.".to_owned())
        );
        assert_eq!(
            trace
                .iter()
                .filter(|request| *request == "event 901")
                .count(),
            1,
            "{trace:?}"
        );
        assert!(
            !trace.iter().any(|request| request.ends_with("402")),
            "{trace:?}"
        );
        let marker = "/api/v10/channels/300/messages/901/reactions";
        assert_eq!(
            trace
                .iter()
                .filter(|request| request.contains("/reactions/"))
                .collect::<Vec<_>>(),
            [
                &format!("PUT {marker}/%F0%9F%91%80/@me"),
                &format!("DELETE {marker}/%F0%9F%91%80/@me"),
                &format!("PUT {marker}/%E2%9A%A0%EF%B8%8F/@me"),
            ]
        );
    }

    #[tokio::test]
    async fn a_command_stopped_while_waiting_to_try_again_makes_no_other_attempt()
    -> anyhow::Result<()> {
        // Core says at once that it cannot take the command; the provider
        // stops while the submission waits to try again.
        let (reply, trace) =
            stopped_while_submitting(Duration::ZERO, Duration::from_millis(100)).await?;
        assert_unconfirmed(&reply, &trace);
        Ok(())
    }

    #[tokio::test]
    async fn a_command_stopped_while_core_answers_its_attempt_makes_no_other() -> anyhow::Result<()>
    {
        // The provider stops while core takes half a second to say it
        // cannot take the command: that attempt ends, and none follows.
        let (reply, trace) =
            stopped_while_submitting(Duration::from_millis(500), Duration::from_millis(100))
                .await?;
        assert_unconfirmed(&reply, &trace);
        Ok(())
    }

    /// Core that does not answer the first submission of command 901 and
    /// refuses it when asked again, accepting other events at once, beside
    /// a Discord that holds every reaction for a minute.
    fn refusing_server(
        accepted: Arc<tokio::sync::Mutex<Vec<String>>>,
        markers: Arc<AtomicUsize>,
    ) -> Router {
        use axum::{
            Json,
            extract::Path as Segment,
            http::Uri,
            response::IntoResponse,
            routing::{get, post},
        };
        use carillon_protocol::model::{EventReceipt, EventStatus, ExternalEvent, TurnState};
        let attempts = Arc::new(AtomicUsize::new(0));
        Router::new()
            .route(
                "/v1/events/{event_id}",
                get(|Segment(_): Segment<String>| async { Json(EventStatus::Unknown) }),
            )
            .route(
                "/v1/events",
                post(move |Json(event): Json<ExternalEvent>| async move {
                    if event.event_id == "901" {
                        if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                            tokio::time::sleep(Duration::from_secs(30)).await;
                        }
                        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
                    }
                    accepted.lock().await.push(event.event_id.clone());
                    Json(EventReceipt {
                        event_id: event.event_id,
                        turn_id: "turn".to_owned(),
                        duplicate: false,
                        state: TurnState::Queued,
                    })
                    .into_response()
                }),
            )
            .merge(marking(markers))
            .fallback(|uri: Uri| async move {
                if uri.path().contains("/reactions/") {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                }
                StatusCode::NO_CONTENT
            })
    }

    #[tokio::test]
    async fn a_refused_command_lets_its_conversation_go_on_beside_its_feedback()
    -> anyhow::Result<()> {
        let (preparing, accepted) = unconfirmed(refusing_server).await?;
        // Core refuses the command when the lane asks again, while Discord
        // still holds its processing reaction, which its failure waits for.
        let mut next = crate::inbound::tests::hello(&preparing.ingress);
        next.request.event_id = "402".to_owned();
        preparing.send.send(next.into()).await?;
        // The person's next message is accepted within its own wait for
        // feedback, not after the command's.
        tokio::time::timeout(Duration::from_secs(5), async {
            while accepted.lock().await.is_empty() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .map_err(|_| anyhow::anyhow!("the conversation waited for the command's feedback"))?;
        // The reactions still own the command's failure and both processing
        // requests, and their stop ends them.
        let (accepted, cancelled) = preparing.stopped(Duration::from_millis(100)).await?;
        assert_eq!(accepted, ["402"]);
        assert_eq!(cancelled, 3);
        Ok(())
    }

    /// Core accepting every event, beside a Discord that posts the marker
    /// and applies the first processing reaction `delay` after receiving it.
    fn applying_server(delay: Duration, applied: Arc<tokio::sync::Mutex<Vec<String>>>) -> Router {
        use axum::{Json, routing::post};
        use carillon_protocol::model::{EventReceipt, ExternalEvent, TurnState};
        Router::new()
            .route(
                "/v1/events",
                post(|Json(event): Json<ExternalEvent>| async move {
                    Json(EventReceipt {
                        event_id: event.event_id,
                        turn_id: "turn".to_owned(),
                        duplicate: false,
                        state: TurnState::Queued,
                    })
                }),
            )
            .route(
                "/api/v10/channels/{channel}/messages",
                post(|Json(body): Json<Value>| async move {
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
                }),
            )
            .merge(crate::delivery::tests::late_processing(delay, applied))
            .fallback(|| async { StatusCode::NO_CONTENT })
    }

    #[tokio::test]
    async fn a_late_processing_reaction_never_lands_after_the_outcome() -> anyhow::Result<()> {
        use carillon_discord_module::contract::HostReply;
        // The coordinator's fault: Discord applies the processing reaction
        // four seconds after receiving it, after admission stopped waiting.
        let applied = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let router = applying_server(Duration::from_secs(4), applied.clone());
        let (calls, reactions, server) = submitting(router, |config| config.typing = true).await?;
        let started = std::time::Instant::now();
        assert_eq!(
            calls.answer(&submit_request()?, calls_until()).await,
            HostReply::Submitted
        );
        // Admission did not wait for Discord to apply it.
        assert!(started.elapsed() < crate::delivery::PROMPT_FEEDBACK + Duration::from_secs(1));
        // The answer is delivered at once and its outcome shown, as the
        // delivery loop does.
        reactions.show_outcome("400:901".to_owned(), super::Feedback::Delivered);
        assert_eq!(reactions.stop(Duration::from_secs(10)).await, 0);
        server.abort();
        let config = crate::conversations::tests::discord_config(
            crate::config::DiscordActivation::Always,
            crate::config::DiscordConversation::Thread,
        );
        assert_eq!(
            *applied.lock().await,
            [
                format!("PUT {}", config.processing_emoji),
                format!("DELETE {}", config.processing_emoji),
                format!("PUT {}", config.delivered_emoji),
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_command_shows_the_reactions_of_the_settings_it_was_admitted_with()
    -> anyhow::Result<()> {
        use carillon_discord_module::contract::HostReply;
        let applied = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let router = applying_server(Duration::ZERO, applied.clone());
        let (calls, reactions, server) = submitting(router, |config| config.typing = false).await?;
        let admitted = reactions.settings().current();
        // The operator serves other reactions after the interaction was
        // admitted, before the module submits its command.
        let mut other = crate::config::DiscordConfig::clone(&admitted);
        "⏳".clone_into(&mut other.processing_emoji);
        "📬".clone_into(&mut other.delivered_emoji);
        reactions
            .settings()
            .apply(|| Ok(other), |_| std::future::ready(Ok(unchanged())))
            .await
            .map_err(|refusal| anyhow::anyhow!("{refusal:?}"))?;
        assert_eq!(
            calls.answer(&submit_request()?, calls_until()).await,
            HostReply::Submitted
        );
        reactions.show_outcome("400:901".to_owned(), super::Feedback::Delivered);
        assert_eq!(reactions.stop(Duration::from_secs(10)).await, 0);
        server.abort();
        assert_eq!(
            *applied.lock().await,
            [
                format!("PUT {}", admitted.processing_emoji),
                format!("DELETE {}", admitted.processing_emoji),
                format!("PUT {}", admitted.delivered_emoji),
            ]
        );
        Ok(())
    }

    /// A module that answers the cards a module is checked with before it
    /// serves, whose origin turn is `turn`, with a valid card, and every other
    /// card by running `on_card`: WAT that leaves the output's length, with
    /// each of `data` at its address. Whatever it does, it installs.
    pub(crate) fn other_cards(on_card: &str, data: &[(u32, &str)]) -> anyhow::Result<Vec<u8>> {
        let describe = describing(&[definition("model")]);
        let describe_length =
            serde_json::to_vec(&carillon_discord_module::contract::Invocation::Describe)?.len();
        let card = showing("checked");
        // The first byte of the origin turn's ID in `{"Card":{"job":{"origin":{"turn_id":"`.
        let turn = 32768 + 37;
        let text = format!(
            r#"(module
  (import "carillon" "call" (func $call (param i32 i32) (result i32)))
  (import "carillon" "take" (func $take (param i32 i32)))
  (memory (export "memory") 40)
  (global $length (mut i32) (i32.const 0))
  (global $output (mut i32) (i32.const 0))
  (data (i32.const 1024) "{}")
  (data (i32.const 4096) "{}")
  {}
  (func (export "input") (param $length i32) (result i32)
    (global.set $length (local.get $length))
    (i32.const 32768))
  (func (export "invoke") (result i32)
    (local $reply i32)
    (if (result i32) (i32.eq (global.get $length) (i32.const {describe_length}))
      (then (global.set $output (i32.const 1024)) (i32.const {}))
      (else
        (if (result i32) (i32.eq (i32.load8_u (i32.const {turn})) (i32.const 116))
          (then (global.set $output (i32.const 4096)) (i32.const {}))
          (else {on_card})))))
  (func (export "output") (result i32) (global.get $output)))"#,
            escape(&describe),
            escape(&card),
            data.iter()
                .map(|(at, text)| format!("(data (i32.const {at}) \"{}\")", escape(text)))
                .collect::<Vec<_>>()
                .join(" "),
            describe.len(),
            card.len(),
        );
        Ok(wat::parse_str(text)?)
    }

    /// A job at work, whose origin turn is `job`.
    fn working_card(headline: &str) -> CardRequest {
        CardRequest {
            job: carillon_protocol::model::Job {
                origin: carillon_protocol::model::Origin {
                    turn_id: "job".to_owned(),
                    accepted_at: 1_790_000_000,
                },
                target_id: "300".to_owned(),
                turns: vec![carillon_protocol::model::JobTurn {
                    turn_id: "job".to_owned(),
                    state: carillon_protocol::model::TurnState::Running,
                    accepted_at: 1_790_000_000,
                    finished_at: None,
                    activity: Some(carillon_protocol::model::TurnActivity {
                        phase: carillon_protocol::model::ActivityPhase::Tools,
                        headline: Some(headline.to_owned()),
                        running: Vec::new(),
                        recent: Vec::new(),
                        tool_calls: 0,
                        work: None,
                        notes: Vec::new(),
                        agent: None,
                        partial: false,
                    }),
                    delivery: None,
                }],
                earlier: false,
            },
            seen: None,
            markers: carillon_discord_module::contract::ActivityMarkers::default(),
        }
    }

    /// The same job, settled: answered and its answer delivered.
    fn settled_card() -> CardRequest {
        let mut request = working_card("done");
        let turn = &mut request.job.turns[0];
        turn.state = carillon_protocol::model::TurnState::Completed;
        turn.finished_at = Some(1_790_000_045);
        turn.delivery = Some(carillon_protocol::model::DeliveryState::Delivered);
        request
    }

    /// What `module` makes of `request`, and how long it took.
    async fn drawn(
        module: &[u8],
        request: &CardRequest,
    ) -> anyhow::Result<(
        anyhow::Result<Option<twilight_model::channel::message::Embed>>,
        Duration,
    )> {
        let host = super::Host::new()?;
        let module = host.load(module)?;
        let started = std::time::Instant::now();
        let card = super::card(&host, &module, request).await;
        Ok((card, started.elapsed()))
    }

    #[tokio::test]
    async fn a_card_the_module_cannot_give_is_a_bounded_failure() -> anyhow::Result<()> {
        let request = working_card("Working");
        let answer = |text: &str| {
            format!(
                "(global.set $output (i32.const 16384)) (i32.const {})",
                text.len()
            )
        };
        let embed = |embed: Value| json!({"Card": {"Show": embed}}).to_string();
        let failing = [
            (
                "not JSON",
                "not json".to_owned(),
                "decode Discord module outcome: ",
            ),
            (
                "another answer",
                replying("hello", &[]),
                "Discord module answered a card with something else",
            ),
            (
                "a failure",
                r#"{"Failure":"no card"}"#.to_owned(),
                "Discord module failed: no card",
            ),
            (
                "not an embed",
                embed(json!({"description": "no type"})),
                "decode the card's embed: ",
            ),
            (
                "over Discord's limits",
                embed(json!({"type": "rich", "description": "x".repeat(4_097)})),
                "the card's embed is unusable: ",
            ),
            (
                "a removal while the job works",
                r#"{"Card":"Remove"}"#.to_owned(),
                "Discord module removed the card of a job that has not settled",
            ),
        ];
        for (case, output, refused) in failing {
            let module = other_cards(&answer(&output), &[(16384, &output)])?;
            let (card, _) = drawn(&module, &request).await?;
            let failure = card.err().with_context(|| format!("{case} was shown"))?;
            assert!(
                format!("{failure:#}").starts_with(refused),
                "{case}: {failure:#}"
            );
        }
        // The same removal of a settled job is the module's to decide.
        let remove = r#"{"Card":"Remove"}"#;
        let module = other_cards(&answer(remove), &[(16384, remove)])?;
        assert_eq!(drawn(&module, &settled_card()).await?.0?, None);

        for (case, on_card, refused) in [
            ("a trap", "(unreachable)", "guest failed: "),
            (
                "a guest that never ends",
                "(loop $forever (br $forever)) (i32.const 0)",
                "guest exceeded ",
            ),
            (
                "an output outside memory",
                "(global.set $output (i32.const 2621400)) (i32.const 4096)",
                "guest failed: guest named 4096 bytes at 2621400 outside its memory",
            ),
        ] {
            let (card, took) = drawn(&other_cards(on_card, &[])?, &request).await?;
            let failure = card.err().with_context(|| format!("{case} was shown"))?;
            assert!(
                format!("{failure:#}").starts_with(refused),
                "{case}: {failure:#}"
            );
            assert!(took < Duration::from_secs(3), "{case} took {took:?}");
        }

        // A request beyond what a module is given is not sent to it.
        let huge = working_card(&"x".repeat(super::CARD_REQUEST_LIMIT));
        let (card, took) = drawn(
            &other_cards("(loop $forever (br $forever)) (i32.const 0)", &[])?,
            &huge,
        )
        .await?;
        assert!(
            format!("{:#}", card.err().context("a huge card was shown")?).contains("more than the"),
        );
        assert!(took < Duration::from_millis(500), "{took:?}");
        Ok(())
    }

    #[tokio::test]
    async fn a_card_reaches_no_native_operation() -> anyhow::Result<()> {
        // The module asks to submit text, then shows whether it was refused:
        // `{"Error": …}` is refused, anything else would have been done.
        let submit = String::from_utf8(submit_request()?)?;
        let (refused, done) = (showing("refused"), showing("done"));
        let on_card = format!(
            "(local.set $reply (call $call (i32.const 16384) (i32.const {})))
             (call $take (i32.const 24576) (local.get $reply))
             (if (result i32) (i32.eq (i32.load8_u (i32.const 24578)) (i32.const 69))
               (then (global.set $output (i32.const 20480)) (i32.const {}))
               (else (global.set $output (i32.const 22528)) (i32.const {})))",
            submit.len(),
            refused.len(),
            done.len(),
        );
        let module = other_cards(
            &on_card,
            &[(16384, &submit), (20480, &refused), (22528, &done)],
        )?;
        let (card, _) = drawn(&module, &working_card("Working")).await?;
        assert_eq!(
            card?.and_then(|embed| embed.description).as_deref(),
            Some("refused")
        );
        Ok(())
    }

    /// The description a card shows, as the serving module draws it.
    async fn drawn_by(module: &Module) -> anyhow::Result<Option<String>> {
        Ok(module
            .render_card(&working_card("Working"))
            .await?
            .and_then(|embed| embed.description))
    }

    #[tokio::test]
    async fn a_module_that_cannot_draw_cards_does_not_serve() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let module = release_commands(root, &["model"], "release").await?;
        let describe = describing(&[definition("model")]);
        for card in [
            replying("not a card", &[]),
            r#"{"Failure":"no cards here"}"#.to_owned(),
            json!({"Card": {"Show": {"type": "rich", "description": "x".repeat(4_097)}}})
                .to_string(),
            // Removing the card of a job at work.
            r#"{"Card":"Remove"}"#.to_owned(),
        ] {
            let candidate = module_with_card(&describe, &replying("candidate", &[]), &card)?;
            std::fs::write(root.join("candidate.wasm"), &candidate)?;
            let declared = custom(root, "candidate.wasm");
            let refused = module.apply(declared.clone(), None).await;
            let Err(ChangeError::Unusable(error)) = refused else {
                anyhow::bail!("a module answering cards with {card} was applied");
            };
            assert!(
                format!("{error:#}").contains("render a card of"),
                "{error:#}"
            );
            assert_eq!(reply(&module).await?, "release");
            // Nor does it serve at startup.
            let unstarted = Module::load(files(root), declared)
                .await
                .err()
                .context("a module that cannot draw cards started")?;
            assert!(
                format!("{unstarted:#}").contains("render a card of"),
                "{unstarted:#}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn cards_are_drawn_by_the_module_serving_when_they_are_drawn() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let describe = describing(&[definition("model")]);
        std::fs::write(
            root.join("release.wasm"),
            module_with_card(
                &describe,
                &replying("release", &[]),
                &showing("release card"),
            )?,
        )?;
        let module = Module::load(files(root), Source::Bundled).await?;
        assert_eq!(drawn_by(&module).await?.as_deref(), Some("release card"));
        std::fs::write(
            root.join("custom.wasm"),
            module_with_card(&describe, &replying("custom", &[]), &showing("custom card"))?,
        )?;
        module.apply(custom(root, "custom.wasm"), None).await?;
        assert_eq!(drawn_by(&module).await?.as_deref(), Some("custom card"));
        // A start with the module declared draws them with it.
        let restarted = Module::load(files(root), custom(root, "custom.wasm")).await?;
        assert_eq!(drawn_by(&restarted).await?.as_deref(), Some("custom card"));
        module.apply(Source::Bundled, None).await?;
        assert_eq!(drawn_by(&module).await?.as_deref(), Some("release card"));
        Ok(())
    }

    /// The cards the native renderer showed before cards moved into the
    /// module, with the requests they were drawn from.
    #[derive(serde::Deserialize)]
    struct Recorded {
        name: String,
        request: CardRequest,
        card: Option<twilight_model::channel::message::Embed>,
    }

    #[tokio::test]
    async fn the_release_module_draws_what_the_native_renderer_drew() -> anyhow::Result<()> {
        let recorded: Vec<Recorded> = serde_json::from_str(include_str!(
            "../../carillon-discord-module/fixtures/cards.json"
        ))?;
        let names = recorded
            .iter()
            .map(|case| case.name.as_str())
            .collect::<Vec<_>>();
        for case in [
            "queued",
            "working",
            "interrupted",
            "earned-summary",
            "removal",
        ] {
            assert!(names.contains(&case), "{case} is not recorded");
        }
        let module = release_module().await?;
        for case in recorded {
            assert_eq!(
                module.render_card(&case.request).await?,
                case.card,
                "{}",
                case.name
            );
        }
        assert_eq!(
            super::check(&release_module_bytes()?).await?,
            ["model", "command"]
        );
        Ok(())
    }
    #[tokio::test]
    async fn the_example_module_colours_the_bundled_cards_through_the_host() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        std::fs::write(root.join("release.wasm"), release_module_bytes()?)?;
        std::fs::write(root.join("custom.wasm"), example_module_bytes()?)?;
        let module = Module::load(files(root), Source::Bundled).await?;
        let bundled = module
            .render_card(&working_card("Working"))
            .await?
            .context("the bundled module drew no card")?;
        assert_eq!(bundled.color, None);

        let change = module.apply(custom(root, "custom.wasm"), None).await?;
        assert_eq!(change.serving.commands, ["model", "command"]);
        let coloured = module
            .render_card(&working_card("Working"))
            .await?
            .context("the example module drew no card")?;
        assert_eq!(coloured.color, Some(0x58_65_F2));
        assert_eq!(coloured.description, bundled.description);
        assert_eq!(coloured.fields, bundled.fields);
        Ok(())
    }
}
