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
    maintenance::MaintenanceLock,
    prompt::Terminal,
};
use clap::{Parser, Subcommand};
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use tracing_subscriber::EnvFilter;

use crate::{
    application::{ServeOptions, serve},
    config::{self, Applied, DiscordConfig, DiscordRoute, FileState, configure, declare_module},
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
        /// The provider's core credential.
        #[arg(long)]
        token_file: PathBuf,
        /// The Discord bot token; required outside deterministic mode.
        #[arg(long)]
        discord_token_file: Option<PathBuf>,
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
    Setup {
        /// Registry ID of the Discord instance; inferred when only one is registered.
        #[arg(long)]
        provider: Option<String>,
    },
    /// Write the provider's admission settings, keeping tuned feedback
    /// settings, then have a running provider serve them when they apply live.
    Configure {
        /// Registry ID of the Discord instance; inferred when only one is configured.
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        guild_id: String,
        #[arg(long)]
        bot_user_id: String,
        #[arg(long, required = true)]
        user_id: Vec<String>,
        #[arg(long, required = true)]
        route: Vec<DiscordRoute>,
    },
    /// Check, without changing anything, that this program accepts the
    /// instance's Discord settings and its installed Discord module.
    Check {
        /// Registry ID of the Discord instance; inferred when only one is configured.
        #[arg(long)]
        provider: Option<String>,
    },
    /// Install Discord tools for Pi or a skill for Claude Code and Codex.
    Tools {
        #[command(subcommand)]
        action: ToolsAction,
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
The agent, guild and bot identity, attachment limits and whether activity
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
enum ToolsAction {
    /// Install Pi tools, or only the common skill in a native T3 workspace.
    Install {
        /// Install the skill for Claude Code and Codex in this workspace, without Pi tools.
        #[arg(long, value_name = "PATH")]
        workspace: Option<PathBuf>,
        /// Operator-provided invocation to use instead of `carillon discord call`.
        #[arg(long, requires = "workspace", value_name = "COMMAND")]
        invocation: Option<String>,
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
            discord_token_file,
            module,
            deterministic,
        } => {
            serve(
                LoadedConfig::load(&config)?,
                ServeOptions {
                    provider_id: &provider,
                    token_file: &token_file,
                    discord_token_file: discord_token_file.as_deref(),
                    module: &module,
                    deterministic,
                },
            )
            .await?;
        }
        Command::Setup { provider } => set_up(&installation()?, provider).await?,
        Command::Configure {
            provider,
            guild_id,
            bot_user_id,
            user_id,
            route,
        } => {
            let loaded = installation()?;
            let provider = instance(&loaded, provider)?;
            let maintenance = MaintenanceLock::acquire_unless_held(&loaded.root)
                .context("the Discord settings were not changed")?;
            configure(
                &loaded,
                &provider,
                &guild_id,
                &bot_user_id,
                &user_id,
                &route,
            )?;
            drop(maintenance);
            println!("{}", loaded.provider_config_path(&provider)?.display());
            serve_configured(&loaded, &provider).await?;
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
        Command::Tools {
            action:
                ToolsAction::Install {
                    workspace,
                    invocation,
                },
        } => {
            let lib = release_library()?;
            if let Some(workspace) = workspace {
                install_workspace_skill(&workspace, &lib, invocation.as_deref())?;
            } else {
                install_tools(&installation()?, &lib)?;
            }
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

/// The release's `lib` directory, beside the directory of this program.
fn release_library() -> Result<PathBuf> {
    Ok(release()?.join("lib"))
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

/// Native harnesses own these resource directories and when changes are loaded.
fn install_workspace_skill(workspace: &Path, lib: &Path, invocation: Option<&str>) -> Result<()> {
    use carillon_installation::filesystem::{publish, sync_ancestors};
    // Syncing reaches every ancestor, which a relative path does not name.
    let workspace = &std::path::absolute(workspace)?;
    if !workspace.is_dir() {
        bail!("workspace must be an existing directory");
    }
    let mut skill = fs::read_to_string(lib.join("carillon-discord-SKILL.md"))?;
    if let Some(invocation) = invocation {
        skill
            .push_str("\n## Local installation\n\nThe operator-provided invocation is:\n\n```sh\n");
        skill.push_str(invocation);
        skill.push_str("\n```\n\nUse it in place of `carillon discord call` in the examples and help commands. This does not grant additional OS authority.\n");
    }
    for root in [".claude", ".agents"] {
        let directory = workspace.join(root).join("skills/discord");
        fs::create_dir_all(&directory)?;
        sync_ancestors(&directory)?;
        publish(&directory.join("SKILL.md"), skill.as_bytes(), 0o644)?;
    }
    println!(
        "Installed Discord skill for Claude Code and Codex in {}",
        workspace.display()
    );
    Ok(())
}

/// Copy the release's tools and skill from `lib` into the agent resources,
/// with core stopped: core captures resources as it accepts turns, so it
/// must not see one file without the other.
fn install_tools(config: &LoadedConfig, lib: &Path) -> Result<()> {
    use carillon_installation::{
        filesystem::{publish, sync_ancestors},
        maintenance::ServiceLock,
    };
    let _maintenance = MaintenanceLock::acquire(&config.root)?;
    // Core cannot start while this is held, nor serve while it is taken.
    let Some(_core) = ServiceLock::try_acquire(&config.value.data_dir)? else {
        bail!(
            "Carillon core is running; stop it (`carillon stop`), install the Discord tools, then start it again (`carillon start`)"
        );
    };
    // Read both before changing resources. The operator explicitly replaces these two files.
    let extension = fs::read(lib.join("carillon-discord-pi.js"))
        .context("release has no bundled Discord Pi tools; use a packaged installation")?;
    let skill = fs::read(lib.join("carillon-discord-SKILL.md"))?;
    let root = &config.value.agent.resource_root;
    let extensions = root.join("extensions");
    let skills = root.join("skills/discord");
    (|| -> Result<()> {
        fs::create_dir_all(&extensions)?;
        fs::create_dir_all(&skills)?;
        sync_ancestors(&extensions)?;
        sync_ancestors(&skills)?;
        publish(&extensions.join("carillon-discord.js"), &extension, 0o600)?;
        publish(&skills.join("SKILL.md"), &skill, 0o600)?;
        Ok(())
    })()
    .context(
        "the Discord tools may be partly installed: run `carillon discord tools install` again until it succeeds before starting Carillon core",
    )?;
    println!(
        "Installed Discord tools and skill in {}. Start Carillon core (`carillon start`) to serve them: turns it accepts from then on capture them, and turns already accepted keep their resources.",
        root.display()
    );
    Ok(())
}

/// `carillon discord setup`, then has a running provider serve the settings.
async fn set_up(loaded: &LoadedConfig, provider: Option<String>) -> Result<()> {
    let provider = match provider {
        Some(provider) => provider,
        None => only_interface(loaded)?,
    };
    setup::setup(&mut Terminal, loaded, &provider, DiscordHttp::new).await?;
    let operator = Operator::new(loaded, &provider, "settings")?;
    match operator.apply_settings().await? {
        Applying::Applied(applied) => report_applied(&provider, &applied),
        Applying::Refused(refusal) => println!(
            "{provider} keeps serving its earlier settings: {refusal}. `carillon restart` serves \
             the new ones."
        ),
        // A stopped provider reads them when it starts.
        Applying::Unreachable => {}
    }
    Ok(())
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
/// file, then ask the running provider to apply it.
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
    match action {
        None | Some(ModuleAction::Check { .. }) => {
            let operator = Operator::new(&loaded, &provider, "module")?;
            print_status(&provider, &operator.request(Method::GET, None).await?);
            return Ok(());
        }
        Some(ModuleAction::Enable { file }) => {
            let declared = match file {
                Some(file) => std::path::absolute(file)?,
                None => DiscordConfig::read(&loaded, &provider)?
                    .module
                    .path
                    .context("the settings name no custom module; give its file")?,
            };
            // Checked before the settings change, against this release.
            let commands = check_file(&loaded.root.join(&declared)).await?;
            declare_module(&loaded, &provider, |module| {
                module.enabled = true;
                module.path = Some(declared.clone());
            })?;
            println!(
                "{} now declares the custom Discord module {}: {}.",
                settings.display(),
                declared.display(),
                list(&commands)
            );
        }
        Some(ModuleAction::Disable) => {
            declare_module(&loaded, &provider, |module| module.enabled = false)?;
            println!(
                "{} now declares the bundled Discord module.",
                settings.display()
            );
        }
    }
    let operator = Operator::new(&loaded, &provider, "settings")?;
    match operator.apply_settings().await? {
        Applying::Applied(applied) => {
            report_applied(&provider, &applied);
            Ok(())
        }
        Applying::Refused(refusal) => bail!(
            "{provider} did not apply the settings file: {refusal}. What it serves is unchanged; \
             `carillon discord module` shows which module that is"
        ),
        Applying::Unreachable => {
            println!(
                "The Discord provider does not answer at {}, so whether it serves the declared \
                 module is not confirmed: a stopped provider serves it when it starts.",
                operator.url
            );
            Ok(())
        }
    }
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
    /// The provider answered and serves what it served before, because of
    /// this.
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
            request = request.body(body);
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
        if let Ok(Refusal { error, serving }) = serde_json::from_slice(&bytes) {
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
        let unconfirmed = |error: &dyn std::fmt::Display| {
            anyhow!(
                "the Discord provider at {} did not confirm whether it serves the settings file \
                 ({error}); `carillon discord settings` shows what it serves",
                self.url
            )
        };
        let response = match self.send(Method::POST, None, CHANGE_TIMEOUT).await {
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
                "it does not offer settings operations ({message}) and may be a different \
                 release than this program"
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

/// The reason an error answer gives.
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
        Applying::Refused(refusal) => {
            bail!("{refusal}; {provider} keeps serving the settings it has")
        }
        Applying::Unreachable => bail!(
            "the Discord provider does not answer at {}, so it did not confirm serving the \
             settings file; a stopped provider reads it when it starts",
            operator.url
        ),
    }
}

/// After `configure` saved the settings file: has a running provider serve
/// it, and says what became of that.
async fn serve_configured(loaded: &LoadedConfig, provider: &str) -> Result<()> {
    let operator = Operator::new(loaded, provider, "settings")?;
    match operator.apply_settings().await? {
        Applying::Applied(applied) => report_applied(provider, &applied),
        Applying::Refused(refusal) => println!(
            "{provider} keeps serving the settings it has: {refusal}. `carillon restart` serves \
             the file."
        ),
        Applying::Unreachable => println!(
            "The Discord provider does not answer at {}, so whether it serves these settings is \
             not confirmed: a stopped provider reads them when it starts, and \
             `carillon discord settings` shows what a running one serves.",
            operator.url
        ),
    }
    Ok(())
}

fn report_applied(provider: &str, applied: &Applied) {
    let Applied { changed, module } = applied;
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
        "{provider} serves guild {} as bot {} for agent {}: {} allowed users, {} routes; \
         typing {}, reactions {}, activity cards {}.",
        serving.guild_id,
        serving.bot_user_id,
        serving.agent,
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

#[cfg(test)]
mod tests {
    use std::{fs, io::Write as _, os::unix::fs::PermissionsExt as _, path::Path};

    use anyhow::{Context as _, Result};
    use carillon_installation::{
        config::{LoadedConfig, Provider, ProviderRole},
        maintenance::ServiceLock,
    };

    use super::{Cli, Command, call, install_tools, install_workspace_skill, instance, uploads};
    use crate::operations::{Messages, Operation};
    use axum::{
        Router,
        body::Bytes,
        http::{HeaderMap, StatusCode, Uri},
        response::IntoResponse as _,
    };
    use clap::Parser as _;
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};

    fn parsed_call(arguments: &[&str]) -> Result<Operation> {
        let cli = Cli::try_parse_from(
            ["carillon-discord", "call"]
                .into_iter()
                .chain(arguments.iter().copied()),
        )?;
        let Command::Call { operation, .. } = cli.command else {
            anyhow::bail!("not a call");
        };
        Ok(operation)
    }

    /// A provider at a loopback address that reads each request and gives
    /// `answer`, or closes without one. It counts the requests it received.
    async fn applying_provider(
        answer: Option<&'static [u8]>,
    ) -> Result<(super::Operator, tokio::task::JoinHandle<usize>)> {
        answering_after(answer.map(<[u8]>::to_vec), std::time::Duration::ZERO).await
    }

    /// [`applying_provider`], answering each request `delay` after reading it.
    async fn answering_after(
        answer: Option<Vec<u8>>,
        delay: std::time::Duration,
    ) -> Result<(super::Operator, tokio::task::JoinHandle<usize>)> {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.2:0").await?;
        let operator = super::Operator {
            url: format!("http://{}/v1/settings", listener.local_addr()?),
            credential: "operator".to_owned(),
        };
        let server = tokio::spawn(async move {
            let mut requests = 0;
            while let Ok(Ok((mut stream, _))) =
                tokio::time::timeout(std::time::Duration::from_secs(1), listener.accept()).await
            {
                requests += 1;
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let Ok(byte) = stream.read_u8().await else {
                        break;
                    };
                    request.push(byte);
                }
                tokio::time::sleep(delay).await;
                if let Some(answer) = &answer {
                    let _ = stream.write_all(answer).await;
                }
            }
            requests
        });
        Ok((operator, server))
    }

    #[tokio::test]
    async fn a_settings_apply_is_sent_once_and_only_an_answer_confirms_it() -> Result<()> {
        // Nothing answering at the provider's configured address does not
        // confirm it is stopped.
        let (mut operator, server) = applying_provider(None).await?;
        server.abort();
        let unused = tokio::net::TcpListener::bind("127.0.0.2:0").await?;
        operator.url = format!("http://{}/v1/settings", unused.local_addr()?);
        drop(unused);
        assert!(matches!(
            operator.apply_settings().await?,
            super::Applying::Unreachable
        ));
        // A refusal is the provider's answer.
        let refusal: &[u8] =
            b"HTTP/1.1 409 Conflict\r\nContent-Length: 25\r\n\r\n{\"error\":\"guild changed\"}";
        let (operator, server) = applying_provider(Some(refusal)).await?;
        let super::Applying::Refused(message) = operator.apply_settings().await? else {
            anyhow::bail!("a refusal was not reported as one");
        };
        assert_eq!(message, "guild changed");
        assert_eq!(server.await?, 1);
        // A request the provider received without answering may have been
        // applied: it is reported as unconfirmed and not sent again.
        let (operator, server) = applying_provider(None).await?;
        let unconfirmed = operator
            .apply_settings()
            .await
            .err()
            .context("an unanswered apply was confirmed")?;
        assert!(
            unconfirmed.to_string().contains("did not confirm"),
            "{unconfirmed}"
        );
        assert_eq!(server.await?, 1);
        Ok(())
    }

    /// An apply that registers a changed module's commands can take longer
    /// than a status read; it is waited for as a change, and sent once.
    #[tokio::test]
    async fn an_apply_slower_than_a_status_read_is_still_confirmed() -> Result<()> {
        let summary =
            |digest: &str| json!({"source": "bundled", "commands": ["model"], "digest": digest});
        let body = json!({
            "changed": ["module"],
            "module": {"serving": summary("new"), "previous": summary("old"), "discord": "registered"},
        })
        .to_string();
        let answer = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let (operator, server) = answering_after(
            Some(answer.into_bytes()),
            super::STATUS_TIMEOUT + std::time::Duration::from_secs(1),
        )
        .await?;
        let super::Applying::Applied(applied) = operator.apply_settings().await? else {
            anyhow::bail!("a slow apply was not confirmed");
        };
        assert_eq!(applied.changed, ["module"]);
        assert_eq!(server.await?, 1);
        Ok(())
    }

    #[test]
    fn call_preserves_large_ids_and_explicit_false_flags() -> Result<()> {
        let operation = parsed_call(&[
            "messages",
            "pin",
            "--channel-id",
            "1557758596627038269",
            "--message-id",
            "1557758596627038270",
            "--pinned",
            "false",
        ])?;
        let encoded = serde_json::to_value(&operation)?;
        assert_eq!(
            encoded,
            json!({
                "action": "messages.pin", "channel_id": "1557758596627038269",
                "message_id": "1557758596627038270", "pinned": false,
            })
        );
        let decoded: Operation = serde_json::from_value(encoded)?;
        assert!(matches!(
            decoded,
            Operation::Messages(Messages::Pin { pinned: false, .. })
        ));
        Ok(())
    }

    #[test]
    fn call_requires_the_intent_of_toggle_mutations_and_valid_ids() {
        assert!(
            parsed_call(&["messages", "pin", "--channel-id", "2", "--message-id", "3"]).is_err()
        );
        assert!(parsed_call(&["members", "ban", "--user-id", "3"]).is_err());
        assert!(
            parsed_call(&["messages", "get", "--channel-id", "0", "--message-id", "3"]).is_err()
        );
    }

    struct CallProvider {
        config: LoadedConfig,
        seen: Arc<Mutex<Vec<(String, Value)>>>,
        server: tokio::task::JoinHandle<std::io::Result<()>>,
        directory: tempfile::TempDir,
    }

    impl Drop for CallProvider {
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    impl CallProvider {
        async fn new(status: StatusCode, answer: String) -> Result<Self> {
            let directory = tempfile::tempdir()?;
            let mut config =
                crate::config::tests::installation(directory.path(), "discord-private", 46_400)?;
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let target = config
                .value
                .providers
                .iter_mut()
                .find(|provider| provider.id == "discord-private")
                .context("missing provider")?;
            target.url = format!("http://{}", listener.local_addr()?);
            let seen = Arc::new(Mutex::new(Vec::new()));
            let requests = seen.clone();
            let router = Router::new().fallback(move |headers: HeaderMap, uri: Uri, body: Bytes| {
                let requests = requests.clone();
                let answer = answer.clone();
                async move {
                    let recorded = json!({
                        "authorization": headers.get("authorization").and_then(|value| value.to_str().ok()),
                        "operation": serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null),
                    });
                    if let Ok(mut requests) = requests.lock() {
                        requests.push((uri.to_string(), recorded));
                    }
                    let mut response = (status, answer).into_response();
                    if status.is_redirection() {
                        response.headers_mut().insert("location", axum::http::HeaderValue::from_static("/redirected"));
                    }
                    response
                }
            });
            let server = tokio::spawn(async move { axum::serve(listener, router).await });
            Ok(Self {
                config,
                seen,
                server,
                directory,
            })
        }

        fn requests(&self) -> Result<Vec<(String, Value)>> {
            Ok(self
                .seen
                .lock()
                .map_err(|_| anyhow::anyhow!("poisoned requests"))?
                .clone())
        }
    }

    #[tokio::test]
    async fn call_authenticates_at_the_existing_endpoint_and_returns_native_json() -> Result<()> {
        let provider = CallProvider::new(
            StatusCode::OK,
            json!({"messages": [], "may_have_more": false}).to_string(),
        )
        .await?;
        let operation = parsed_call(&["messages", "list", "--channel-id", "2", "--limit", "7"])?;
        let result = call(&provider.config, "discord-private", operation).await?;
        assert_eq!(result, json!({"messages": [], "may_have_more": false}));
        assert_eq!(
            provider.requests()?,
            vec![(
                "/v1/operations".into(),
                json!({
                    "authorization": "Bearer operator",
                    "operation": {"action": "messages.list", "channel_id": "2", "before": null, "limit": 7},
                })
            )]
        );
        Ok(())
    }

    #[tokio::test]
    async fn call_never_retries_an_uncertain_write_or_follows_a_redirect() -> Result<()> {
        for (status, answer) in [
            (
                StatusCode::BAD_GATEWAY,
                json!({"outcome": "uncertain", "message": "inspect the message"}).to_string(),
            ),
            (StatusCode::FOUND, json!({"error": "moved"}).to_string()),
        ] {
            let provider = CallProvider::new(status, answer).await?;
            let operation = parsed_call(&[
                "messages",
                "delete",
                "--channel-id",
                "2",
                "--message-id",
                "3",
            ])?;
            let error = call(&provider.config, "discord-private", operation)
                .await
                .err()
                .context("write should fail")?;
            assert!(error.to_string().starts_with("uncertain:"), "{error}");
            assert_eq!(provider.requests()?.len(), 1);
        }
        Ok(())
    }

    #[tokio::test]
    async fn an_unreadable_answer_preserves_write_uncertainty_without_replaying() -> Result<()> {
        for answer in ["not JSON".to_owned(), "x".repeat(3 * 1024 * 1024)] {
            let provider = CallProvider::new(StatusCode::OK, answer).await?;
            let operation = parsed_call(&[
                "messages",
                "send",
                "--channel-id",
                "2",
                "--content",
                "hello",
            ])?;
            let error = call(&provider.config, "discord-private", operation)
                .await
                .err()
                .context("invalid answer should fail")?;
            assert!(error.to_string().starts_with("uncertain:"), "{error}");
            assert_eq!(provider.requests()?.len(), 1);
        }
        Ok(())
    }

    #[tokio::test]
    async fn a_refusal_without_a_readable_body_keeps_its_status() -> Result<()> {
        // An older provider's framework rejects an operation it does not
        // know in plain text; a missing route answers with no body at all.
        for (status, answer, outcome) in [
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                "Failed to deserialize the JSON body",
                "rejected",
            ),
            (StatusCode::NOT_FOUND, "", "rejected"),
            (StatusCode::BAD_GATEWAY, "upstream failed", "uncertain"),
        ] {
            let provider = CallProvider::new(status, answer.to_owned()).await?;
            let operation = parsed_call(&[
                "messages",
                "send",
                "--channel-id",
                "2",
                "--content",
                "hello",
            ])?;
            let error = call(&provider.config, "discord-private", operation)
                .await
                .err()
                .context("a refusal succeeded")?
                .to_string();
            assert!(error.starts_with(&format!("{outcome}:")), "{error}");
            assert!(error.contains(status.as_str()), "{error}");
            assert_eq!(provider.requests()?.len(), 1);
        }
        Ok(())
    }

    #[tokio::test]
    async fn upload_rejections_happen_before_any_http_operation() -> Result<()> {
        let provider = CallProvider::new(StatusCode::OK, "{}".into()).await?;
        let file = provider.directory.path().join("oversize");
        fs::File::create(&file)?.set_len(5 * 1024 * 1024)?;
        let path = file.to_str().context("temporary path")?;
        let send = |files: &[&str]| {
            let mut arguments = vec!["messages", "send", "--channel-id", "2", "--content", "hi"];
            for file in files {
                arguments.extend(["--file", file]);
            }
            parsed_call(&arguments)
        };
        let refused = call(&provider.config, "discord-private", send(&[path])?)
            .await
            .err()
            .context("an oversized file was sent")?;
        assert!(refused.to_string().contains("4 MiB"), "{refused}");
        let small = provider.directory.path().join("small");
        fs::write(&small, "x")?;
        let small = small.to_str().context("temporary path")?;
        let refused = call(&provider.config, "discord-private", send(&[small; 11])?)
            .await
            .err()
            .context("eleven files were sent")?;
        assert!(refused.to_string().contains("At most 10"), "{refused}");
        assert!(provider.requests()?.is_empty());
        assert!(uploads(&[provider.directory.path().to_path_buf()]).is_err());
        // Only a message send takes files.
        assert!(
            parsed_call(&[
                "messages",
                "get",
                "--channel-id",
                "2",
                "--message-id",
                "3",
                "--file",
                small
            ])
            .is_err()
        );
        assert!(parsed_call(&["--file", small, "messages", "send", "--channel-id", "2"]).is_err());
        Ok(())
    }

    #[test]
    fn a_timeout_is_set_or_cleared_explicitly() -> Result<()> {
        let timeout = |arguments: &[&str]| {
            let mut all = vec!["members", "timeout", "--user-id", "3"];
            all.extend(arguments);
            parsed_call(&all).and_then(|operation| Ok(serde_json::to_value(operation)?))
        };
        assert!(timeout(&[]).is_err(), "no expiry must not remove a timeout");
        assert!(timeout(&["--clear", "--until", "2030-01-01T00:00:00Z"]).is_err());
        assert_eq!(
            timeout(&["--clear"])?,
            json!({"action": "members.timeout", "user_id": "3", "until": null})
        );
        assert_eq!(
            timeout(&["--until", "2030-01-01T00:00:00Z"])?,
            json!({"action": "members.timeout", "user_id": "3", "until": "2030-01-01T00:00:00Z"})
        );
        // On the wire, too, only an explicit null clears.
        assert!(
            serde_json::from_value::<Operation>(
                json!({"action": "members.timeout", "user_id": "3"})
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn the_provider_is_named_before_or_after_the_operation() -> Result<()> {
        for arguments in [
            ["--provider", "discord-b", "server", "info"],
            ["server", "info", "--provider", "discord-b"],
        ] {
            let cli =
                Cli::try_parse_from(["carillon-discord", "call"].into_iter().chain(arguments))?;
            let Command::Call { provider, .. } = cli.command else {
                anyhow::bail!("not a call");
            };
            assert_eq!(provider.as_deref(), Some("discord-b"));
        }
        Ok(())
    }

    #[tokio::test]
    async fn message_files_are_loaded_locally_and_sent_as_bounded_bytes() -> Result<()> {
        let provider = CallProvider::new(StatusCode::OK, json!({"id": "3"}).to_string()).await?;
        let file = provider.directory.path().join("evidence.txt");
        fs::write(&file, b"hello\n")?;
        let operation = parsed_call(&[
            "messages",
            "send",
            "--channel-id",
            "2",
            "--content",
            "evidence",
            "--file",
            file.to_str().context("temporary path")?,
        ])?;
        call(&provider.config, "discord-private", operation).await?;
        let requests = provider.requests()?;
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].1["operation"]["attachments"],
            json!([
                {"filename": "evidence.txt", "data": "aGVsbG8K"}
            ])
        );
        Ok(())
    }

    #[test]
    fn workspace_install_uses_both_native_skill_locations_without_touching_other_resources()
    -> Result<()> {
        let directory = tempfile::tempdir()?;
        let (config, lib) = tools_fixture(directory.path())?;
        let workspace = directory.path().join("workspace");
        fs::create_dir(&workspace)?;
        let personal = workspace.join("CLAUDE.md");
        fs::write(&personal, "personal instructions")?;
        // Native workspace skills do not participate in Pi's resource capture.
        let _serving = ServiceLock::acquire(&config.value.data_dir)?;
        install_workspace_skill(&workspace, &lib, Some("owner-cli discord call"))?;
        for root in [".claude", ".agents"] {
            let installed =
                fs::read_to_string(workspace.join(root).join("skills/discord/SKILL.md"))?;
            assert!(installed.starts_with("Discord skill\n"));
            assert!(installed.contains("owner-cli discord call"));
        }
        assert_eq!(fs::read_to_string(personal)?, "personal instructions");
        assert!(!workspace.join("extensions").exists());
        install_workspace_skill(&workspace, &lib, None)?;
        assert_eq!(
            fs::read_to_string(workspace.join(".agents/skills/discord/SKILL.md"))?,
            "Discord skill\n"
        );
        assert!(install_workspace_skill(&workspace.join("missing"), &lib, None).is_err());
        assert!(!workspace.join("missing").exists());
        // A workspace named relative to the current directory, as people type it.
        let relative = std::env::current_dir()?
            .components()
            .skip(1)
            .map(|_| Path::new(".."))
            .collect::<std::path::PathBuf>()
            .join(workspace.strip_prefix("/")?);
        assert!(relative.is_relative());
        fs::remove_dir_all(workspace.join(".claude"))?;
        install_workspace_skill(&relative, &lib, None)?;
        assert_eq!(
            fs::read_to_string(workspace.join(".claude/skills/discord/SKILL.md"))?,
            "Discord skill\n"
        );
        assert!(
            Cli::try_parse_from([
                "carillon-discord",
                "tools",
                "install",
                "--invocation",
                "owner-cli"
            ])
            .is_err()
        );
        Ok(())
    }

    /// An installation whose agent resources already hold a file of the
    /// owner's own, and a release library with the Discord tools.
    fn tools_fixture(root: &Path) -> Result<(LoadedConfig, std::path::PathBuf)> {
        let config = crate::config::tests::installation(
            &root.join("installation"),
            "discord-private",
            46_400,
        )?;
        let lib = root.join("lib");
        fs::create_dir_all(&lib)?;
        fs::write(
            lib.join("carillon-discord-pi.js"),
            "export default function tools() {}\n",
        )?;
        fs::write(lib.join("carillon-discord-SKILL.md"), "Discord skill\n")?;
        let extensions = config.value.agent.resource_root.join("extensions");
        fs::create_dir_all(&extensions)?;
        fs::write(extensions.join("custom.js"), "the owner's own")?;
        Ok((config, lib))
    }

    #[test]
    fn tools_install_refuses_while_core_serves() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let (config, lib) = tools_fixture(directory.path())?;
        let resources = &config.value.agent.resource_root;
        // Core holds its lock while it serves.
        let serving = ServiceLock::acquire(&config.value.data_dir)?;
        let refused = install_tools(&config, &lib)
            .err()
            .context("installed while core was serving")?;
        assert!(
            format!("{refused:#}").contains("Carillon core is running"),
            "{refused:#}"
        );
        assert!(!resources.join("extensions/carillon-discord.js").exists());
        assert!(!resources.join("skills/discord/SKILL.md").exists());
        drop(serving);
        Ok(())
    }

    #[test]
    fn tools_install_keeps_core_from_starting_until_both_files_are_in_place() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let (config, lib) = tools_fixture(directory.path())?;
        let resources = &config.value.agent.resource_root;
        // The release skill is a FIFO, so the install waits while reading it.
        let skill = lib.join("carillon-discord-SKILL.md");
        fs::remove_file(&skill)?;
        let made = std::process::Command::new("mkfifo").arg(&skill).status()?;
        assert!(made.success());
        std::thread::scope(|scope| -> Result<()> {
            let installing = scope.spawn(|| install_tools(&config, &lib));
            // Opening the FIFO waits for the install to read it.
            let mut writer = fs::OpenOptions::new().write(true).open(&skill)?;
            assert!(
                ServiceLock::try_acquire(&config.value.data_dir)?.is_none(),
                "core could start while the tools were being installed"
            );
            writer.write_all(b"Discord skill\n")?;
            drop(writer);
            installing
                .join()
                .map_err(|_| anyhow::anyhow!("install panicked"))?
        })?;
        assert_eq!(
            fs::read_to_string(resources.join("extensions/carillon-discord.js"))?,
            "export default function tools() {}\n"
        );
        assert_eq!(
            fs::read_to_string(resources.join("skills/discord/SKILL.md"))?,
            "Discord skill\n"
        );
        assert_eq!(
            fs::read_to_string(resources.join("extensions/custom.js"))?,
            "the owner's own"
        );
        assert!(ServiceLock::try_acquire(&config.value.data_dir)?.is_some());
        Ok(())
    }

    #[test]
    fn a_failed_tools_install_says_it_may_be_partial_and_a_retry_completes_it() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let (config, lib) = tools_fixture(directory.path())?;
        let resources = &config.value.agent.resource_root;
        // The skill cannot be written, so only the extension can change.
        let skills = resources.join("skills/discord");
        fs::create_dir_all(&skills)?;
        fs::set_permissions(&skills, fs::Permissions::from_mode(0o500))?;
        let failed = install_tools(&config, &lib)
            .err()
            .context("installed into an unwritable directory")?;
        fs::set_permissions(&skills, fs::Permissions::from_mode(0o700))?;
        let message = format!("{failed:#}");
        assert!(message.contains("may be partly installed"), "{message}");
        assert!(
            message.contains("before starting Carillon core"),
            "{message}"
        );
        install_tools(&config, &lib)?;
        assert_eq!(
            fs::read_to_string(skills.join("SKILL.md"))?,
            "Discord skill\n"
        );
        assert_eq!(
            fs::read_to_string(resources.join("extensions/custom.js"))?,
            "the owner's own"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_module_declaration_changes_only_outside_other_maintenance() -> Result<()> {
        use super::{ModuleAction, manage_module};
        use carillon_installation::maintenance::MaintenanceLock;

        let directory = tempfile::tempdir()?;
        let mut config =
            crate::config::tests::installation(directory.path(), "discord-private", 46_500)?;
        // Nothing answers at the provider's address.
        let unused = tokio::net::TcpListener::bind("127.0.0.2:0").await?;
        config
            .value
            .providers
            .iter_mut()
            .find(|provider| provider.id == "discord-private")
            .context("missing provider")?
            .url = format!("http://{}", unused.local_addr()?);
        drop(unused);
        let module = directory.path().join("custom.wasm");
        crate::module::tests::write_module(
            &module,
            &crate::module::tests::describing(&[crate::module::tests::definition("model")]),
            &crate::module::tests::replying("custom", &[]),
        )?;
        let settings = config.provider_config_path("discord-private")?;
        let original = fs::read(&settings)?;
        let held = MaintenanceLock::acquire(&config.root)?;
        for action in [
            ModuleAction::Enable {
                file: Some(module.clone()),
            },
            ModuleAction::Disable,
        ] {
            let refused = manage_module(|| Ok(config.clone()), None, Some(action))
                .await
                .err()
                .context("changed the declaration during other maintenance")?;
            assert!(
                format!("{refused:#}").contains("another Carillon maintenance operation"),
                "{refused:#}"
            );
            assert_eq!(fs::read(&settings)?, original);
        }
        drop(held);
        // The declaration changes, and the lock is free again for the
        // provider's own change.
        manage_module(
            || Ok(config.clone()),
            None,
            Some(ModuleAction::Enable {
                file: Some(module.clone()),
            }),
        )
        .await?;
        let declared = crate::config::DiscordConfig::read(&config, "discord-private")?.module;
        assert!(declared.enabled);
        assert_eq!(declared.path, Some(module));
        assert!(MaintenanceLock::try_acquire(&config.root)?.is_some());
        manage_module(|| Ok(config.clone()), None, Some(ModuleAction::Disable)).await?;
        assert!(
            !crate::config::DiscordConfig::read(&config, "discord-private")?
                .module
                .enabled
        );
        Ok(())
    }

    /// An update's preflight runs the new release's program: a module the
    /// installed application ships is checked as the new release ships it,
    /// and any other module as it is.
    #[tokio::test]
    async fn a_declared_module_is_checked_where_the_update_takes_it_from() -> Result<()> {
        use crate::{
            config::DiscordConfig,
            module::tests::{definition, describing, module, replying},
        };

        let directory = tempfile::tempdir()?;
        let config = crate::config::tests::installation(
            &directory.path().join("installation"),
            "discord-private",
            46_700,
        )?;
        let release = directory.path().join("release");
        let shipped = Path::new("lib/examples/card-colours.wasm");
        for root in [config.root.join("app"), release.clone()] {
            fs::create_dir_all(root.join("lib/examples"))?;
        }
        let usable = module(&describing(&[definition("model")]), &replying("ok", &[]))?;
        let declare = |path: &Path| -> Result<DiscordConfig> {
            let mut discord = DiscordConfig::read(&config, "discord-private")?;
            discord.module.enabled = true;
            discord.module.path = Some(path.to_path_buf());
            Ok(discord)
        };
        let check = |discord: DiscordConfig| {
            let (config, release) = (config.clone(), release.clone());
            async move {
                super::check_declared(&config, "discord-private", &discord, &release)
                    .await
                    .map_err(|error| format!("{error:#}"))
            }
        };

        // The installed copy is the old release's; the new release's serves.
        let installed = config.root.join("app").join(shipped);
        fs::write(&installed, b"\0asm old contract")?;
        fs::write(release.join(shipped), &usable)?;
        check(declare(&installed)?)
            .await
            .map_err(anyhow::Error::msg)?;
        // A relative declaration starts at the installation, inside `app/` too.
        check(declare(&Path::new("app").join(shipped))?)
            .await
            .map_err(anyhow::Error::msg)?;

        // A usable installed copy does not hide a new release that cannot serve.
        fs::write(&installed, &usable)?;
        fs::write(release.join(shipped), b"\0asm broken")?;
        let refusal = check(declare(&installed)?).await.err().unwrap_or_default();
        assert!(refusal.contains("cannot serve"), "{refusal}");
        fs::remove_file(release.join(shipped))?;
        let refusal = check(declare(&installed)?).await.err().unwrap_or_default();
        assert!(refusal.contains("read Discord module"), "{refusal}");

        // A detour that stays inside the application is still the release's.
        fs::write(release.join(shipped), &usable)?;
        fs::write(&installed, b"\0asm old contract")?;
        check(declare(
            &config
                .root
                .join("app/lib/../lib/examples/card-colours.wasm"),
        )?)
        .await
        .map_err(anyhow::Error::msg)?;
        // One that leads out of it is the owner's file, never the release's
        // path spelt the same way.
        let modules = config.root.join("modules");
        fs::create_dir_all(&modules)?;
        fs::create_dir_all(directory.path().join("modules"))?;
        fs::write(modules.join("custom.wasm"), b"\0asm broken")?;
        fs::write(directory.path().join("modules/custom.wasm"), &usable)?;
        let escaping = Path::new("app/../modules/custom.wasm");
        let refusal = check(declare(escaping)?).await.err().unwrap_or_default();
        assert!(refusal.contains("cannot serve"), "{refusal}");
        fs::write(modules.join("custom.wasm"), &usable)?;
        check(declare(escaping)?)
            .await
            .map_err(anyhow::Error::msg)?;

        // A module of the owner's own is checked as it is, whatever the
        // release ships.
        fs::write(release.join(shipped), &usable)?;
        let own = directory.path().join("own.wasm");
        fs::write(&own, b"\0asm broken")?;
        let refusal = check(declare(&own)?).await.err().unwrap_or_default();
        assert!(refusal.contains("cannot serve"), "{refusal}");
        fs::write(&own, &usable)?;
        check(declare(&own)?).await.map_err(anyhow::Error::msg)?;
        Ok(())
    }

    #[test]
    fn only_an_unambiguous_discord_instance_is_inferred() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut config = crate::config::tests::installation(directory.path(), "discord-a", 46_300)?;
        assert_eq!(instance(&config, None)?, "discord-a");
        assert_eq!(instance(&config, Some("chosen".to_owned()))?, "chosen");

        // Another platform's interface has its own settings, not a Discord choice.
        let mut other = config.value.providers[0].clone();
        "slack".clone_into(&mut other.id);
        config.value.providers.push(other);
        fs::write(
            config.provider_config_path("slack")?,
            "workspace = \"T1\"\n",
        )?;
        assert_eq!(instance(&config, None)?, "discord-a");

        let second = Provider {
            id: "discord-b".to_owned(),
            role: ProviderRole::Interface,
            ..config.value.providers[0].clone()
        };
        config.value.providers.push(second);
        fs::copy(
            config.provider_config_path("discord-a")?,
            config.provider_config_path("discord-b")?,
        )?;
        let ambiguous = instance(&config, None)
            .err()
            .context("inferred one of several instances")?;
        assert!(
            ambiguous.to_string().ends_with("discord-a, discord-b"),
            "{ambiguous}"
        );

        for id in ["discord-a", "discord-b"] {
            fs::remove_file(config.provider_config_path(id)?)?;
        }
        let unconfigured = instance(&config, None)
            .err()
            .context("inferred an unconfigured instance")?;
        assert!(
            unconfigured
                .to_string()
                .ends_with("discord-a, slack, discord-b"),
            "{unconfigured}"
        );
        Ok(())
    }
}
