use std::{
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub const RELEASE_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Config {
    pub release: String,
    pub data_dir: PathBuf,
    pub core: CoreConfig,
    pub discord: DiscordConfig,
    pub agent: AgentConfig,
    pub runtime: Component,
    pub interface_extension: Component,
    pub capability: Component,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CoreConfig {
    pub listen: SocketAddr,
    pub operator_token_hash: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DiscordConfig {
    pub id: String,
    pub version: String,
    pub listen: SocketAddr,
    pub guild_id: String,
    pub allowed_user_ids: Vec<String>,
    pub token_hash: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AgentConfig {
    pub id: String,
    pub name: String,
    pub resource_root: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Component {
    pub id: String,
    pub role: ComponentRole,
    pub version: String,
    pub url: String,
    pub token_hash: String,
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ComponentRole {
    RuntimeAdapter,
    InterfaceExtension,
    Capability,
}

#[derive(Clone, Debug)]
pub struct LoadedConfig {
    pub path: PathBuf,
    pub root: PathBuf,
    pub value: Config,
}

impl LoadedConfig {
    pub fn load(path: &Path) -> Result<Self> {
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
        value.agent.resource_root = absolutize(&root, &value.agent.resource_root);
        value.validate()?;
        Ok(Self { path, root, value })
    }

    pub fn credential_path(&self, id: &str) -> PathBuf {
        self.root.join("credentials").join(format!("{id}.token"))
    }

    pub fn credential(&self, id: &str) -> Result<String> {
        let path = self.credential_path(id);
        Ok(fs::read_to_string(&path)
            .with_context(|| format!("read component credential {}", path.display()))?
            .trim()
            .to_owned())
    }
}

impl Config {
    fn validate(&self) -> Result<()> {
        if self.discord.guild_id.is_empty() || self.discord.allowed_user_ids.is_empty() {
            bail!("Discord admission requires a guild ID and at least one allowed user ID");
        }
        if self.core.operator_token_hash.is_empty() {
            bail!("operator diagnostics credential is not configured");
        }
        if self.runtime.role != ComponentRole::RuntimeAdapter
            || self.interface_extension.role != ComponentRole::InterfaceExtension
            || self.capability.role != ComponentRole::Capability
        {
            bail!("component roles do not match their configuration slots");
        }
        Ok(())
    }
}

pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub fn new_token() -> String {
    format!("{}{}", Uuid::now_v7().simple(), Uuid::now_v7().simple())
}

pub fn token_matches(expected_hash: &str, candidate: &str) -> bool {
    hash_token(candidate).as_bytes() == expected_hash.as_bytes()
}

pub fn read_token_file(path: &Path) -> Result<String> {
    Ok(fs::read_to_string(path)
        .with_context(|| format!("read token file {}", path.display()))?
        .trim()
        .to_owned())
}

fn absolutize(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}
