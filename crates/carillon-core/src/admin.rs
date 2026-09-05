//! Operator actions on an installation: initialization, configuration edits,
//! backup, and restore. These run against a stopped or exclusive-locked
//! installation, never inside the services.

use std::{
    fs,
    io::{BufReader, BufWriter, Write as _},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
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

/// Exclusive lock over an installation's durable state directory. Core holds
/// one while serving; backup takes it to prove the services are stopped.
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
    if !workspace.is_absolute() || !workspace.is_dir() {
        bail!("workspace must be an absolute path to an existing directory");
    }
    if root.exists() && fs::read_dir(root)?.next().is_some() {
        bail!("installation root {} is not empty", root.display());
    }
    let discord_token = new_token();
    let operator_token = new_token();
    let runtime_token = new_token();

    let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let config = Config {
        data_dir: PathBuf::from("state"),
        core: CoreConfig {
            listen: SocketAddr::new(loopback, base_port),
            operator_token_hash: hash_token(&operator_token),
        },
        providers: default_providers(base_port, &discord_token, &runtime_token)?,
        discord: Some(DiscordConfig::with_defaults(
            "discord-private".to_owned(),
            "default".to_owned(),
            guild_id.to_owned(),
            bot_user_id.to_owned(),
            users.to_vec(),
            routes.to_vec(),
        )),
        agent: AgentConfig {
            id: "default".to_owned(),
            name: "Default agent".to_owned(),
            resource_root: PathBuf::from("agents/default"),
            workspace: workspace.to_path_buf(),
            runtime_provider: "runtime-pi".to_owned(),
        },
    };
    config.validate()?;
    config.discord()?;
    let target = absolute_destination(root)?;
    let parent = target
        .parent()
        .context("installation root must have a parent")?;
    fs::create_dir_all(parent)?;
    let staging = parent.join(format!(".carillon-init-{}", Uuid::now_v7()));
    fs::DirBuilder::new().mode(0o700).create(&staging)?;
    let result = (|| -> Result<()> {
        let root = &staging;
        create_installation_directories(root)?;
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
        Database::create(&root.join("state/carillon.sqlite3"))?;
        crate::filesystem::sync_tree(root)?;
        // Rename replaces an empty target directory atomically, but refuses a
        // nonempty target that appeared meanwhile. Never adopt partial state.
        fs::rename(root, &target)?;
        // Persist the publication and any newly created parent directories. A
        // failure after rename leaves the complete installation selected for retry.
        crate::filesystem::sync_ancestors(parent)?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    Ok(target.join("carillon.toml"))
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
    let root = config
        .root
        .canonicalize()
        .context("resolve installation root")?;
    let destination = absolute_destination(destination)?;
    let parent = destination
        .parent()
        .context("backup destination must have a parent directory")?
        .canonicalize()
        .context("resolve backup destination parent directory")?;
    if parent.starts_with(&root) {
        bail!("backup destination must be outside the installation root");
    }
    let name = destination
        .file_name()
        .context("backup destination must name a file")?;
    let destination = parent.join(name);
    if destination.exists() {
        bail!(
            "backup destination {} already exists",
            destination.display()
        );
    }
    // Only installation-owned state is archived; the user's workspace may
    // deliberately live elsewhere.
    for (field, path) in [
        ("data_dir", &config.value.data_dir),
        ("agent.resource_root", &config.value.agent.resource_root),
    ] {
        let path = path
            .canonicalize()
            .with_context(|| format!("resolve backup {field}"))?;
        if !path.starts_with(&root) {
            bail!("backup requires {field} to be inside the installation root");
        }
    }
    validate_portable_paths(&root)?;
    let _lock = ensure_stopped(config)?;
    validate_installation(config)?;
    let staging = parent.join(format!(
        ".{}.backup-{}",
        name.to_string_lossy(),
        Uuid::now_v7()
    ));
    let result = (|| {
        let file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&staging)?;
        let encoder = GzEncoder::new(BufWriter::new(file), Compression::default());
        let mut archive = Builder::new(encoder);
        append_directory(&mut archive, &root, &root)?;
        archive.finish()?;
        let encoder = archive.into_inner()?;
        let mut writer = encoder.finish()?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
        Ok(())
    })();
    if let Err(failure) = result {
        let _ = fs::remove_file(&staging);
        return Err(failure);
    }
    // Unlike rename, hard_link fails if the destination appeared meanwhile,
    // so a finished archive can never overwrite an existing file.
    if let Err(failure) = fs::hard_link(&staging, &destination) {
        let _ = fs::remove_file(&staging);
        return Err(failure.into());
    }
    let _ = fs::remove_file(&staging);
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

pub fn restore(archive_path: &Path, target: &Path) -> Result<PathBuf> {
    let target = absolute_destination(target)?;
    if target.exists() && fs::read_dir(&target)?.next().is_some() {
        bail!("restore target {} is not empty", target.display());
    }
    let parent = target
        .parent()
        .context("restore target must have a parent directory")?;
    fs::create_dir_all(parent)?;
    let name = target
        .file_name()
        .context("restore target must name a directory")?
        .to_string_lossy();
    let staging = parent.join(format!(".{name}.restore-{}", Uuid::now_v7()));
    fs::DirBuilder::new().mode(0o700).create(&staging)?;

    let result = restore_into(archive_path, &staging)
        .and_then(|()| Ok(crate::filesystem::sync_tree(&staging)?));
    if let Err(failure) = result {
        let _ = fs::remove_dir_all(&staging);
        return Err(failure);
    }
    if let Err(failure) = fs::rename(&staging, &target) {
        let _ = fs::remove_dir_all(&staging);
        return Err(failure.into());
    }
    crate::filesystem::sync_ancestors(parent)?;
    Ok(target.join("carillon.toml"))
}

fn restore_into(archive_path: &Path, target: &Path) -> Result<()> {
    let file = fs::File::open(archive_path)
        .with_context(|| format!("open backup {}", archive_path.display()))?;
    let decoder = GzDecoder::new(BufReader::new(file));
    let mut archive = Archive::new(decoder);
    archive.set_preserve_permissions(true);
    archive.set_preserve_ownerships(false);
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
        if !entry.header().entry_type().is_file() && !entry.header().entry_type().is_dir() {
            bail!("backup contains unsupported entry {}", path.display());
        }
        entry.unpack_in(target)?;
    }
    validate_portable_paths(target)?;
    let config = LoadedConfig::load(&target.join("carillon.toml"))
        .context("restored backup is not a valid Carillon installation")?;
    validate_installation(&config)?;
    Ok(())
}

fn validate_portable_paths(root: &Path) -> Result<()> {
    // LoadedConfig resolves paths for services; check the stored spelling before
    // allowing it into an archive that must work under a different root.
    let config: Config = toml::from_str(&fs::read_to_string(root.join("carillon.toml"))?)?;
    for (field, path) in [
        ("data_dir", &config.data_dir),
        ("agent.resource_root", &config.agent.resource_root),
    ] {
        if path.as_os_str().is_empty()
            || path.is_absolute()
            || path
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            bail!("backup and restore require {field} to be installation-relative without '..'");
        }
    }
    Ok(())
}

fn validate_installation(config: &LoadedConfig) -> Result<()> {
    if !config.value.agent.resource_root.is_dir() {
        bail!("backup is missing the configured agent resource directory");
    }
    for (id, expected) in std::iter::once(("operator", &config.value.core.operator_token_hash))
        .chain(
            config
                .value
                .providers
                .iter()
                .map(|provider| (provider.id.as_str(), &provider.token_hash)),
        )
    {
        let token = config.credential(id)?;
        if token.is_empty() || hash_token(&token) != *expected {
            bail!("backup credential for {id} does not match the configuration");
        }
    }
    Database::validate_backup(&config.value.data_dir.join("carillon.sqlite3"))
        .context("backup does not contain a valid current core database")
}

fn ensure_stopped(config: &LoadedConfig) -> Result<ServiceLock> {
    let lock = ServiceLock::acquire(&config.value.data_dir)?;
    let mut addresses = vec![config.value.core.listen];
    for provider in &config.value.providers {
        addresses.push(provider.listen_address()?);
    }
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
        io::Cursor,
        os::unix::fs::{MetadataExt, PermissionsExt, symlink},
        path::{Path, PathBuf},
    };

    use anyhow::{Context, Result};
    use flate2::{Compression, write::GzEncoder};
    use tar::{Builder, Header};
    use tempfile::tempdir;

    use super::{backup, initialize, restore, set_provider};
    use crate::config::{
        Config, DiscordActivation, DiscordConversation, DiscordRoute, LoadedConfig, ProviderRole,
    };

    fn route() -> DiscordRoute {
        DiscordRoute {
            channel_id: "300".to_owned(),
            activation: DiscordActivation::Always,
            conversation: DiscordConversation::Channel,
        }
    }

    fn initialize_test_installation(
        root: &Path,
        workspace: &Path,
        base_port: u16,
    ) -> Result<PathBuf> {
        initialize(
            root,
            workspace,
            "100",
            "101",
            &["200".to_owned()],
            &[route()],
            base_port,
        )
    }

    #[test]
    fn initialization_rejects_unusable_workspaces_without_publishing_state() -> Result<()> {
        let directory = tempdir()?;
        let root = directory.path().join("installation");
        let missing = directory.path().join("missing");
        let file = directory.path().join("file");
        fs::write(&file, "not a directory")?;
        for workspace in [Path::new("."), Path::new("relative"), &missing, &file] {
            assert!(initialize_test_installation(&root, workspace, 44_000).is_err());
            assert!(!root.exists());
            fs::create_dir(&root)?;
            assert!(initialize_test_installation(&root, workspace, 44_000).is_err());
            assert!(fs::read_dir(&root)?.next().is_none());
            fs::remove_dir(&root)?;
        }
        let config = initialize_test_installation(&root, directory.path(), 44_000)?;
        assert_eq!(
            LoadedConfig::load(&config)?.value.agent.workspace,
            directory.path()
        );
        Ok(())
    }

    #[test]
    fn initialization_rejects_unusable_ports_without_writing_installation() -> Result<()> {
        let directory = tempdir()?;
        let root = directory.path().join("installation");
        for base_port in [0, 65_534, 65_535] {
            assert!(initialize_test_installation(&root, directory.path(), base_port).is_err());
            assert!(!root.exists());
            fs::create_dir(&root)?;
            assert!(initialize_test_installation(&root, directory.path(), base_port).is_err());
            assert!(fs::read_dir(&root)?.next().is_none());
            fs::remove_dir(&root)?;
        }
        initialize_test_installation(&root, directory.path(), 44_000)?;
        LoadedConfig::load(&root.join("carillon.toml"))?;
        Ok(())
    }

    #[test]
    fn bundled_install_configures_default_providers_and_their_credentials() -> Result<()> {
        let directory = tempdir()?;
        let root = directory.path().join("installation");
        let config_path = initialize_test_installation(&root, directory.path(), 44_000)?;
        let config = LoadedConfig::load(&config_path)?;

        assert_eq!(config.value.providers.len(), 2);
        // This initializer bundles defaults; it does not constrain core providers.
        for (id, role) in [
            ("discord-private", ProviderRole::Interface),
            ("runtime-pi", ProviderRole::Runtime),
        ] {
            config.provider(id, role)?;
        }

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
        Ok(())
    }

    #[test]
    fn backup_archive_is_owner_only() -> Result<()> {
        let directory = tempdir()?;
        let root = directory.path().join("installation");
        let config_path = initialize_test_installation(&root, directory.path(), 44_100)?;
        let destination = directory.path().join("backup.tar.gz");

        backup(&LoadedConfig::load(&config_path)?, &destination)?;

        assert_eq!(
            fs::metadata(destination)?.permissions().mode() & 0o777,
            0o600
        );
        Ok(())
    }

    #[test]
    fn backup_rejects_symlink_and_traversal_destinations_inside_installation() -> Result<()> {
        let directory = tempdir()?;
        let root = directory.path().join("installation");
        let config_path = initialize_test_installation(&root, directory.path(), 44_400)?;
        let mut config = LoadedConfig::load(&config_path)?;
        let inside = root.join("backups");
        fs::create_dir(&inside)?;
        let alias = directory.path().join("alias");
        symlink(&root, &alias)?;
        let outside = directory.path().join("outside");
        fs::create_dir(&outside)?;

        for source in [&root, &alias] {
            config.root = source.clone();
            for destination in [
                alias.join("backups/backup.tar.gz"),
                outside.join("../installation/backups/backup.tar.gz"),
            ] {
                let error = backup(&config, &destination)
                    .err()
                    .context("backup accepted a destination inside the installation")?;
                assert_eq!(
                    error.to_string(),
                    "backup destination must be outside the installation root"
                );
                assert!(fs::read_dir(&inside)?.next().is_none());
                assert!(!root.join("state/service.lock").exists());
            }
        }
        Ok(())
    }

    #[test]
    fn backup_rejects_external_configured_owned_paths() -> Result<()> {
        for field in ["data_dir", "agent.resource_root"] {
            let directory = tempdir()?;
            let root = directory.path().join("installation");
            let config_path = initialize_test_installation(&root, directory.path(), 44_500)?;
            let external = directory.path().join("external");
            fs::create_dir(&external)?;
            fs::write(external.join("owned-state"), b"must not be omitted")?;
            let alias = root.join("external-alias");
            symlink(&external, &alias)?;
            let output = directory.path().join("backups");
            fs::create_dir(&output)?;

            let mut config: Config = toml::from_str(&fs::read_to_string(&config_path)?)?;
            for path in [
                external.clone(),
                PathBuf::from("../external"),
                PathBuf::from("external-alias"),
            ] {
                match field {
                    "data_dir" => config.data_dir = path,
                    _ => config.agent.resource_root = path,
                }
                fs::write(&config_path, toml::to_string_pretty(&config)?)?;
                let loaded = LoadedConfig::load(&config_path)?;
                let error = backup(&loaded, &output.join("backup.tar.gz"))
                    .err()
                    .context("backup accepted external owned state")?;
                assert_eq!(
                    error.to_string(),
                    format!("backup requires {field} to be inside the installation root")
                );
                assert!(fs::read_dir(&output)?.next().is_none());
                assert!(!external.join("service.lock").exists());
                assert!(!root.join("state/service.lock").exists());
                assert_eq!(
                    fs::read(external.join("owned-state"))?,
                    b"must not be omitted"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn backup_and_restore_reject_absolute_managed_paths_inside_source() -> Result<()> {
        for field in ["data_dir", "agent.resource_root"] {
            let directory = tempdir()?;
            let root = directory.path().join("installation");
            let config_path = initialize_test_installation(&root, directory.path(), 44_700)?;
            let mut config: Config = toml::from_str(&fs::read_to_string(&config_path)?)?;
            match field {
                "data_dir" => config.data_dir = root.join("state"),
                _ => config.agent.resource_root = root.join("agents/default"),
            }
            fs::write(&config_path, toml::to_string_pretty(&config)?)?;
            let destination = directory.path().join("backup.tar.gz");
            let error = backup(&LoadedConfig::load(&config_path)?, &destination)
                .err()
                .context("backup accepted an absolute managed path")?;
            assert!(
                error
                    .to_string()
                    .contains(&format!("require {field} to be installation-relative"))
            );
            assert!(!destination.exists());
            assert!(!root.join("state/service.lock").exists());

            // A manually built archive must not make restore use source state,
            // even when that state is still accessible to this Unix user.
            archive_test_tree(&root, &destination)?;
            let target = directory.path().join("restored");
            assert!(restore(&destination, &target).is_err());
            assert!(!target.exists());
        }
        Ok(())
    }

    fn archive_test_tree(root: &Path, destination: &Path) -> Result<()> {
        let encoder = GzEncoder::new(fs::File::create(destination)?, Compression::default());
        let mut archive = Builder::new(encoder);
        super::append_directory(&mut archive, root, root)?;
        archive.into_inner()?.finish()?;
        Ok(())
    }

    #[test]
    fn incomplete_restore_is_rejected_before_publication_and_can_be_retried() -> Result<()> {
        for damage in [
            "config-only",
            "database",
            "operator",
            "provider",
            "resource-root",
            "wrong-credential",
            "corrupt-database",
            "empty-database",
            "old-schema",
            "foreign-key",
        ] {
            let directory = tempdir()?;
            let source = directory.path().join("source");
            let config_path = initialize_test_installation(&source, directory.path(), 44_800)?;
            let valid = directory.path().join("valid.tar.gz");
            backup(&LoadedConfig::load(&config_path)?, &valid)?;
            let database = source.join("state/carillon.sqlite3");
            match damage {
                "config-only" => {
                    for entry in fs::read_dir(&source)? {
                        let entry = entry?;
                        if entry.file_type()?.is_dir() {
                            fs::remove_dir_all(entry.path())?;
                        }
                    }
                }
                "database" => fs::remove_file(&database)?,
                "operator" => fs::remove_file(source.join("credentials/operator.token"))?,
                "provider" => fs::remove_file(source.join("credentials/runtime-pi.token"))?,
                "resource-root" => fs::remove_dir_all(source.join("agents/default"))?,
                "wrong-credential" => {
                    fs::write(source.join("credentials/runtime-pi.token"), b"wrong")?;
                }
                "corrupt-database" => fs::write(&database, b"not a database")?,
                "empty-database" => fs::write(&database, b"")?,
                "old-schema" => {
                    rusqlite::Connection::open(&database)?
                        .execute_batch("ALTER TABLE deliveries DROP COLUMN claim_owner")?;
                }
                "foreign-key" => {
                    rusqlite::Connection::open(&database)?.execute_batch(
                        "PRAGMA foreign_keys = OFF;
                         INSERT INTO deliveries (turn_id, interface_id, target_id, feedback_target_id, content, state, updated_at)
                         VALUES ('missing', 'interface', 'target', 'feedback', 'output', 'pending', 0)"
                    )?;
                }
                _ => unreachable!(),
            }
            let invalid = directory.path().join("invalid.tar.gz");
            archive_test_tree(&source, &invalid)?;
            fs::remove_dir_all(&source)?;
            for existing_target in [false, true] {
                let target = directory.path().join("restored");
                if existing_target {
                    fs::create_dir(&target)?;
                }
                assert!(restore(&invalid, &target).is_err(), "accepted {damage}");
                assert_eq!(target.exists(), existing_target, "published {damage}");
                if existing_target {
                    assert!(fs::read_dir(&target)?.next().is_none());
                }
                for entry in fs::read_dir(directory.path())? {
                    assert!(
                        !entry?
                            .file_name()
                            .to_string_lossy()
                            .starts_with(".restored.restore-")
                    );
                }
                let restored = LoadedConfig::load(&restore(&valid, &target)?)?;
                super::validate_installation(&restored)?;
                fs::remove_dir_all(&target)?;
            }
        }
        Ok(())
    }

    #[test]
    fn backup_restores_opaque_native_state_without_bundled_provider_requirements() -> Result<()> {
        let directory = tempdir()?;
        let root = directory.path().join("source");
        let config_path = initialize_test_installation(&root, directory.path(), 44_900)?;
        let mut config: Config = toml::from_str(&fs::read_to_string(&config_path)?)?;
        config.discord = None;
        config
            .providers
            .retain(|provider| provider.role == ProviderRole::Runtime);
        config.providers[0].id = "other-runtime".to_owned();
        config.agent.runtime_provider = "other-runtime".to_owned();
        fs::rename(
            root.join("credentials/runtime-pi.token"),
            root.join("credentials/other-runtime.token"),
        )?;
        fs::remove_file(root.join("credentials/discord-private.token"))?;
        fs::remove_dir_all(root.join("pi"))?;
        fs::remove_dir_all(root.join("state/components"))?;
        fs::remove_dir_all(root.join("agents/default"))?;
        fs::create_dir(root.join("agents/default"))?; // All individual resources are optional.
        fs::write(&config_path, toml::to_string_pretty(&config)?)?;
        let native = root.join("other-engine");
        fs::create_dir(&native)?;
        let fixtures = [
            ("history", "native session history"),
            ("auth", "private auth fixture"),
            ("settings", "private engine settings"),
        ];
        for (name, content) in fixtures {
            fs::write(native.join(name), content)?;
        }
        let destination = directory.path().join("backup.tar.gz");
        backup(&LoadedConfig::load(&config_path)?, &destination)?;
        fs::remove_dir_all(&root)?;
        let target = directory.path().join("restored");
        restore(&destination, &target)?;
        for (name, content) in fixtures {
            assert_eq!(
                fs::read_to_string(target.join("other-engine").join(name))?,
                content
            );
        }
        assert!(!target.join("pi").exists());
        Ok(())
    }

    #[test]
    fn backup_restores_owned_state_with_an_external_workspace() -> Result<()> {
        let directory = tempdir()?;
        let root = directory.path().join("installation");
        let workspace = directory.path().join("workspace");
        fs::create_dir(&workspace)?;
        fs::write(workspace.join("user-file"), b"user-owned")?;
        let config_path = initialize_test_installation(&root, &workspace, 44_600)?;
        fs::write(root.join("state/owned-state"), b"durable state")?;
        fs::write(root.join("agents/default/SYSTEM.md"), b"agent instructions")?;
        let output = directory.path().join("backups");
        fs::create_dir(&output)?;
        let alias = directory.path().join("backup-alias");
        symlink(&output, &alias)?;
        let destination = alias.join("backup.tar.gz");

        backup(&LoadedConfig::load(&config_path)?, &destination)?;
        fs::remove_dir_all(&root)?;
        let target = directory.path().join("restored");
        let restored = LoadedConfig::load(&restore(&destination, &target)?)?;

        assert_eq!(
            fs::read(restored.value.data_dir.join("owned-state"))?,
            b"durable state"
        );
        assert!(restored.value.data_dir.join("carillon.sqlite3").is_file());
        assert_eq!(
            fs::read(restored.value.agent.resource_root.join("SYSTEM.md"))?,
            b"agent instructions"
        );
        assert_eq!(restored.value.agent.workspace, workspace);
        assert_eq!(fs::read(workspace.join("user-file"))?, b"user-owned");
        assert!(!target.join("workspace").exists());
        Ok(())
    }

    #[test]
    fn failed_restore_leaves_target_retryable() -> Result<()> {
        let directory = tempdir()?;
        let source = directory.path().join("source");
        let config_path = initialize_test_installation(&source, directory.path(), 44_200)?;
        let valid_archive = directory.path().join("valid.tar.gz");
        backup(&LoadedConfig::load(&config_path)?, &valid_archive)?;

        let invalid_archive = directory.path().join("invalid.tar.gz");
        let archive_file = fs::File::create(&invalid_archive)?;
        let encoder = GzEncoder::new(archive_file, Compression::default());
        let mut archive = Builder::new(encoder);
        let invalid_config = b"not valid TOML";
        let mut header = Header::new_gnu();
        header.set_path("carillon.toml")?;
        header.set_size(invalid_config.len().try_into()?);
        header.set_mode(0o600);
        header.set_cksum();
        archive.append(&header, Cursor::new(invalid_config))?;
        archive.into_inner()?.finish()?;
        let target = directory.path().join("restored");
        assert!(restore(&invalid_archive, &target).is_err());
        assert!(!target.exists());
        let names = fs::read_dir(directory.path())?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<std::io::Result<Vec<_>>>()?;
        assert!(
            names
                .iter()
                .all(|name| !name.to_string_lossy().starts_with(".restored.restore-"))
        );

        let restored_config = restore(&valid_archive, &target)?;
        LoadedConfig::load(&restored_config)?;
        Ok(())
    }

    #[test]
    fn restore_uses_the_invoking_users_ownership() -> Result<()> {
        let directory = tempdir()?;
        let source = directory.path().join("source");
        let config_path = initialize_test_installation(&source, directory.path(), 44_300)?;
        let valid = directory.path().join("valid.tar.gz");
        backup(&LoadedConfig::load(&config_path)?, &valid)?;
        let mut original = tar::Archive::new(flate2::read::GzDecoder::new(fs::File::open(valid)?));
        let archive_path = directory.path().join("foreign-owner.tar.gz");
        let encoder = GzEncoder::new(fs::File::create(&archive_path)?, Compression::default());
        let mut archive = Builder::new(encoder);
        for entry in original.entries()? {
            let mut entry = entry?;
            let mut header = entry.header().clone();
            header.set_uid(123_456);
            header.set_gid(123_456);
            header.set_cksum();
            archive.append(&header, &mut entry)?;
        }
        archive.into_inner()?.finish()?;
        fs::remove_dir_all(&source)?;

        let target = directory.path().join("restored");
        let restored_config = restore(&archive_path, &target)?;

        assert_eq!(
            fs::metadata(restored_config)?.uid(),
            fs::metadata(directory.path())?.uid()
        );
        Ok(())
    }

    #[test]
    fn provider_command_rejects_unsafe_identity_before_mutation() -> Result<()> {
        let directory = tempdir()?;
        let root = directory.path().join("installation");
        let config_path = initialize_test_installation(&root, directory.path(), 44_000)?;
        let before = fs::read(&config_path)?;
        assert!(set_provider(&config_path, "../escape", false).is_err());
        assert_eq!(fs::read(&config_path)?, before);
        Ok(())
    }
}
