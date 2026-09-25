//! The `carillon-discord` command: the provider service and its operator
//! actions. Core's `carillon discord …` forwards here with the resolved
//! `CARILLON_HOME`; the service itself is started with explicit files.

use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow, bail};
use carillon_installation::{
    config::{LoadedConfig, ProviderRole, RELEASE_VERSION},
    home,
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
        Command::Commands { provider, action } => {
            manage_commands(installation, provider, action).await?;
        }
    }
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
        let mut request = reqwest::Client::new()
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
             unchanged, so a restart would not serve this module. {again}."
        ),
        (Saved::Failed { error }, Source::Release) => bail!(
            "it serves now, but removing the installed module failed: {error}. The installed \
             module is still on disk, so a restart would select it again. {again}."
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
    use std::fs;

    use anyhow::{Context as _, Result};
    use carillon_installation::config::{Provider, ProviderRole};

    use super::instance;

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
