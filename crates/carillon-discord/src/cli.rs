//! The `carillon-discord` command: the provider service and its operator
//! actions. Core's `carillon discord …` forwards here with the resolved
//! `CARILLON_HOME`; the service itself is started with explicit files.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use carillon_installation::{
    config::{LoadedConfig, ProviderRole, RELEASE_VERSION, read_token_file},
    home,
};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::{
    application::{ServeOptions, serve},
    commands::{Commands, DiscordApp},
    config::{DiscordConfig, DiscordRoute, configure},
};

#[derive(Debug, Parser)]
#[command(
    name = "carillon-discord",
    version,
    about = "Carillon Discord interface provider"
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
    /// Run the provider.
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
        /// The command module (`carillon_discord_commands.wasm`).
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
    /// Register the command module's commands with Discord without serving.
    Register {
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        discord_token_file: PathBuf,
        #[arg(long)]
        commands: PathBuf,
    },
    /// Check, without changing anything, that this program accepts the
    /// instance's Discord settings.
    Check {
        #[arg(long)]
        provider: Option<String>,
    },
    /// Ask the running provider to reload its command module.
    Reload {
        #[arg(long)]
        provider: Option<String>,
    },
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
        Command::Register {
            provider,
            discord_token_file,
            commands,
        } => {
            let loaded = installation()?;
            let provider = instance(&loaded, provider)?;
            let count = register(&loaded, &provider, &discord_token_file, &commands).await?;
            println!("{count} commands registered");
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
        }
        Command::Reload { provider } => {
            let loaded = installation()?;
            let provider = instance(&loaded, provider)?;
            let target = loaded.provider(&provider, ProviderRole::Interface)?;
            let response = reqwest::Client::new()
                .post(format!("{}/v1/commands/reload", target.url))
                .bearer_auth(loaded.credential("operator")?)
                .timeout(std::time::Duration::from_secs(30))
                .send()
                .await?;
            let status = response.status();
            let body: serde_json::Value = response.json().await?;
            if !status.is_success() {
                bail!(
                    "reload failed: {}",
                    body["error"].as_str().unwrap_or("unexpected response")
                );
            }
            println!("{}", serde_json::to_string_pretty(&body)?);
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

async fn register(
    loaded: &LoadedConfig,
    provider_id: &str,
    discord_token_file: &Path,
    module: &Path,
) -> Result<usize> {
    let provider = loaded.provider(provider_id, ProviderRole::Interface)?;
    let discord = DiscordConfig::load(loaded, provider_id)?;
    let record = loaded
        .value
        .data_dir
        .join("components")
        .join(&provider.id)
        .join("commands.json");
    let commands = Commands::load(module.to_path_buf(), record).await?;
    let http = std::sync::Arc::new(twilight_http::Client::new(read_token_file(
        discord_token_file,
    )?));
    let current_user = http.current_user().await?.model().await?;
    if current_user.id.to_string() != discord.bot_user_id {
        bail!("configured Discord bot user ID does not match the token");
    }
    let application_id = http
        .current_user_application()
        .await
        .context("resolve Discord application")?
        .model()
        .await
        .context("decode Discord application")?
        .id;
    let app = DiscordApp {
        http,
        application_id,
        guild_id: discord
            .guild_id
            .parse()
            .context("configured Discord guild ID is invalid")?,
    };
    let definitions = commands.definitions();
    commands.register(&app, &definitions).await?;
    Ok(definitions.len())
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
