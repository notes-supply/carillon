//! The `carillon-discord` command: the provider service and its operator
//! actions. Core's `carillon discord …` forwards here with the resolved
//! `CARILLON_HOME`; the service itself is started with explicit files.

use std::{
    fs,
    io::Read as _,
    path::{Component, Path, PathBuf},
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow, bail};
use carillon_installation::{
    config::{LoadedConfig, ProviderRole, RELEASE_VERSION},
    home, loopback,
    prompt::Terminal,
};
use clap::{Parser, Subcommand};
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use tracing_subscriber::EnvFilter;

use crate::{
    application::{ServeOptions, serve},
    config::{self, Applied, DiscordConfig, DiscordRoute, FileChange, FileState},
    module::{self, Declared, Refusal, Registration, Source, Status},
    operations::{
        self, ATTACHMENT_BYTES, ATTACHMENT_COUNT, Messages, Operation, OperationError, Upload,
    },
    setup,
};
use twilight_http::Client as DiscordHttp;

/// Status requests only read.
const STATUS_TIMEOUT: Duration = Duration::from_secs(10);
/// Changes may wait for Discord to confirm a registration.
const CHANGE_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Parser)]
#[command(
    name = "carillon-discord",
    version,
    about = "Carillon Discord interface provider",
    after_help = "Run as `carillon discord …`. Slash commands and work cards come from a \
                  replaceable Discord module; see `carillon discord module --help`."
)]
struct Cli {
    /// Installation for operator actions. Defaults to `CARILLON_HOME`, then
    /// the installation containing this program, then `~/.carillon`.
    #[arg(long, global = true, value_name = "PATH")]
    home: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the provider (started by the installed user service).
    Serve {
        #[arg(long)]
        config: PathBuf,
        /// The provider's registry ID.
        #[arg(long)]
        provider: String,
        /// The provider's core credential; its settings name the bot token's.
        #[arg(long)]
        token_file: PathBuf,
        /// The release's bundled Discord module, served unless the settings
        /// enable a custom one.
        #[arg(long)]
        module: PathBuf,
        /// Serve local simulation endpoints instead of connecting to Discord.
        #[arg(long)]
        deterministic: bool,
    },
    /// Ask for the bot token, then the server, channels and people the agent
    /// answers, and write the token and the provider's admission settings.
    /// Given the guild, bot, people and routes, ask nothing and write those,
    /// keeping the bot token file and tuned feedback settings.
    #[command(after_help = "\
--route only creates the first routes: once the settings route channels, it
is refused unless it names exactly those routes. Change a channel's route
with `carillon settings <channel> agent=… listens=… replies=…` or /settings
in Discord.")]
    Setup {
        /// Registry ID of the Discord instance; inferred when only one is registered.
        #[arg(long)]
        provider: Option<String>,
        #[arg(long, requires_all = ["bot_user_id", "user_id", "route"])]
        guild_id: Option<String>,
        #[arg(long, requires = "guild_id")]
        bot_user_id: Option<String>,
        #[arg(long, requires = "guild_id")]
        user_id: Vec<String>,
        /// `CHANNEL_ID:mention|always:thread|channel:AGENT_ID`
        #[arg(long, requires = "guild_id")]
        route: Vec<DiscordRoute>,
    },
    /// Check, without changing anything, that this program accepts the
    /// instance's Discord settings and its installed Discord module.
    Check {
        /// Registry ID of the Discord instance; inferred when only one is configured.
        #[arg(long)]
        provider: Option<String>,
    },
    /// Call the running provider's Discord API; prints JSON and never retries a write.
    Call {
        /// Registry ID of the Discord instance; inferred when only one is configured.
        #[arg(long, global = true)]
        provider: Option<String>,
        #[command(subcommand)]
        operation: Operation,
    },
    /// Show the settings the running provider serves, or have it serve its
    /// settings file.
    #[command(after_help = "\
Without a subcommand, shows what the running provider serves and how its
settings file differs.

Allowed users, routes, typing, reactions and their emoji, and activity
markers apply live: messages and interactions admitted afterwards follow
them, while work already admitted keeps the settings it was admitted with.
The guild and bot identity, attachment limits and whether activity
cards are shown apply only when the provider restarts; a file that changes
any of them is not applied at all until then.")]
    Settings {
        /// Registry ID of the Discord instance; inferred when only one is configured.
        #[arg(long, global = true)]
        provider: Option<String>,
        #[command(subcommand)]
        action: Option<SettingsAction>,
    },
    /// Show which Discord module the running provider serves, or enable or
    /// disable a custom one.
    #[command(after_help = "\
Without a subcommand, shows the serving module, its commands, and whether the
settings file declares another.

The settings file's [module] table decides the module: a custom one while
enabled, the release's bundled module otherwise. Applying the settings serves
it without restarting the provider or reconnecting its gateway, rereading the
file so a rebuilt module at the same path serves too. Changed command
definitions are registered with Discord first. A module that cannot serve
leaves the serving module and settings in place. Interactions already running
finish on the module they started with; each card update is drawn by the
module serving then.")]
    Module {
        /// Registry ID of the Discord instance; inferred when only one is configured.
        #[arg(long, global = true)]
        provider: Option<String>,
        #[command(subcommand)]
        action: Option<ModuleAction>,
    },
}

#[derive(Debug, Subcommand)]
enum SettingsAction {
    /// Serve the settings file in the running provider, whole, when
    /// everything it changes applies live.
    Apply,
}

#[derive(Debug, Subcommand)]
enum ModuleAction {
    /// Check that FILE is a Discord module this program can serve, without
    /// serving it. Needs no installation or running provider.
    Check { file: PathBuf },
    /// Enable a custom module in the settings file, FILE or the one it
    /// already names, and apply the settings.
    Enable { file: Option<PathBuf> },
    /// Disable the custom module in the settings file and apply the
    /// settings, so the bundled module serves.
    Disable,
}

fn initialize_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("carillon_discord=info")),
        )
        .with_target(false)
        .init();
}

pub(crate) async fn run() -> Result<()> {
    initialize_tracing();
    let Cli { home, command } = Cli::parse();
    let installation = || -> Result<LoadedConfig> {
        LoadedConfig::load(&home::resolve(home.as_deref())?.join("carillon.toml"))
    };
    match command {
        Command::Serve {
            config,
            provider,
            token_file,
            module,
            deterministic,
        } => {
            serve(
                LoadedConfig::load(&config)?,
                ServeOptions {
                    provider_id: &provider,
                    token_file: &token_file,
                    module: &module,
                    deterministic,
                },
            )
            .await?;
        }
        Command::Setup {
            provider,
            guild_id,
            bot_user_id,
            user_id,
            route,
        } => {
            let loaded = installation()?;
            let provider = match provider {
                Some(provider) => provider,
                None => only_interface(&loaded)?,
            };
            match (guild_id, bot_user_id) {
                (Some(guild_id), Some(bot_user_id)) => {
                    setup::unattended(&loaded, &provider, guild_id, bot_user_id, user_id, route)
                        .await?;
                }
                _ => setup::setup(&mut Terminal, &loaded, &provider, DiscordHttp::new).await?,
            }
        }
        Command::Check { provider } => {
            let loaded = installation()?;
            let provider = instance(&loaded, provider)?;
            let discord = DiscordConfig::load(&loaded, &provider).with_context(|| {
                format!("carillon-discord {RELEASE_VERSION} cannot use the settings of {provider}")
            })?;
            println!(
                "{} is compatible with carillon-discord {}",
                loaded.provider_config_path(&provider)?.display(),
                RELEASE_VERSION
            );
            check_declared(&loaded, &provider, &discord, &release()?).await?;
        }
        Command::Call {
            provider,
            operation,
        } => {
            let loaded = installation()?;
            let provider = instance(&loaded, provider)?;
            let result = call(&loaded, &provider, operation).await?;
            println!("{}", serde_json::to_string(&result)?);
        }
        Command::Settings { provider, action } => {
            let loaded = installation()?;
            let provider = instance(&loaded, provider)?;
            manage_settings(&loaded, &provider, action).await?;
        }
        Command::Module { provider, action } => {
            manage_module(installation, provider, action).await?;
        }
    }
    Ok(())
}

/// The provider owns validation and Discord execution. This client only loads
/// caller-owned files and authentication outside the model's shell arguments.
async fn call(
    loaded: &LoadedConfig,
    provider: &str,
    mut operation: Operation,
) -> Result<serde_json::Value> {
    if let Operation::Messages(Messages::Send {
        files, attachments, ..
    }) = &mut operation
    {
        *attachments = uploads(files)?;
    }
    let mutates = operation.mutates();
    let target = loaded.provider(provider, ProviderRole::Interface)?;
    let url = format!("{}/v1/operations", target.url);
    let unanswered = |error: reqwest::Error| {
        if mutates && !error.is_connect() {
            anyhow!(
                "uncertain: Discord did not confirm the operation; inspect the target before repeating it"
            )
        } else {
            anyhow!("unavailable: the Discord provider did not complete the request")
        }
    };
    let mut response = loopback::client()
        .retry(reqwest::retry::never())
        .timeout(Duration::from_secs(35))
        .build()?
        .post(&url)
        .bearer_auth(loaded.credential("operator")?)
        .json(&operation)
        .send()
        .await
        .map_err(unanswered)?;
    let status = response.status();
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(unanswered)? {
        if chunk.len() > 2 * 1024 * 1024 - bytes.len() {
            bail!(
                "{}: Discord response exceeds 2 MiB; inspect the target before repeating a write",
                if mutates { "uncertain" } else { "unavailable" }
            );
        }
        bytes.extend_from_slice(&chunk);
    }
    let result = serde_json::from_slice::<serde_json::Value>(&bytes);
    // The status decides a failure: a 4xx was not carried out, whether or not
    // its body reads, such as an older provider's refusal of an operation.
    if !status.is_success() {
        let result = result.unwrap_or_default();
        let fallback = if status.is_client_error() {
            "rejected"
        } else if mutates {
            "uncertain"
        } else {
            "unavailable"
        };
        bail!(
            "{}: {}",
            result["outcome"].as_str().unwrap_or(fallback),
            result["message"]
                .as_str()
                .or_else(|| result["error"].as_str())
                .map_or_else(
                    || format!("the Discord provider answered {status}"),
                    str::to_owned
                )
        );
    }
    result.with_context(|| {
        format!(
            "{}: invalid Discord provider response; inspect the target before repeating a write",
            if mutates { "uncertain" } else { "unavailable" }
        )
    })
}

/// The files a message send names, read here so the provider receives
/// their bytes. Every limit is checked before anything is sent.
fn uploads(files: &[PathBuf]) -> Result<Vec<Upload>> {
    let refused = |failure: OperationError| anyhow!("{failure}; nothing was changed");
    if files.len() > ATTACHMENT_COUNT {
        return Err(refused(operations::too_many()));
    }
    let mut remaining = ATTACHMENT_BYTES;
    files
        .iter()
        .map(|path| {
            if !fs::metadata(path)?.is_file() {
                bail!("attachments must be regular files; nothing was changed");
            }
            let file = fs::File::open(path)?;
            if !file.metadata()?.is_file() {
                bail!("attachments must be regular files; nothing was changed");
            }
            let mut bytes = Vec::new();
            file.take(u64::try_from(remaining)? + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() > remaining {
                return Err(refused(operations::too_large()));
            }
            remaining -= bytes.len();
            let filename = path
                .file_name()
                .and_then(|name| name.to_str())
                .context("attachment filename must be UTF-8")?
                .to_owned();
            Upload::new(filename, &bytes).map_err(refused)
        })
        .collect()
}

/// The release this program belongs to, the directory holding its `bin`:
/// the installed application, or a release an update is checking.
fn release() -> Result<PathBuf> {
    let executable = std::env::current_exe()?.canonicalize()?;
    executable
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .context("cannot locate this program's release")
}

/// The only interface provider registered, which setup gives its first
/// settings.
fn only_interface(config: &LoadedConfig) -> Result<String> {
    let interfaces: Vec<&str> = config
        .value
        .providers
        .iter()
        .filter(|provider| provider.role == ProviderRole::Interface)
        .map(|provider| provider.id.as_str())
        .collect();
    match interfaces.as_slice() {
        [only] => Ok((*only).to_owned()),
        several => bail!(
            "choose the Discord instance to set up with --provider from: {}",
            several.join(", ")
        ),
    }
}

/// The Discord instance an operator action names, or the only interface
/// provider with Discord settings. Anything else is a choice for the operator.
fn instance(config: &LoadedConfig, explicit: Option<String>) -> Result<String> {
    if let Some(explicit) = explicit {
        return Ok(explicit);
    }
    let interfaces: Vec<&str> = config
        .value
        .providers
        .iter()
        .filter(|provider| provider.role == ProviderRole::Interface)
        .map(|provider| provider.id.as_str())
        .collect();
    let discord: Vec<&str> = interfaces
        .iter()
        .copied()
        .filter(|id| DiscordConfig::read(config, id).is_ok())
        .collect();
    match discord.as_slice() {
        [only] => Ok((*only).to_owned()),
        [] => bail!(
            "no interface provider has Discord settings this program can read; choose one \
             with --provider from: {}",
            interfaces.join(", ")
        ),
        several => bail!(
            "several Discord instances are configured; choose one with --provider from: {}",
            several.join(", ")
        ),
    }
}

/// `carillon discord module …`: a file check needs no installation; status
/// asks the running provider; enabling and disabling change the settings
/// file through the running provider, or in place while it is stopped.
async fn manage_module(
    installation: impl FnOnce() -> Result<LoadedConfig>,
    provider: Option<String>,
    action: Option<ModuleAction>,
) -> Result<()> {
    if let Some(ModuleAction::Check { file }) = &action {
        if let Some(provider) = provider {
            bail!("`module check` examines a file and does not use --provider {provider}");
        }
        let commands = check_file(file).await?;
        println!(
            "{} is a usable Discord module for carillon-discord {RELEASE_VERSION}: {}.",
            file.display(),
            list(&commands)
        );
        return Ok(());
    }
    let loaded = installation()?;
    let provider = instance(&loaded, provider)?;
    let settings = loaded.provider_config_path(&provider)?;
    let (module, declares) = match action {
        None | Some(ModuleAction::Check { .. }) => {
            let operator = Operator::new(&loaded, &provider, "module")?;
            print_status(&provider, &operator.request(Method::GET, None).await?);
            return Ok(());
        }
        Some(ModuleAction::Enable { file }) => {
            let given = file.map(std::path::absolute).transpose()?;
            let declared = match &given {
                Some(file) => file.clone(),
                None => DiscordConfig::read(&loaded, &provider)?
                    .module
                    .path
                    .context("the settings name no custom module; give its file")?,
            };
            // Checked before the settings change, against this release.
            let commands = check_file(&loaded.root.join(&declared)).await?;
            let named = format!(
                "the custom Discord module {}: {}",
                declared.display(),
                list(&commands)
            );
            let module = FileChange::Module {
                enabled: true,
                path: given,
            };
            (module, named)
        }
        Some(ModuleAction::Disable) => {
            let module = FileChange::Module {
                enabled: false,
                path: None,
            };
            (module, "the bundled Discord module".to_owned())
        }
    };
    deliver(&loaded, &provider, module).await?;
    println!("{} now declares {declares}.", settings.display());
    Ok(())
}

/// Validates the custom module the settings enable against this program, as
/// an update's preflight does before it stops the services. A module inside
/// the installed application is replaced with it, so the copy in `release`,
/// this program's own, is the one checked; any other is checked as it is.
async fn check_declared(
    loaded: &LoadedConfig,
    provider: &str,
    discord: &DiscordConfig,
    release: &Path,
) -> Result<()> {
    let Source::Custom { path } = discord.module_source(&loaded.root) else {
        return Ok(());
    };
    let shipped = in_application(loaded, &path).map(|relative| release.join(relative));
    let checked = shipped.as_deref().unwrap_or(&path);
    let commands = check_file(checked).await.with_context(|| {
        format!(
            "the settings of {provider} enable a custom Discord module this release cannot \
             serve. Run `carillon discord module disable`, then enable a module built for this \
             release"
        )
    })?;
    println!(
        "custom Discord module {}{} is usable by carillon-discord {RELEASE_VERSION}: {}.",
        path.display(),
        if shipped.is_some() {
            ", as this release ships it,"
        } else {
            ""
        },
        list(&commands)
    );
    Ok(())
}

/// Where `path` lies within the installation's application directory, if
/// it does: the application's own files are replaced by an update.
fn in_application(loaded: &LoadedConfig, path: &Path) -> Option<PathBuf> {
    let application = loaded.root.join("app");
    // A `..` can lead back out of the application, so only a path without
    // one is decided by its spelling; otherwise the files themselves decide.
    if let Ok(relative) = path.strip_prefix(&application)
        && !relative
            .components()
            .any(|part| part == Component::ParentDir)
    {
        return Some(relative.to_path_buf());
    }
    let (application, path) = (application.canonicalize().ok()?, path.canonicalize().ok()?);
    path.strip_prefix(application).ok().map(Path::to_path_buf)
}

async fn check_file(file: &Path) -> Result<Vec<String>> {
    let bytes = module::read_module(file)
        .with_context(|| format!("read Discord module {}", file.display()))?;
    module::check(&bytes).await.with_context(|| {
        format!(
            "{} is not a usable Discord module for carillon-discord {RELEASE_VERSION}",
            file.display()
        )
    })
}

/// The running provider's operator endpoint for its Discord module or its
/// settings, at the address `carillon.toml` gives the provider.
struct Operator {
    url: String,
    credential: String,
}

/// What became of asking the running provider to serve its settings file.
enum Applying {
    Applied(Applied),
    /// The provider answered, saying why, and serves what it served before.
    Refused(String),
    /// Nothing answers at the provider's address: it may be stopped, or
    /// still serve at an address `carillon.toml` no longer gives, which only
    /// a restart changes. Whether it serves the file is not confirmed.
    Unreachable,
}

impl Operator {
    fn new(loaded: &LoadedConfig, provider: &str, endpoint: &str) -> Result<Self> {
        let target = loaded.provider(provider, ProviderRole::Interface)?;
        Ok(Self {
            url: format!("{}/v1/{endpoint}", target.url),
            credential: loaded.credential("operator")?,
        })
    }

    /// One request, never repeated: a write the provider may have received
    /// is not sent again.
    async fn send(
        &self,
        method: Method,
        body: Option<Vec<u8>>,
        timeout: Duration,
    ) -> reqwest::Result<reqwest::Response> {
        let mut request = loopback::client()
            .retry(reqwest::retry::never())
            .build()?
            .request(method, &self.url)
            .bearer_auth(&self.credential)
            .timeout(timeout);
        if let Some(body) = body {
            request = request
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body);
        }
        request.send().await
    }

    /// The provider's answer, or its refusal rendered with what still serves.
    async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        body: Option<Vec<u8>>,
    ) -> Result<T> {
        let changing = method != Method::GET;
        let timeout = if changing {
            CHANGE_TIMEOUT
        } else {
            STATUS_TIMEOUT
        };
        let response = self
            .send(method, body, timeout)
            .await
            .map_err(|error| self.unreachable(&error, changing))?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|error| self.unreachable(&error, changing))?;
        if status.is_success() {
            return serde_json::from_slice(&bytes).with_context(|| {
                format!(
                    "unexpected answer from the Discord provider at {}",
                    self.url
                )
            });
        }
        if let Ok(Refusal { error, serving, .. }) = serde_json::from_slice(&bytes) {
            bail!(
                "{error}.\nStill serving {}: {}.",
                serving.source,
                list(&serving.commands)
            );
        }
        let message = refusal(&bytes);
        if status == StatusCode::NOT_FOUND {
            bail!(
                "the Discord provider at {} does not offer Discord module operations ({message}); \
                 it may be a different release than this program",
                self.url
            );
        }
        bail!("the Discord provider refused the request ({status}): {message}")
    }

    /// Asks the provider to serve its settings file, waiting as for any
    /// change, since the module it declares may need Discord to register
    /// commands first. Only its answer says what became of that; an apply
    /// is never sent twice.
    async fn apply_settings(&self) -> Result<Applying> {
        self.settings(None).await
    }

    /// Asks the provider to apply `change` to its settings file and serve
    /// it, as [`Self::apply_settings`] serves the file as it is.
    async fn change_settings(&self, change: &FileChange) -> Result<Applying> {
        self.settings(Some(serde_json::to_vec(change)?)).await
    }

    async fn settings(&self, change: Option<Vec<u8>>) -> Result<Applying> {
        let method = if change.is_some() {
            Method::PUT
        } else {
            Method::POST
        };
        let unconfirmed = |error: &dyn std::fmt::Display| {
            anyhow!(
                "the Discord provider at {} did not confirm whether it serves the settings file \
                 ({error}); `carillon discord settings` shows what it serves",
                self.url
            )
        };
        let response = match self.send(method, change, CHANGE_TIMEOUT).await {
            Ok(response) => response,
            Err(error) if error.is_connect() => return Ok(Applying::Unreachable),
            Err(error) => return Err(unconfirmed(&error)),
        };
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|error| unconfirmed(&error))?;
        if status.is_success() {
            return serde_json::from_slice(&bytes)
                .map(Applying::Applied)
                .map_err(|error| unconfirmed(&error));
        }
        let message = refusal(&bytes);
        if status == StatusCode::NOT_FOUND {
            return Ok(Applying::Refused(format!(
                "it does not offer settings operations ({message}) and may be a different release \
                 than this program"
            )));
        }
        if status.is_client_error() {
            return Ok(Applying::Refused(message));
        }
        Err(unconfirmed(&format!("{status}: {message}")))
    }

    /// A transport failure establishes only that the provider did not
    /// answer, not that it is stopped or that a change did not happen.
    fn unreachable(&self, error: &reqwest::Error, changing: bool) -> anyhow::Error {
        if error.is_timeout() {
            let outcome = if changing {
                " The change may still complete; check with `carillon discord settings`."
            } else {
                ""
            };
            anyhow!(
                "the Discord provider at {} did not answer in time.{outcome}",
                self.url
            )
        } else if error.is_connect() {
            anyhow!(
                "could not connect to the Discord provider at {}; nothing was changed. \
                 `carillon status` and `carillon logs discord` show whether it is running.",
                self.url
            )
        } else {
            anyhow!(
                "the request to the Discord provider at {} failed: {error}.{}",
                self.url,
                if changing {
                    " Whether the change happened is unknown; check with `carillon discord settings`."
                } else {
                    ""
                }
            )
        }
    }
}

fn refusal(bytes: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .and_then(|body| body["error"].as_str().map(str::to_owned))
        .unwrap_or_else(|| String::from_utf8_lossy(bytes).trim().to_owned())
}

/// `carillon discord settings [apply]`, answered by the running provider.
async fn manage_settings(
    loaded: &LoadedConfig,
    provider: &str,
    action: Option<SettingsAction>,
) -> Result<()> {
    let operator = Operator::new(loaded, provider, "settings")?;
    let Some(SettingsAction::Apply) = action else {
        print_settings(provider, &operator.request(Method::GET, None).await?);
        return Ok(());
    };
    match operator.apply_settings().await? {
        Applying::Applied(applied) => {
            report_applied(provider, &applied);
            Ok(())
        }
        Applying::Refused(refused) => {
            bail!("{refused}; {provider} keeps serving the settings it has")
        }
        Applying::Unreachable => bail!(
            "the Discord provider does not answer at {}, so it did not confirm serving the \
             settings file; a stopped provider reads it when it starts",
            operator.url
        ),
    }
}

fn report_applied(provider: &str, applied: &Applied) {
    let Applied {
        changed, module, ..
    } = applied;
    if changed.is_empty() {
        println!("{provider} already serves its settings file.");
    } else {
        println!(
            "{provider} now serves its settings file: {}.",
            changed.join(", ")
        );
    }
    if module.serving != module.previous {
        println!(
            "It serves {}: {}.",
            module.serving.source,
            list(&module.serving.commands)
        );
    }
    if module.discord == Registration::Registered {
        println!("Registered its commands with Discord.");
    }
}

fn print_settings(provider: &str, status: &config::Status) {
    let config::Status { serving, file } = status;
    let on = |enabled: bool| if enabled { "on" } else { "off" };
    println!(
        "{provider} serves guild {} as bot {}: {} allowed users, {} routes; \
         typing {}, reactions {}, activity cards {}.",
        serving.guild_id,
        serving.bot_user_id,
        serving.allowed_user_ids.len(),
        serving.routes.len(),
        on(serving.typing),
        on(serving.reactions),
        on(serving.activity),
    );
    match file {
        FileState::Read(difference)
            if difference.live.is_empty() && difference.restart.is_empty() =>
        {
            println!("It serves its settings file.");
        }
        FileState::Read(difference) => {
            if !difference.restart.is_empty() {
                println!(
                    "The settings file changes what only a restart applies ({}); \
                     `carillon restart` serves it, and until then it is not applied.",
                    difference.restart.join(", ")
                );
            }
            if !difference.live.is_empty() {
                println!(
                    "The settings file also changes: {}.",
                    difference.live.join(", ")
                );
            }
            if difference.restart.is_empty() {
                println!("`carillon discord settings apply` serves it.");
            }
        }
        FileState::Invalid { error } => {
            println!("The settings file is not valid, so nothing would apply it: {error}.");
        }
    }
}

fn list(commands: &[String]) -> String {
    if commands.is_empty() {
        return "no commands".to_owned();
    }
    commands
        .iter()
        .map(|name| format!("/{name}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn print_status(provider: &str, status: &Status) {
    let Status {
        serving,
        declared,
        registered,
    } = status;
    println!(
        "{provider} serves {}: {}.",
        serving.source,
        list(&serving.commands)
    );
    match declared {
        Declared::Serving => {}
        Declared::Other { source } if *source == serving.source => println!(
            "Its file has changed since it was applied; `carillon discord settings apply` serves \
             it as it is now."
        ),
        Declared::Other { source } => println!(
            "The settings file declares {source}; `carillon discord settings apply` serves it."
        ),
        Declared::Unknown { error } => {
            println!("The module the settings file declares cannot be read: {error}.");
        }
    }
    if *registered == Some(false) {
        println!(
            "Discord's registration of these commands is not confirmed; applying the settings \
             or restarting the provider registers them."
        );
    }
}

mod writing;

pub(crate) use writing::deliver;

#[cfg(test)]
mod tests;
