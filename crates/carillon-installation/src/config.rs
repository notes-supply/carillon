//! The installation registry `carillon.toml`: core listener, provider
//! registry, and the agents interfaces route conversations to.
//! Provider-owned settings live in the provider's own file under
//! `providers/`.

use std::{
    collections::BTreeSet,
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// The workspace release version shared by every bundled Rust component.
pub const RELEASE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Credential names that are never provider identities.
pub const RESERVED_CREDENTIALS: [&str; 2] = ["operator", "discord-bot"];

/// The registry, `carillon.toml`. Its tables reject fields they do not
/// define, so a misspelt setting fails validation instead of being dropped.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub data_dir: PathBuf,
    pub core: CoreConfig,
    pub providers: Vec<Provider>,
    #[serde(default)]
    pub agents: Vec<AgentConfig>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CoreConfig {
    pub listen: SocketAddr,
    pub operator_token_hash: String,
    #[serde(default, skip_serializing_if = "AttachmentLimits::is_default")]
    pub attachments: AttachmentLimits,
}

/// Core's bounds on the files it captures with accepted events, whichever
/// interface submits them. Interfaces apply their own, possibly smaller,
/// limits before staging; these protect core's storage. Changes apply when
/// core restarts.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct AttachmentLimits {
    /// Files one event may carry; 0 refuses every attachment.
    pub max_files: u32,
    pub max_file_mib: u64,
    pub max_event_mib: u64,
    /// Bytes kept by accepted captures together with captures in progress.
    /// New captures are refused beyond it; accepted files are never removed
    /// to make room.
    pub storage_mib: u64,
    /// Space a capture must leave free on the attachment filesystem.
    pub free_space_mib: u64,
    /// Captures copied at once; further submissions are refused as busy.
    pub concurrent_captures: u32,
}

impl Default for AttachmentLimits {
    fn default() -> Self {
        Self {
            max_files: 10,
            max_file_mib: 1024,
            max_event_mib: 2048,
            storage_mib: 10 * 1024,
            free_space_mib: 2048,
            concurrent_captures: 2,
        }
    }
}

/// One MiB, the unit of configured attachment sizes.
pub const MIB: u64 = 1024 * 1024;

impl AttachmentLimits {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }

    #[must_use]
    pub const fn max_file_bytes(&self) -> u64 {
        self.max_file_mib * MIB
    }

    #[must_use]
    pub const fn max_event_bytes(&self) -> u64 {
        self.max_event_mib * MIB
    }

    #[must_use]
    pub const fn storage_bytes(&self) -> u64 {
        self.storage_mib * MIB
    }

    #[must_use]
    pub const fn free_space_bytes(&self) -> u64 {
        self.free_space_mib * MIB
    }

    /// # Errors
    ///
    /// Returns an error naming the first setting outside its range.
    pub fn validate(&self) -> Result<()> {
        // A terabyte bounds every size so byte arithmetic cannot overflow.
        const MAX_MIB: u64 = 1024 * 1024;
        if self.max_files > 100 {
            bail!("core.attachments.max_files must be at most 100");
        }
        if !(1..=MAX_MIB).contains(&self.max_file_mib) {
            bail!("core.attachments.max_file_mib must be between 1 and {MAX_MIB}");
        }
        if !(self.max_file_mib..=MAX_MIB).contains(&self.max_event_mib) {
            bail!("core.attachments.max_event_mib must be between max_file_mib and {MAX_MIB}");
        }
        if !(self.max_event_mib..=MAX_MIB).contains(&self.storage_mib) {
            bail!("core.attachments.storage_mib must be between max_event_mib and {MAX_MIB}");
        }
        if self.free_space_mib > MAX_MIB {
            bail!("core.attachments.free_space_mib must be at most {MAX_MIB}");
        }
        if !(1..=16).contains(&self.concurrent_captures) {
            bail!("core.attachments.concurrent_captures must be between 1 and 16");
        }
        Ok(())
    }
}

/// A provider instance's private state directory, `<data_dir>/components/<id>`,
/// for a registry identity (which loading has already validated).
#[must_use]
pub fn component_directory(data_dir: &Path, provider_id: &str) -> PathBuf {
    data_dir.join("components").join(provider_id)
}

/// Where interface `provider_id` stages files for core to capture:
/// `incoming` in its component directory, one subdirectory per event batch.
/// Interface and core both derive it from the installation, so events never
/// carry filesystem paths. The staged files belong to the interface.
#[must_use]
pub fn attachment_staging(data_dir: &Path, provider_id: &str) -> PathBuf {
    component_directory(data_dir, provider_id).join("incoming")
}

/// An agent: who answers a conversation an interface routes to it, on which
/// runtime, in which workspace. Its runtime may hold further settings of
/// its own for it, keyed by its ID.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    pub id: String,
    pub name: String,
    /// The runtime provider a new conversation of the agent runs on.
    pub runtime: String,
    pub workspace: PathBuf,
    /// Carillon's resources for the agent, captured with each accepted
    /// turn. An agent without them runs with its runtime's own resources
    /// alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<PathBuf>,
}

impl AgentConfig {
    /// Display name, with the ID when another agent has the same name.
    #[must_use]
    pub fn label<'a>(&self, agents: impl IntoIterator<Item = &'a Self>) -> String {
        if agents
            .into_iter()
            .any(|other| other.id != self.id && other.name == self.name)
        {
            format!("{} ({})", self.name, self.id)
        } else {
            self.name.clone()
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    pub id: String,
    pub role: ProviderRole,
    pub version: String,
    pub url: String,
    pub token_hash: String,
    pub enabled: bool,
}

impl Provider {
    /// The loopback socket address behind the provider's URL.
    ///
    /// # Errors
    ///
    /// Returns an error when the URL is not `http://` on a loopback address
    /// with a nonzero port.
    pub fn listen_address(&self) -> Result<SocketAddr> {
        let address: SocketAddr = self
            .url
            .strip_prefix("http://")
            .context("provider URL must use http://")?
            .parse()
            .context("provider URL must contain a socket address")?;
        if !address.ip().is_loopback() || address.port() == 0 {
            bail!("provider URL must use a loopback address and a nonzero port");
        }
        Ok(address)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderRole {
    Interface,
    Runtime,
}

impl ProviderRole {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Interface => "interface",
            Self::Runtime => "runtime",
        }
    }
}

/// A validated `carillon.toml` with installation-relative paths resolved
/// against the file's directory.
#[derive(Clone, Debug)]
pub struct LoadedConfig {
    pub root: PathBuf,
    /// The registry file as the caller named it, which [`Self::reload`]
    /// reads again.
    pub path: PathBuf,
    pub value: Config,
}

impl LoadedConfig {
    /// # Errors
    ///
    /// Returns an error when the file cannot be read or parsed, or when the
    /// registry is inconsistent.
    pub fn load(path: &Path) -> Result<Self> {
        let named = path.to_path_buf();
        let path = path
            .canonicalize()
            .with_context(|| format!("read config {}", path.display()))?;
        let root = path
            .parent()
            .context("configuration path has no parent")?
            .to_path_buf();
        let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let mut value: Config =
            toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        value.data_dir = absolutize(&root, &value.data_dir);
        for agent in &mut value.agents {
            agent.workspace = absolutize(&root, &agent.workspace);
            if let Some(resources) = &mut agent.resources {
                *resources = absolutize(&root, resources);
            }
        }
        value.validate()?;
        value.validate_resource_state_separation()?;
        Ok(Self {
            root,
            path: named,
            value,
        })
    }

    /// The registry as its file says now, read again from where it was
    /// loaded.
    ///
    /// # Errors
    ///
    /// As for [`Self::load`].
    pub fn reload(&self) -> Result<Self> {
        Self::load(&self.path)
    }

    /// # Errors
    ///
    /// Returns an error when no provider has that identity and role.
    pub fn provider(&self, id: &str, role: ProviderRole) -> Result<&Provider> {
        self.value.provider(id, role)
    }

    /// The installation credential for a provider or reserved name.
    ///
    /// # Errors
    ///
    /// Returns an error when the identity is malformed or the file is
    /// unreadable.
    pub fn credential(&self, id: &str) -> Result<String> {
        validate_provider_identity(id)?;
        let path = self.root.join("credentials").join(format!("{id}.token"));
        Ok(fs::read_to_string(&path)
            .with_context(|| format!("read provider credential {}", path.display()))?
            .trim()
            .to_owned())
    }

    /// The provider-owned configuration file `providers/<id>.toml`. Core
    /// never reads it; its schema belongs to the provider.
    ///
    /// # Errors
    ///
    /// Returns an error when the identity is malformed.
    pub fn provider_config_path(&self, id: &str) -> Result<PathBuf> {
        validate_provider_identity(id)?;
        Ok(self.root.join("providers").join(format!("{id}.toml")))
    }
}

impl Config {
    /// # Errors
    ///
    /// Returns an error when no provider has that identity and role.
    pub fn provider(&self, id: &str, role: ProviderRole) -> Result<&Provider> {
        let provider = self
            .providers
            .iter()
            .find(|provider| provider.id == id)
            .with_context(|| format!("provider {id} is not configured"))?;
        if provider.role != role {
            bail!("provider {id} does not have role {}", role.as_str());
        }
        Ok(provider)
    }

    /// # Errors
    ///
    /// Returns an error when no agent has that identity.
    pub fn agent(&self, id: &str) -> Result<&AgentConfig> {
        self.agents
            .iter()
            .find(|agent| agent.id == id)
            .with_context(|| format!("agent {id} is not configured"))
    }

    /// Each distinct resource directory the agents name, in registry order.
    #[must_use]
    pub fn resource_roots(&self) -> Vec<&Path> {
        let mut roots: Vec<&Path> = Vec::new();
        for root in self
            .agents
            .iter()
            .filter_map(|agent| agent.resources.as_deref())
        {
            if !roots.contains(&root) {
                roots.push(root);
            }
        }
        roots
    }

    fn validate_resource_state_separation(&self) -> Result<()> {
        // Core requires existing state; capture requires an existing resource
        // root. Let those boundaries diagnose absent paths, so diagnostics and
        // receipt recovery do not depend on live resources being available.
        let resolve = |path: &Path| -> Result<Option<PathBuf>> {
            match path.canonicalize() {
                Ok(path) => Ok(Some(path)),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    Ok(None)
                }
                Err(error) => Err(error).with_context(|| format!("resolve {}", path.display())),
            }
        };
        let Some(data) = resolve(&self.data_dir)? else {
            return Ok(());
        };
        for agent in &self.agents {
            let Some(resources) = agent
                .resources
                .as_deref()
                .map(resolve)
                .transpose()?
                .flatten()
            else {
                continue;
            };
            for subtree in ["extensions", "skills"] {
                if data.starts_with(resources.join(subtree)) {
                    bail!(
                        "data_dir must not be inside the resources of agent {} ({subtree})",
                        agent.id
                    );
                }
            }
        }
        Ok(())
    }

    /// # Errors
    ///
    /// Returns an error when the registry is internally inconsistent.
    pub fn validate(&self) -> Result<()> {
        if self.core.listen.port() == 0 {
            bail!("core listen port must be nonzero");
        }
        if !valid_token_hash(&self.core.operator_token_hash) {
            bail!(
                "operator diagnostics credential hash must be 64 lowercase hexadecimal characters"
            );
        }
        self.core.attachments.validate()?;
        let mut provider_ids = BTreeSet::new();
        let mut listen_addresses = BTreeSet::from([self.core.listen]);
        for provider in &self.providers {
            validate_provider_identity(&provider.id)?;
            if RESERVED_CREDENTIALS.contains(&provider.id.as_str()) {
                bail!(
                    "provider ID {} is reserved for installation credentials",
                    provider.id
                );
            }
            if provider.version.trim().is_empty() {
                bail!("provider {} must declare a version", provider.id);
            }
            let address = provider
                .listen_address()
                .with_context(|| format!("invalid URL for provider {}", provider.id))?;
            if provider.enabled
                && (!listen_addresses.insert(address)
                    || (self.core.listen.port() == address.port()
                        && self.core.listen.ip().is_unspecified()
                        && (self.core.listen.is_ipv6() || address.is_ipv4())))
            {
                bail!(
                    "provider {} listen address conflicts with another service",
                    provider.id
                );
            }
            if !valid_token_hash(&provider.token_hash) {
                bail!(
                    "provider {} credential hash must be 64 lowercase hexadecimal characters",
                    provider.id
                );
            }
            if !provider_ids.insert(&provider.id) {
                bail!("provider ID {} is configured more than once", provider.id);
            }
        }
        let mut agent_ids = BTreeSet::new();
        for agent in &self.agents {
            validate_agent_identity(&agent.id)?;
            if !agent_ids.insert(&agent.id) {
                bail!("agent ID {} is configured more than once", agent.id);
            }
            if agent.name.trim().is_empty() {
                bail!("agent {} must have a name", agent.id);
            }
            self.provider(&agent.runtime, ProviderRole::Runtime)
                .with_context(|| format!("agent {} needs a configured runtime", agent.id))?;
        }
        Ok(())
    }
}

/// Agent identities share provider identities' path-safe grammar: runtimes
/// key their own files and settings by them.
///
/// # Errors
///
/// Returns an error unless the value is 1-128 bytes of ASCII letters, digits,
/// `-` or `_`.
pub fn validate_agent_identity(value: &str) -> Result<()> {
    validate_provider_identity(value).map_err(|_| {
        anyhow::anyhow!("agent identity must be 1-128 bytes of ASCII letters, digits, '-' or '_'")
    })
}

/// # Errors
///
/// Returns an error unless the value is 1-128 bytes of ASCII letters, digits,
/// `-` or `_`.
pub fn validate_provider_identity(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("provider identity must be 1-128 bytes of ASCII letters, digits, '-' or '_'");
    }
    Ok(())
}

#[must_use]
pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

#[must_use]
pub fn new_token() -> String {
    format!("{}{}", Uuid::now_v7().simple(), Uuid::now_v7().simple())
}

#[must_use]
pub fn token_matches(expected_hash: &str, candidate: &str) -> bool {
    hash_token(candidate).as_bytes() == expected_hash.as_bytes()
}

/// # Errors
///
/// Returns an error when the file cannot be read.
pub fn read_token_file(path: &Path) -> Result<String> {
    Ok(fs::read_to_string(path)
        .with_context(|| format!("read token file {}", path.display()))?
        .trim()
        .to_owned())
}

#[must_use]
pub fn absolutize(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn valid_token_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::validate_provider_identity;

    #[test]
    fn provider_identity_uses_one_path_safe_slug_grammar() {
        for valid in ["a", "runtime-pi", "interface_2", "A9"] {
            assert_eq!(
                validate_provider_identity(valid).map_err(|error| error.to_string()),
                Ok(()),
                "{valid:?} should be valid"
            );
        }
        let overlong = "a".repeat(129);
        for invalid in [
            "",
            ".",
            "..",
            "../escape",
            "/absolute",
            "with/slash",
            r"with\backslash",
            "with space",
            "with\ttab",
            "with\ncontrol",
            &overlong,
        ] {
            assert_eq!(
                validate_provider_identity(invalid).map_err(|error| error.to_string()),
                Err(
                    "provider identity must be 1-128 bytes of ASCII letters, digits, '-' or '_'"
                        .to_owned()
                ),
                "{invalid:?} should be invalid"
            );
        }
    }
}
