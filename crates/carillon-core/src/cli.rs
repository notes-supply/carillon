use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::{
    components::serve_discord,
    config::{DiscordRoute, LoadedConfig},
    core::{
        fetch_execution_profiles, fetch_inspection, fetch_recent, fetch_status,
        select_execution_profile, serialize_pretty, serve,
    },
    db::Database,
    model::{ExecutionProfileScope, ExecutionProfileSelection, ServiceTier},
    operations::{
        ServiceLock, backup, configure_discord, enable_demo_resources, initialize as init, restore,
        set_provider,
    },
};

#[derive(Debug, Parser)]
#[command(name = "carillon", version, about = "Self-hosted agent service")]
struct Cli {
    #[command(subcommand)]
    command: Command,
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
        #[arg(value_parser = ["enable", "disable"])]
        action: String,
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
        #[arg(value_parser = ["retry", "skip"])]
        action: String,
    },
    Model {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        reasoning: Option<String>,
        #[arg(long)]
        service_tier: Option<ServiceTier>,
        #[arg(
            long,
            value_parser = ["current-conversation", "new-conversations"],
            default_value = "new-conversations"
        )]
        scope: String,
        #[arg(long)]
        audience: Option<String>,
        #[arg(long)]
        interface: Option<String>,
        selection: Option<String>,
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
        } => configure_discord_command(&config, &guild_id, &bot_user_id, &user_id, &route)?,
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
            serve_discord(
                LoadedConfig::load(&config)?,
                &token_file,
                discord_token_file.as_deref(),
                deterministic,
            )
            .await?;
        }

        Command::Provider { config, id, action } => {
            set_provider(&config, &id, action == "enable")?;
            println!("{id} {action}d");
        }
        Command::EnableDemoResources { config } => enable_demo_resources_command(&config)?,
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
            Database::open(&loaded.value.data_dir.join("carillon.sqlite3"))?
                .resolve_uncertain_turn(&turn_id, action == "retry")?;
            println!("{turn_id} {action}");
        }
        command @ Command::Model { .. } => run_model_command(command).await?,
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
    println!(
        "{}",
        serialize_pretty(&fetch_status(&LoadedConfig::load(config)?).await?)?
    );
    Ok(())
}

async fn print_recent(config: &Path, event_id: Option<&str>, limit: u32) -> Result<()> {
    println!(
        "{}",
        serialize_pretty(&fetch_recent(&LoadedConfig::load(config)?, event_id, limit).await?)?
    );
    Ok(())
}

async fn print_inspection(config: &Path, turn_id: &str) -> Result<()> {
    println!(
        "{}",
        serialize_pretty(&fetch_inspection(&LoadedConfig::load(config)?, turn_id).await?)?
    );
    Ok(())
}

fn enable_demo_resources_command(config: &Path) -> Result<()> {
    enable_demo_resources(&LoadedConfig::load(config)?)?;
    println!("Pi-native extension and Agent Skill enabled");
    Ok(())
}

fn configure_discord_command(
    config: &Path,
    guild_id: &str,
    bot_user_id: &str,
    user_id: &[String],
    route: &[DiscordRoute],
) -> Result<()> {
    configure_discord(config, guild_id, bot_user_id, user_id, route)?;
    println!("{}", config.display());
    Ok(())
}

async fn run_model_command(command: Command) -> Result<()> {
    let Command::Model {
        config,
        refresh,
        reasoning,
        service_tier,
        scope,
        audience: audience_id,
        interface,
        selection,
    } = command
    else {
        unreachable!("model command dispatcher passed another command");
    };
    let config = LoadedConfig::load(&config)?;
    let scope = if scope == "current-conversation" {
        ExecutionProfileScope::CurrentConversation
    } else {
        ExecutionProfileScope::NewConversations
    };
    let current = matches!(scope, ExecutionProfileScope::CurrentConversation);
    let interface_id = if current {
        Some(
            interface
                .or_else(|| config.value.sole_enabled_interface().map(str::to_owned))
                .context(
                    "current conversation scope needs --interface when multiple interfaces are enabled",
                )?,
        )
    } else {
        None
    };
    let audience_id = if current {
        Some(
            audience_id
                .as_deref()
                .filter(|value| !value.is_empty())
                .context("current conversation scope needs --audience")?
                .to_owned(),
        )
    } else {
        None
    };
    let profiles = fetch_execution_profiles(
        &config,
        refresh,
        interface_id.as_deref(),
        audience_id.as_deref(),
    )
    .await?;
    let Some(reference) = selection else {
        println!("{}", serialize_pretty(&profiles)?);
        return Ok(());
    };
    let model = profiles
        .models
        .iter()
        .find(|model| format!("{}/{}", model.provider, model.id) == reference)
        .with_context(|| format!("model {reference} is not available"))?;
    let applied = select_execution_profile(
        &config,
        &ExecutionProfileSelection {
            key: model.key.clone(),
            reasoning,
            service_tier,
            scope,
            audience_id,
            interface_id,
        },
    )
    .await?;
    println!("{}", serialize_pretty(&applied)?);
    Ok(())
}
