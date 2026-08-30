use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::{
    components::{serve_capability, serve_discord, serve_extension},
    config::{DiscordRoute, LoadedConfig},
    core::{
        fetch_inspection, fetch_models, fetch_recent, fetch_status, select_runtime_model,
        serialize_pretty, serve,
    },
    model::RuntimeModelSelection,
    operations::{
        ServiceLock, backup, configure_discord, enable_demo_resources, initialize, restore,
        set_component,
    },
};

#[derive(Debug, Parser)]
#[command(
    name = "carillon",
    version,
    about = "Durable private agent composition service"
)]
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
    InterfaceExtension {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        token_file: PathBuf,
    },
    Capability {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        token_file: PathBuf,
    },
    Component {
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
    Model {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        refresh: bool,
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

pub(crate) async fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("carillon=info")),
        )
        .with_target(false)
        .init();
    match Cli::parse().command {
        Command::Init {
            root,
            guild_id,
            bot_user_id,
            user_id,
            route,
            base_port,
        } => {
            let path = initialize(&root, &guild_id, &bot_user_id, &user_id, &route, base_port)?;
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
        Command::InterfaceExtension { config, token_file } => {
            serve_extension(LoadedConfig::load(&config)?, &token_file).await?;
        }
        Command::Capability { config, token_file } => {
            serve_capability(LoadedConfig::load(&config)?, &token_file).await?;
        }
        Command::Component { config, id, action } => {
            set_component(&config, &id, action == "enable")?;
            println!("{id} {action}d");
        }
        Command::EnableDemoResources { config } => enable_demo_resources_command(&config)?,
        Command::Status { config } => {
            println!(
                "{}",
                serialize_pretty(&fetch_status(&LoadedConfig::load(&config)?).await?)?
            );
        }
        Command::Recent {
            config,
            event_id,
            limit,
        } => {
            println!(
                "{}",
                serialize_pretty(
                    &fetch_recent(&LoadedConfig::load(&config)?, event_id.as_deref(), limit)
                        .await?
                )?
            );
        }
        Command::Inspect { config, turn_id } => {
            println!(
                "{}",
                serialize_pretty(
                    &fetch_inspection(&LoadedConfig::load(&config)?, &turn_id).await?
                )?
            );
        }
        Command::Model {
            config,
            refresh,
            selection,
        } => run_model_command(config, refresh, selection).await?,
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

async fn run_model_command(
    config: PathBuf,
    refresh: bool,
    selection: Option<String>,
) -> Result<()> {
    let config = LoadedConfig::load(&config)?;
    let models = fetch_models(&config, refresh).await?;
    let models = if let Some(reference) = selection {
        let model = models
            .models
            .iter()
            .find(|model| format!("{}/{}", model.provider, model.id) == reference)
            .with_context(|| format!("model {reference} is not available"))?;
        select_runtime_model(
            &config,
            &RuntimeModelSelection {
                key: model.key.clone(),
            },
        )
        .await?
    } else {
        models
    };
    println!("{}", serialize_pretty(&models)?);
    Ok(())
}
