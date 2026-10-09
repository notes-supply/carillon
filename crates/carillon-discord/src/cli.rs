//! The `carillon-discord` command: the provider service and its operator
//! actions. Core's `carillon discord …` forwards here with the resolved
//! `CARILLON_HOME`; the service itself is started with explicit files.

use std::{
    fs,
    io::Read as _,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow, bail};
use carillon_installation::{
    config::{LoadedConfig, ProviderRole, RELEASE_VERSION},
    home, loopback,
};
use clap::{Parser, Subcommand};
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use tracing_subscriber::EnvFilter;

use crate::{
    application::{ServeOptions, serve},
    commands::{
        self, Change, CommandFiles, ModuleSummary, NextStart, Refusal, Registration, Saved, Source,
        Status,
    },
    config::{DiscordConfig, DiscordRoute, configure},
    operations::{
        self, ATTACHMENT_BYTES, ATTACHMENT_COUNT, Messages, Operation, OperationError, Upload,
    },
};

/// Status requests only read.
const STATUS_TIMEOUT: Duration = Duration::from_secs(10);
/// Changes may wait for Discord to confirm a registration.
const CHANGE_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Parser)]
#[command(
    name = "carillon-discord",
    version,
    about = "Carillon Discord interface provider",
    after_help = "Run as `carillon discord …`. Slash commands come from a replaceable command \
                  module; see `carillon discord commands --help`."
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
        /// The release's command module; an installed module takes precedence.
        #[arg(long)]
        commands: PathBuf,
        /// Serve local simulation endpoints instead of connecting to Discord.
        #[arg(long)]
        deterministic: bool,
    },
    /// Write the provider's admission settings, keeping tuned feedback settings.
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
    /// instance's Discord settings and its installed command module.
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
    /// Show which command module the running provider serves, or change it.
    #[command(after_help = "\
Without a subcommand, shows the serving module and its commands.

An installed module replaces the release's module without restarting the
provider or reconnecting its gateway, and keeps serving after restarts and
release updates. Changed command definitions are registered with Discord
before the module serves. Interactions already running finish on the module
they started with.")]
    Commands {
        /// Registry ID of the Discord instance; inferred when only one is configured.
        #[arg(long, global = true)]
        provider: Option<String>,
        #[command(subcommand)]
        action: Option<CommandsAction>,
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
enum CommandsAction {
    /// Check that FILE is a command module this program can serve, without
    /// serving it. Needs no installation or running provider.
    Check { file: PathBuf },
    /// Serve FILE in the running provider and keep it for later starts.
    Install { file: PathBuf },
    /// Serve the release's command module again and remove the installed one.
    Reset,
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
            commands,
            deterministic,
        } => {
            serve(
                LoadedConfig::load(&config)?,
                ServeOptions {
                    provider_id: &provider,
                    token_file: &token_file,
                    discord_token_file: discord_token_file.as_deref(),
                    commands: &commands,
                    deterministic,
                },
            )
            .await?;
        }
        Command::Configure {
            provider,
            guild_id,
            bot_user_id,
            user_id,
            route,
        } => {
            let loaded = installation()?;
            let provider = instance(&loaded, provider)?;
            configure(
                &loaded,
                &provider,
                &guild_id,
                &bot_user_id,
                &user_id,
                &route,
            )?;
            println!("{}", loaded.provider_config_path(&provider)?.display());
        }
        Command::Check { provider } => {
            let loaded = installation()?;
            let provider = instance(&loaded, provider)?;
            DiscordConfig::load(&loaded, &provider).with_context(|| {
                format!("carillon-discord {RELEASE_VERSION} cannot use the settings of {provider}")
            })?;
            println!(
                "{} is compatible with carillon-discord {}",
                loaded.provider_config_path(&provider)?.display(),
                RELEASE_VERSION
            );
            check_installed(&loaded, &provider).await?;
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
        Command::Commands { provider, action } => {
            manage_commands(installation, provider, action).await?;
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
    let result: serde_json::Value = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "{}: invalid Discord provider response; inspect the target before repeating a write",
            if mutates { "uncertain" } else { "unavailable" }
        )
    })?;
    if !status.is_success() {
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
                .unwrap_or("the Discord provider refused the request")
        );
    }
    Ok(result)
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
    let executable = std::env::current_exe()?.canonicalize()?;
    Ok(executable
        .parent()
        .and_then(Path::parent)
        .context("cannot locate release library")?
        .join("lib"))
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
        maintenance::{MaintenanceLock, ServiceLock},
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
            "no interface provider has Discord settings; choose one with --provider from: {}",
            interfaces.join(", ")
        ),
        several => bail!(
            "several Discord instances are configured; choose one with --provider from: {}",
            several.join(", ")
        ),
    }
}

/// `carillon discord commands …`: a file check needs no installation; the
/// rest ask the running provider, which owns the module and its changes.
async fn manage_commands(
    installation: impl FnOnce() -> Result<LoadedConfig>,
    provider: Option<String>,
    action: Option<CommandsAction>,
) -> Result<()> {
    let (method, body, requested) = match action {
        Some(CommandsAction::Check { file }) => {
            if let Some(provider) = provider {
                bail!("`commands check` examines a file and does not use --provider {provider}");
            }
            let module = check_file(&file).await?;
            println!(
                "{} is a usable command module for carillon-discord {RELEASE_VERSION}: {}.",
                file.display(),
                list(&module)
            );
            return Ok(());
        }
        None => (Method::GET, None, None),
        Some(CommandsAction::Install { file }) => {
            let bytes = fs::read(&file)
                .with_context(|| format!("read command module {}", file.display()))?;
            (Method::PUT, Some(bytes), Some(Source::Installed))
        }
        Some(CommandsAction::Reset) => (Method::DELETE, None, Some(Source::Release)),
    };
    let loaded = installation()?;
    let provider = instance(&loaded, provider)?;
    let operator = Operator::new(&loaded, &provider)?;
    match requested {
        None => {
            print_status(&provider, &operator.request(method, body).await?);
            Ok(())
        }
        Some(requested) => {
            report_change(&provider, requested, &operator.request(method, body).await?)
        }
    }
}

/// Validates an installed module against this program, as an update's
/// preflight does before it stops the services.
async fn check_installed(loaded: &LoadedConfig, provider: &str) -> Result<()> {
    let installed = CommandFiles::of(loaded, provider, PathBuf::new()).installed;
    if !installed.exists() {
        return Ok(());
    }
    let module = check_file(&installed).await.with_context(|| {
        format!(
            "{provider} has an installed command module this release cannot serve. Run \
             `carillon discord commands reset` (or remove the file while the provider is \
             stopped), then install a module built for this release"
        )
    })?;
    println!(
        "installed command module {} is usable by carillon-discord {RELEASE_VERSION}: {}.",
        installed.display(),
        list(&module)
    );
    Ok(())
}

async fn check_file(file: &Path) -> Result<ModuleSummary> {
    let bytes =
        fs::read(file).with_context(|| format!("read command module {}", file.display()))?;
    commands::check(&bytes).await.with_context(|| {
        format!(
            "{} is not a usable command module for carillon-discord {RELEASE_VERSION}",
            file.display()
        )
    })
}

/// The running provider's operator endpoint for the command module.
struct Operator {
    url: String,
    credential: String,
}

impl Operator {
    fn new(loaded: &LoadedConfig, provider: &str) -> Result<Self> {
        let target = loaded.provider(provider, ProviderRole::Interface)?;
        Ok(Self {
            url: format!("{}/v1/commands", target.url),
            credential: loaded.credential("operator")?,
        })
    }

    /// The provider's answer, or its refusal rendered with what still serves.
    async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        body: Option<Vec<u8>>,
    ) -> Result<T> {
        let changing = method != Method::GET;
        let mut request = loopback::client()
            .build()?
            .request(method, &self.url)
            .bearer_auth(&self.credential)
            .timeout(if changing {
                CHANGE_TIMEOUT
            } else {
                STATUS_TIMEOUT
            });
        if let Some(body) = body {
            request = request.body(body);
        }
        let response = request
            .send()
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
                "{error}.\nStill serving the {} command module: {}.",
                serving.source,
                list(&serving)
            );
        }
        let message = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|body| body["error"].as_str().map(str::to_owned))
            .unwrap_or_else(|| String::from_utf8_lossy(&bytes).trim().to_owned());
        if status == StatusCode::NOT_FOUND {
            bail!(
                "the Discord provider at {} does not offer command module operations ({message}); \
                 it may be a different release than this program",
                self.url
            );
        }
        bail!("the Discord provider refused the request ({status}): {message}")
    }

    /// A transport failure establishes only that the provider did not
    /// answer, not that it is stopped or that a change did not happen.
    fn unreachable(&self, error: &reqwest::Error, changing: bool) -> anyhow::Error {
        if error.is_timeout() {
            let outcome = if changing {
                " The change may still complete; check with `carillon discord commands`."
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
                    " Whether the change happened is unknown; check with `carillon discord commands`."
                } else {
                    ""
                }
            )
        }
    }
}

fn list(module: &ModuleSummary) -> String {
    if module.commands.is_empty() {
        return "no commands".to_owned();
    }
    module
        .commands
        .iter()
        .map(|name| format!("/{name}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn print_status(provider: &str, status: &Status) {
    let Status {
        serving,
        next_start,
        registered,
    } = status;
    println!(
        "{provider} serves the {} command module: {}.",
        serving.source,
        list(serving)
    );
    match next_start {
        NextStart::Same => {}
        NextStart::Different { source } => println!(
            "The {source} module on disk is not the one serving, so a restart would select it; \
             the last install or reset may not have finished saving. Run it again."
        ),
        NextStart::Unknown { error } => {
            println!("The module for the next start cannot be read: {error}.");
        }
    }
    if *registered == Some(false) {
        println!(
            "Discord's registration of these commands is not confirmed; the next install, \
             reset or provider restart registers them."
        );
    }
}

/// Short success output; an unconfirmed save is an error the operator must
/// finish by repeating the command.
fn report_change(provider: &str, requested: Source, change: &Change) -> Result<()> {
    let Change {
        serving,
        previous,
        discord,
        saved,
    } = change;
    if previous == serving {
        println!(
            "{provider} was already serving the {} command module: {}.",
            serving.source,
            list(serving)
        );
    } else {
        println!(
            "{provider} now serves the {} command module: {}.",
            serving.source,
            list(serving)
        );
    }
    if *discord == Registration::Registered {
        println!("Registered its commands with Discord.");
    }
    let again = match requested {
        Source::Installed => "Run the same install again",
        Source::Release => "Run `carillon discord commands reset` again",
    };
    match (saved, requested) {
        (Saved::Durable, _) => Ok(()),
        (Saved::Failed { error }, Source::Installed) => bail!(
            "it serves now, but saving it failed: {error}. The saved state on disk is \
             unchanged; restart selection still depends on that saved state. {again}."
        ),
        (Saved::Failed { error }, Source::Release) => bail!(
            "it serves now, but saving the reset failed: {error}. The saved state on disk is \
             unchanged; restart selection still depends on that saved state. {again}."
        ),
        (Saved::Unsynced { error }, Source::Installed) => bail!(
            "it serves now and is written to disk, but the write is not confirmed durable: \
             {error}. A system crash may undo it. {again}."
        ),
        (Saved::Unsynced { error }, Source::Release) => bail!(
            "it serves now and the installed module is removed from disk, but the removal is \
             not confirmed durable: {error}. A system crash may undo it. {again}."
        ),
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

    #[test]
    fn failed_saves_do_not_infer_restart_selection() -> Result<()> {
        use crate::commands::{Change, ModuleSummary, Registration, Saved, Source};

        // Retrying may request the same bytes already on disk, and a reset
        // may fail before it even inspects an absent override. Neither error
        // alone establishes what a restart would load.
        for source in [Source::Installed, Source::Release] {
            let summary = ModuleSummary {
                source,
                commands: vec!["model".to_owned()],
                digest: "same-module".to_owned(),
            };
            let change = Change {
                serving: summary.clone(),
                previous: summary,
                discord: Registration::Unchanged,
                saved: Saved::Failed {
                    error: "directory sync failed".to_owned(),
                },
            };
            let message = super::report_change("discord", source, &change)
                .err()
                .context("a failed save must exit with an error")?
                .to_string();
            assert!(message.contains("saved state on disk is unchanged"));
            assert!(message.contains("restart selection still depends on that saved state"));
            assert!(message.contains("again"));
        }
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
