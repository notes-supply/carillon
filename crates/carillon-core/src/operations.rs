use std::{
    fs,
    io::{BufReader, BufWriter, Write as _},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use fs2::FileExt;
use tar::{Archive, Builder};
use uuid::Uuid;

use crate::{
    config::{
        AgentConfig, Config, CoreConfig, DiscordConfig, DiscordRoute, LoadedConfig, Provider,
        ProviderRole, RELEASE_VERSION, hash_token, new_token, validate_provider_identity,
    },
    db::Database,
};

const DEFAULT_SYSTEM: &str = r"You are Carillon, a capable general-purpose agent.

Help the user understand, decide, create, and act. Think independently, speak plainly, and recommend the best course when one is clear. Identify flawed assumptions and important consequences.

Use available tools when useful. Act on clear, sufficiently authorized requests; ask before consequential actions whose scope is unclear. Prefer the smallest complete solution. Be honest about uncertainty and failure, and never claim unverified success.
";
const DEFAULT_CONTEXT: &str = r"Use only capabilities and resources available in the current environment.

Lead with the result, decision, blocker, or required action. Keep operational narration proportional.

Never expose credentials or place secrets in messages, prompts, sessions, ordinary logs, or commands.

Treat each conversation according to context supplied by its interface. Do not import assumptions from unrelated conversations.
";
const NATIVE_EXTENSION: &str = r#"export default function carillonContext(pi) {
  pi.on("before_agent_start", async (event) => ({
    systemPrompt: `${event.systemPrompt}\n\nCarillon native extension active: preserve audience and turn continuity.`,
  }));
}
"#;
const SKILL: &str = "---\nname: carillon-status\ndescription: Explain Carillon component and continuity status when the operator asks.\n---\n\nReport component identity, resource selection, and delivery certainty plainly.\n";

pub struct ServiceLock {
    _file: fs::File,
}

impl ServiceLock {
    pub fn acquire(data_dir: &Path) -> Result<Self> {
        fs::create_dir_all(data_dir)?;
        let path = data_dir.join("service.lock");
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;
        file.try_lock_exclusive()
            .with_context(|| format!("Carillon core already owns {}", path.display()))?;
        Ok(Self { _file: file })
    }
}

pub fn initialize(
    root: &Path,
    workspace: &Path,
    guild_id: &str,
    bot_user_id: &str,
    users: &[String],
    routes: &[DiscordRoute],
    base_port: u16,
) -> Result<PathBuf> {
    if guild_id.is_empty() || bot_user_id.is_empty() || users.is_empty() || routes.is_empty() {
        bail!("Discord guild, bot, user, and channel route IDs are required");
    }
    if root.exists() && fs::read_dir(root)?.next().is_some() {
        bail!("installation root {} is not empty", root.display());
    }
    create_installation_directories(root)?;
    let discord_token = new_token();
    let operator_token = new_token();
    let runtime_token = new_token();

    let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let config = Config {
        release: RELEASE_VERSION.to_owned(),
        data_dir: PathBuf::from("state"),
        core: CoreConfig {
            listen: SocketAddr::new(loopback, base_port),
            operator_token_hash: hash_token(&operator_token),
        },
        providers: default_providers(base_port, &discord_token, &runtime_token)?,
        discord: Some(DiscordConfig {
            provider: "discord-private".to_owned(),
            agent: "default".to_owned(),
            guild_id: guild_id.to_owned(),
            bot_user_id: bot_user_id.to_owned(),
            allowed_user_ids: users.to_vec(),
            routes: routes.to_vec(),
            typing: true,
            reactions: true,
            processing_emoji: "👀".to_owned(),
            delivered_emoji: "✅".to_owned(),
            failed_emoji: "❌".to_owned(),
            uncertain_emoji: "⚠️".to_owned(),
        }),
        agent: AgentConfig {
            id: "default".to_owned(),
            name: "Default agent".to_owned(),
            resource_root: PathBuf::from("agents/default"),
            workspace: workspace.to_path_buf(),
            runtime_provider: "runtime-pi".to_owned(),
            interface_extensions: Vec::new(),
            capability_provider: None,
        },
    };
    config.validate()?;
    config.discord()?;
    let config_path = root.join("carillon.toml");
    atomic_write(
        &config_path,
        toml::to_string_pretty(&config)?.as_bytes(),
        0o600,
    )?;
    write_credentials(
        root,
        &[
            ("operator", &operator_token),
            ("discord-private", &discord_token),
            ("runtime-pi", &runtime_token),
        ],
    )?;
    let agent = root.join("agents/default");
    atomic_write(&agent.join("SYSTEM.md"), DEFAULT_SYSTEM.as_bytes(), 0o640)?;
    atomic_write(&agent.join("AGENTS.md"), DEFAULT_CONTEXT.as_bytes(), 0o640)?;
    atomic_write(&agent.join("settings.json"), b"{}\n", 0o640)?;
    atomic_write(
        &root.join("examples/carillon-context.js"),
        NATIVE_EXTENSION.as_bytes(),
        0o640,
    )?;
    atomic_write(&root.join("examples/SKILL.md"), SKILL.as_bytes(), 0o640)?;
    Database::open(&root.join("state/carillon.sqlite3"))?;
    Ok(config_path)
}

fn default_providers(
    base_port: u16,
    discord_token: &str,
    runtime_token: &str,
) -> Result<Vec<Provider>> {
    Ok(vec![
        Provider {
            id: "discord-private".to_owned(),
            role: ProviderRole::Interface,
            version: RELEASE_VERSION.to_owned(),
            url: format!("http://127.0.0.1:{}", checked_port(base_port, 1)?),
            token_hash: hash_token(discord_token),
            enabled: true,
        },
        Provider {
            id: "runtime-pi".to_owned(),
            role: ProviderRole::Runtime,
            version: RELEASE_VERSION.to_owned(),
            url: format!("http://127.0.0.1:{}", checked_port(base_port, 2)?),
            token_hash: hash_token(runtime_token),
            enabled: true,
        },
    ])
}

fn create_installation_directories(root: &Path) -> Result<()> {
    fs::create_dir_all(root)?;
    fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
    for directory in [
        "state",
        "credentials",
        "pi",
        "logs",
        "state/components/discord-private",
        "state/components/runtime-pi",
        "agents/default/extensions",
        "agents/default/skills",
        "agents/default/sessions",
        "examples",
    ] {
        let path = root.join(directory);
        fs::create_dir_all(&path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn write_credentials(root: &Path, credentials: &[(&str, &str)]) -> Result<()> {
    for (id, token) in credentials {
        atomic_write(
            &root.join("credentials").join(format!("{id}.token")),
            format!("{token}\n").as_bytes(),
            0o600,
        )?;
    }
    Ok(())
}

pub fn set_provider(config_path: &Path, id: &str, enabled: bool) -> Result<()> {
    validate_provider_identity(id)?;
    let text = fs::read_to_string(config_path)?;
    let mut config: Config = toml::from_str(&text)?;
    let provider = config
        .providers
        .iter_mut()
        .find(|provider| provider.id == id)
        .with_context(|| format!("provider {id} is not configured"))?;
    provider.enabled = enabled;
    config.validate()?;
    write_config(config_path, &config)
}

pub fn configure_discord(
    config_path: &Path,
    guild_id: &str,
    bot_user_id: &str,
    users: &[String],
    routes: &[DiscordRoute],
) -> Result<()> {
    let text = fs::read_to_string(config_path)?;
    let mut config: Config = toml::from_str(&text)?;
    let discord = config
        .discord
        .as_mut()
        .context("Discord interface configuration is absent")?;
    guild_id.clone_into(&mut discord.guild_id);
    bot_user_id.clone_into(&mut discord.bot_user_id);
    discord.allowed_user_ids = users.to_vec();
    discord.routes = routes.to_vec();
    config.validate()?;
    config.discord()?;
    write_config(config_path, &config)
}

fn write_config(config_path: &Path, config: &Config) -> Result<()> {
    let mode = fs::metadata(config_path)?.permissions().mode() & 0o777;
    atomic_write(
        config_path,
        toml::to_string_pretty(config)?.as_bytes(),
        mode,
    )
}

pub fn enable_demo_resources(config: &LoadedConfig) -> Result<()> {
    let extension = config.root.join("examples/carillon-context.js");
    let skill = config.root.join("examples/SKILL.md");
    if !extension.exists() || !skill.exists() {
        bail!("installation does not contain the first-release example resources");
    }
    let root = &config.value.agent.resource_root;
    fs::create_dir_all(root.join("extensions"))?;
    fs::create_dir_all(root.join("skills/carillon-status"))?;
    fs::copy(extension, root.join("extensions/carillon-context.js"))?;
    fs::copy(skill, root.join("skills/carillon-status/SKILL.md"))?;
    Ok(())
}

pub fn backup(config: &LoadedConfig, destination: &Path) -> Result<()> {
    let _lock = ensure_stopped(config)?;
    let destination = absolute_destination(destination)?;
    if destination.starts_with(&config.root) {
        bail!("backup destination must be outside the installation root");
    }
    if destination.exists() {
        bail!(
            "backup destination {} already exists",
            destination.display()
        );
    }
    let database = Database::open(&config.value.data_dir.join("carillon.sqlite3"))?;
    let file_name = destination
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("carillon-backup");
    let temporary = destination.with_file_name(format!(".{file_name}.partial-{}", Uuid::now_v7()));
    if let Err(failure) = write_backup_archive(&config.root, &temporary) {
        let _ = fs::remove_file(&temporary);
        return Err(failure);
    }
    if let Err(failure) = fs::rename(&temporary, &destination) {
        let _ = fs::remove_file(&temporary);
        return Err(failure.into());
    }
    if let Err(failure) = database.record_backup(&destination) {
        let _ = fs::remove_file(&destination);
        return Err(failure);
    }
    Ok(())
}

fn write_backup_archive(root: &Path, destination: &Path) -> Result<()> {
    let file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(destination)?;
    let encoder = GzEncoder::new(BufWriter::new(file), Compression::default());
    let mut archive = Builder::new(encoder);
    append_directory(&mut archive, root, root)?;
    archive.finish()?;
    let encoder = archive.into_inner()?;
    let mut writer = encoder.finish()?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    Ok(())
}

pub fn restore(archive_path: &Path, target: &Path) -> Result<PathBuf> {
    if target.exists() && fs::read_dir(target)?.next().is_some() {
        bail!("restore target {} is not empty", target.display());
    }
    fs::create_dir_all(target)?;
    fs::set_permissions(target, fs::Permissions::from_mode(0o700))?;
    let file = fs::File::open(archive_path)
        .with_context(|| format!("open backup {}", archive_path.display()))?;
    let decoder = GzDecoder::new(BufReader::new(file));
    let mut archive = Archive::new(decoder);
    archive.set_preserve_permissions(true);
    archive.set_preserve_ownerships(true);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?;
        if path.is_absolute()
            || path
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            bail!("backup contains unsafe path {}", path.display());
        }
        entry.unpack_in(target)?;
    }
    let config = target.join("carillon.toml");
    LoadedConfig::load(&config).context("restored backup is not a valid Carillon installation")?;
    Ok(config)
}

fn ensure_stopped(config: &LoadedConfig) -> Result<ServiceLock> {
    let lock = ServiceLock::acquire(&config.value.data_dir)?;
    let mut addresses = vec![config.value.core.listen];
    addresses.extend(
        config
            .value
            .providers
            .iter()
            .map(|provider| component_address(&provider.url))
            .collect::<Result<Vec<_>>>()?,
    );
    for address in addresses {
        if TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_ok() {
            bail!(
                "service at {address} is still running; stop every Carillon provider before backup"
            );
        }
    }
    Ok(lock)
}

fn append_directory<W: std::io::Write>(
    archive: &mut Builder<W>,
    root: &Path,
    directory: &Path,
) -> Result<()> {
    let mut entries = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let relative = path.strip_prefix(root)?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            bail!(
                "installation contains unsupported symlink {}",
                path.display()
            );
        }
        if file_type.is_dir() {
            archive.append_dir(relative, &path)?;
            append_directory(archive, root, &path)?;
        } else if file_type.is_file() {
            archive.append_path_with_name(&path, relative)?;
        }
    }
    Ok(())
}

fn component_address(url: &str) -> Result<SocketAddr> {
    url.strip_prefix("http://")
        .context("component URL must use http://")?
        .parse()
        .context("component URL must contain a socket address")
}

fn checked_port(base: u16, offset: u16) -> Result<u16> {
    base.checked_add(offset)
        .context("base port does not leave room for all services")
}

fn absolute_destination(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(std::env::current_dir()?.join(path))
}

fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, bytes)?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(mode))?;
    fs::rename(temporary, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::{PermissionsExt as _, symlink},
    };

    use anyhow::Result;
    use tempfile::tempdir;

    use super::{backup, initialize, restore, set_provider};
    use crate::config::{
        DiscordActivation, DiscordConversation, DiscordRoute, LoadedConfig, ProviderRole,
    };
    use crate::db::Database;

    fn route() -> DiscordRoute {
        DiscordRoute {
            channel_id: "300".to_owned(),
            activation: DiscordActivation::Always,
            conversation: DiscordConversation::Channel,
        }
    }

    #[test]
    fn fresh_install_configures_only_discord_and_pi() -> Result<()> {
        let directory = tempdir()?;
        let root = directory.path().join("installation");
        let config_path = initialize(
            &root,
            directory.path(),
            "100",
            "101",
            &["200".to_owned()],
            &[route()],
            44_000,
        )?;
        let config = LoadedConfig::load(&config_path)?;

        assert_eq!(config.value.providers.len(), 2);
        assert_eq!(
            config
                .value
                .providers
                .iter()
                .map(|provider| (provider.id.as_str(), provider.role))
                .collect::<Vec<_>>(),
            [
                ("discord-private", ProviderRole::Interface),
                ("runtime-pi", ProviderRole::Runtime),
            ]
        );
        assert!(config.value.agent.interface_extensions.is_empty());
        assert_eq!(config.value.agent.capability_provider, None);

        let mut credentials = fs::read_dir(root.join("credentials"))?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<Result<Vec<_>, _>>()?;
        credentials.sort();
        assert_eq!(
            credentials,
            [
                "discord-private.token",
                "operator.token",
                "runtime-pi.token"
            ]
        );
        assert!(!root.join("state/components/echo-capability").exists());
        Ok(())
    }

    #[test]
    fn provider_command_rejects_unsafe_identity_before_mutation() -> Result<()> {
        let directory = tempdir()?;
        let root = directory.path().join("installation");
        let config_path = initialize(
            &root,
            directory.path(),
            "100",
            "101",
            &["200".to_owned()],
            &[route()],
            44_000,
        )?;
        let before = fs::read(&config_path)?;
        assert!(set_provider(&config_path, "../escape", false).is_err());
        assert_eq!(fs::read(&config_path)?, before);
        Ok(())
    }

    #[test]
    fn backup_and_restore_preserve_private_installation() -> Result<()> {
        let directory = tempdir()?;
        let root = directory.path().join("installation");
        let config_path = initialize(
            &root,
            directory.path(),
            "100",
            "101",
            &["200".to_owned()],
            &[route()],
            44_000,
        )?;
        fs::write(root.join("pi/auth.json"), "private runtime credential")?;
        let unsupported = root.join("unsupported-link");
        symlink(root.join("pi/auth.json"), &unsupported)?;
        let archive = directory.path().join("backup.tar.gz");
        let config = LoadedConfig::load(&config_path)?;
        assert!(backup(&config, &archive).is_err());
        assert!(!archive.exists());
        assert!(
            Database::open(&config.value.data_dir.join("carillon.sqlite3"))?
                .latest_backup()?
                .is_none()
        );
        fs::remove_file(unsupported)?;
        backup(&config, &archive)?;

        assert_eq!(fs::metadata(&archive)?.permissions().mode() & 0o777, 0o600);

        let restored = directory.path().join("restored");
        restore(&archive, &restored)?;

        assert_eq!(fs::metadata(&restored)?.permissions().mode() & 0o777, 0o700);
        assert_eq!(
            fs::read_to_string(restored.join("pi/auth.json"))?,
            "private runtime credential"
        );
        Ok(())
    }
}
