use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::{
    components::{serve_capability, serve_discord, serve_extension},
    config::LoadedConfig,
    core::{fetch_inspection, fetch_recent, fetch_status, serialize_pretty, serve},
    operations::{ServiceLock, backup, enable_demo_resources, initialize, restore, set_component},
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
        #[arg(long, required = true)]
        user_id: Vec<String>,
        #[arg(long, default_value_t = 43_800)]
        base_port: u16,
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
            user_id,
            base_port,
        } => {
            let path = initialize(&root, &guild_id, &user_id, base_port)?;
            println!("{}", path.display());
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
        Command::EnableDemoResources { config } => {
            enable_demo_resources(&LoadedConfig::load(&config)?)?;
            println!("Pi-native extension and Agent Skill enabled");
        }
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
