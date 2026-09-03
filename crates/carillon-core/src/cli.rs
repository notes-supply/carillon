//! The `carillon` operator command: service entry points, installation
//! actions, and the authenticated diagnostics client for a running core.

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use reqwest::Url;
use serde::{Serialize, de::DeserializeOwned};
use tracing_subscriber::EnvFilter;

use crate::{
    admin::{
        ServiceLock, backup, configure_discord, enable_demo_resources, initialize as init, restore,
        set_provider,
    },
    config::{DiscordRoute, LoadedConfig},
    core::serve,
    db::Database,
    discord,
    model::{StatusReport, TurnInspection, TurnSummary},
};

#[derive(Debug, Parser)]
#[command(name = "carillon", version, about = "Self-hosted agent service")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum ProviderAction {
    Enable,
    Disable,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ResolveAction {
    Retry,
    Skip,
}

#[derive(Debug, Subcommand)]
enum Command {
    Init {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        workspace: PathBuf,
        #[arg(long)]
        guild_id: String,
        #[arg(long)]
        bot_user_id: String,
        #[arg(long, required = true)]
        user_id: Vec<String>,
        #[arg(long, required = true)]
        route: Vec<DiscordRoute>,
        #[arg(long, default_value_t = 43_800)]
        base_port: u16,
    },
    ConfigureDiscord {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        guild_id: String,
        #[arg(long)]
        bot_user_id: String,
        #[arg(long, required = true)]
        user_id: Vec<String>,
        #[arg(long, required = true)]
        route: Vec<DiscordRoute>,
    },
    Core {
        #[arg(long)]
        config: PathBuf,
    },
    Discord {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        token_file: PathBuf,
        #[arg(long)]
        discord_token_file: Option<PathBuf>,
        #[arg(long)]
        deterministic: bool,
    },

    Provider {
        #[arg(long)]
        config: PathBuf,
        id: String,
        action: ProviderAction,
    },
    EnableDemoResources {
        #[arg(long)]
        config: PathBuf,
    },
    Status {
        #[arg(long)]
        config: PathBuf,
    },
    Recent {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        event_id: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    Inspect {
        #[arg(long)]
        config: PathBuf,
        turn_id: String,
    },
    Resolve {
        #[arg(long)]
        config: PathBuf,
        turn_id: String,
        action: ResolveAction,
    },

    Backup {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    Restore {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        target: PathBuf,
    },
}

fn initialize_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("carillon=info")),
        )
        .with_target(false)
        .init();
}

pub(crate) async fn run() -> Result<()> {
    initialize_tracing();
    match Cli::parse().command {
        Command::Init {
            root,
            workspace,
            guild_id,
            bot_user_id,
            user_id,
            route,
            base_port,
        } => {
            let path = init(
                &root,
                &workspace,
                &guild_id,
                &bot_user_id,
                &user_id,
                &route,
                base_port,
            )?;
            println!("{}", path.display());
        }
        Command::ConfigureDiscord {
            config,
            guild_id,
            bot_user_id,
            user_id,
            route,
        } => {
            configure_discord(&config, &guild_id, &bot_user_id, &user_id, &route)?;
            println!("{}", config.display());
        }
        Command::Core { config } => {
            let loaded = LoadedConfig::load(&config)?;
            let _lock = ServiceLock::acquire(&loaded.value.data_dir)?;
            serve(loaded).await?;
        }
        Command::Discord {
            config,
            token_file,
            discord_token_file,
            deterministic,
        } => {
            discord::serve(
                LoadedConfig::load(&config)?,
                &token_file,
                discord_token_file.as_deref(),
                deterministic,
            )
            .await?;
        }

        Command::Provider { config, id, action } => {
            set_provider(&config, &id, action == ProviderAction::Enable)?;
            let outcome = match action {
                ProviderAction::Enable => "enabled",
                ProviderAction::Disable => "disabled",
            };
            println!("{id} {outcome}");
        }
        Command::EnableDemoResources { config } => {
            enable_demo_resources(&LoadedConfig::load(&config)?)?;
            println!("Pi-native extension and Agent Skill enabled");
        }
        Command::Status { config } => print_status(&config).await?,
        Command::Recent {
            config,
            event_id,
            limit,
        } => print_recent(&config, event_id.as_deref(), limit).await?,
        Command::Inspect { config, turn_id } => print_inspection(&config, &turn_id).await?,
        Command::Resolve {
            config,
            turn_id,
            action,
        } => {
            let loaded = LoadedConfig::load(&config)?;
            let database = Database::open(&loaded.value.data_dir.join("carillon.sqlite3"))?;
            let outcome = match action {
                ResolveAction::Retry => {
                    database.retry_turn(&turn_id)?;
                    "retry"
                }
                ResolveAction::Skip => {
                    database.skip_turn(&turn_id)?;
                    "skip"
                }
            };
            println!("{turn_id} {outcome}");
        }

        Command::Backup { config, output } => {
            backup(&LoadedConfig::load(&config)?, &output)?;
            println!("{}", output.display());
        }
        Command::Restore { input, target } => {
            println!("{}", restore(&input, &target)?.display());
        }
    }
    Ok(())
}

async fn print_status(config: &Path) -> Result<()> {
    let loaded = LoadedConfig::load(config)?;
    let report: StatusReport = operator_get(&loaded, operator_url(&loaded, "/v1/status")?).await?;
    print_pretty(&report)
}

async fn print_recent(config: &Path, event_id: Option<&str>, limit: u32) -> Result<()> {
    let loaded = LoadedConfig::load(config)?;
    let mut url = operator_url(&loaded, "/v1/turns")?;
    url.query_pairs_mut()
        .append_pair("limit", &limit.to_string());
    if let Some(event_id) = event_id {
        url.query_pairs_mut().append_pair("event_id", event_id);
    }
    let turns: Vec<TurnSummary> = operator_get(&loaded, url).await?;
    print_pretty(&turns)
}

async fn print_inspection(config: &Path, turn_id: &str) -> Result<()> {
    let loaded = LoadedConfig::load(config)?;
    let inspection: TurnInspection = operator_get(
        &loaded,
        operator_url(&loaded, &format!("/v1/turns/{turn_id}"))?,
    )
    .await?;
    print_pretty(&inspection)
}

fn operator_url(config: &LoadedConfig, path: &str) -> Result<Url> {
    Ok(Url::parse(&format!(
        "http://{}{path}",
        config.value.core.listen
    ))?)
}

async fn operator_get<T: DeserializeOwned>(config: &LoadedConfig, url: Url) -> Result<T> {
    reqwest::Client::new()
        .get(url)
        .bearer_auth(config.credential("operator")?)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
        .map_err(Into::into)
}

fn print_pretty<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
