//! Hosting the replaceable Discord module, registering its commands,
//! running interactions with native calls, rendering work cards without
//! native calls, and switching modules while serving.
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
    CONTRACT, Card, CardRequest, Description, HostReply, HostRequest, Intent, Interaction,
    Invocation, Outcome, Reply,
};
use carillon_installation::{
    config::LoadedConfig,
    filesystem::{PublishError, atomic_write, remove, sync_ancestors},
    maintenance::MaintenanceLock,
};
use carillon_protocol::model::ConversationQuery;
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
    channels,
    client::{CoreClient, RUNTIME_LATE},
    config::{DiscordConfig, DiscordRoute},
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

/// The native operations one interaction may perform: the settings page of
/// the routed channel it happened in and changes to it; inside a
/// conversation, the runtime's commands there; and, with a Discord
/// connection, submitting text to the conversation as the person who
/// invoked it.
pub(crate) struct InteractionCalls {
    core: CoreClient,
    /// The route that admitted the interaction, also in one of its threads.
    route: DiscordRoute,
    /// The conversation it happened in, when the agent participates there.
    audience_id: Option<String>,
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
    pub(crate) const fn new(
        core: CoreClient,
        route: DiscordRoute,
        audience_id: Option<String>,
    ) -> Self {
        Self {
            core,
            route,
            audience_id,
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
    call: impl Future<Output = Result<T, crate::InterfaceError>>,
) -> Result<T, String> {
    timeout_at(until, call)
        .await
        .map_err(|_| RUNTIME_LATE.to_owned())?
        .map_err(|error| error.to_string())
}

impl InteractionCalls {
    /// Answers one of the interaction's native requests by `until`.
    async fn answer(&self, request: &[u8], until: Instant) -> HostReply {
        let title = format!("<#{}>", self.route.channel_id);
        match serde_json::from_slice::<HostRequest>(request) {
            Ok(HostRequest::Settings) => HostReply::Page {
                page: channels::page(&self.core, &self.route, title, until).await,
                notice: None,
            },
            Ok(HostRequest::ChangeSetting(change)) => {
                let (page, notice) =
                    channels::settled(&self.core, &self.route, title, change, until).await;
                HostReply::Page { page, notice }
            }
            Ok(HostRequest::Commands) => {
                let query = ConversationQuery {
                    agent_id: self.route.agent.clone(),
                    audience_id: self.audience_id.clone(),
                };
                match within(until, self.core.commands(&query)).await {
                    Ok(catalog) => HostReply::Commands(catalog),
                    Err(message) => HostReply::Error(message),
                }
            }
            Ok(HostRequest::Submit { text, intent }) => {
                match (&self.submissions, &self.audience_id) {
                    (Some(submissions), Some(audience_id)) => {
                        match submit(
                            &self.core,
                            submissions,
                            &self.route.agent,
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
                }
            }
            Err(error) => HostReply::Error(format!("invalid host request: {error}")),
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
pub(crate) enum Source {
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
pub(crate) struct ModuleSummary {
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
pub(crate) enum Registration {
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
pub(crate) struct Change {
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
pub(crate) enum ChangeError {
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
        let payload = serde_json::to_vec(&Invocation::Interaction(Box::new(interaction)))?;
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
    for (case, request) in smoke::cards() {
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
        Card::Show(embed) => Ok(answer::embeds(vec![embed])
            .context("the card's embed is unusable")?
            .pop()),
        Card::Remove if request.job.settled().is_some() => Ok(None),
        Card::Remove => bail!("Discord module removed the card of a job that has not settled"),
    }
}

/// Serializes exactly as `carillon_discord_module::contract::Invocation::Card`, which the guest decodes.
#[derive(Serialize)]
enum InvocationRef<'a> {
    Card(&'a CardRequest),
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

pub(crate) mod answer;
#[cfg(test)]
mod measurement;
mod smoke;

#[cfg(test)]
pub(crate) mod tests;
