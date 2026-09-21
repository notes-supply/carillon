//! The `carillon-discord` command: the provider service and its operator
//! actions. Core's `carillon discord …` forwards here.

use std::path::PathBuf;

use anyhow::{Context as _, Result, bail};
use carillon_protocol::config::{LoadedConfig, ProviderRole, read_token_file};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::{
    commands::{Commands, DiscordApp},
    config::{DiscordConfig, DiscordRoute, configure},
    provider::{ServeOptions, serve},
};

#[derive(Debug, Parser)]
#[command(
    name = "carillon-discord",
    version,
    about = "Carillon Discord interface provider"
)]
struct Cli {
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
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        provider: String,
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
        config: PathBuf,
        #[arg(long)]
        provider: String,
        #[arg(long)]
        discord_token_file: PathBuf,
        #[arg(long)]
        commands: PathBuf,
    },
    /// Ask the running provider to reload its command module.
    Reload {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        provider: String,
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
    match Cli::parse().command {
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
            config,
            provider,
            guild_id,
            bot_user_id,
            user_id,
            route,
        } => {
            let loaded = LoadedConfig::load(&config)?;
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
            config,
            provider,
            discord_token_file,
            commands,
        } => {
            let count = register(&config, &provider, &discord_token_file, &commands).await?;
            println!("{count} commands registered");
        }
        Command::Reload { config, provider } => {
            let loaded = LoadedConfig::load(&config)?;
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

async fn register(
    config: &std::path::Path,
    provider_id: &str,
    discord_token_file: &std::path::Path,
    module: &std::path::Path,
) -> Result<usize> {
    let loaded = LoadedConfig::load(config)?;
    let provider = loaded.provider(provider_id, ProviderRole::Interface)?;
    let discord = DiscordConfig::load(&loaded, provider_id)?;
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
