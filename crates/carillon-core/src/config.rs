use std::{
    collections::BTreeSet,
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    str::FromStr,
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
    #[serde(default)]
    pub bot_user_id: String,
    pub allowed_user_ids: Vec<String>,
    #[serde(default)]
    pub routes: Vec<DiscordRoute>,
    #[serde(default = "enabled")]
    pub typing: bool,
    #[serde(default = "enabled")]
    pub reactions: bool,
    #[serde(default = "processing_emoji")]
    pub processing_emoji: String,
    #[serde(default = "delivered_emoji")]
    pub delivered_emoji: String,
    #[serde(default = "failed_emoji")]
    pub failed_emoji: String,
    #[serde(default = "uncertain_emoji")]
    pub uncertain_emoji: String,
    pub token_hash: String,
}

const fn enabled() -> bool {
    true
}

fn processing_emoji() -> String {
    "👀".to_owned()
}

fn delivered_emoji() -> String {
    "✅".to_owned()
}

fn failed_emoji() -> String {
    "❌".to_owned()
}

fn uncertain_emoji() -> String {
    "⚠️".to_owned()
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct DiscordRoute {
    pub channel_id: String,
    pub activation: DiscordActivation,
    pub conversation: DiscordConversation,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DiscordActivation {
    Mention,
    Always,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DiscordConversation {
    Thread,
    Channel,
}

impl FromStr for DiscordRoute {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let mut fields = value.split(':');
        let channel_id = fields.next().unwrap_or_default();
        let activation = match fields.next() {
            Some("mention") => DiscordActivation::Mention,
            Some("always") => DiscordActivation::Always,
            _ => bail!("route activation must be mention or always"),
        };
        let conversation = match fields.next() {
            Some("thread") => DiscordConversation::Thread,
            Some("channel") => DiscordConversation::Channel,
            _ => bail!("route conversation must be thread or channel"),
        };
        if fields.next().is_some() || !valid_snowflake(channel_id) {
            bail!("route must be CHANNEL_ID:mention|always:thread|channel");
        }
        Ok(Self {
            channel_id: channel_id.to_owned(),
            activation,
            conversation,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiscordEmoji {
    Unicode(String),
    Custom { name: String, id: u64 },
}

impl DiscordEmoji {
    pub fn parse(value: &str) -> Result<Self> {
        if let Some((name, id)) = value.split_once(':') {
            if name.len() < 2
                || name.len() > 32
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                || id.contains(':')
                || !valid_snowflake(id)
            {
                bail!("custom Discord emoji must use canonical name:id syntax");
            }
            return Ok(Self::Custom {
                name: name.to_owned(),
                id: id.parse()?,
            });
        }
        if emojis::get(value).is_none() {
            bail!("Discord emoji must be Unicode emoji or canonical custom name:id");
        }
        Ok(Self::Unicode(value.to_owned()))
    }
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
    pub(crate) fn validate(&self) -> Result<()> {
        let discord = &self.discord;
        if !valid_snowflake(&discord.guild_id)
            || !valid_snowflake(&discord.bot_user_id)
            || discord.allowed_user_ids.is_empty()
            || discord
                .allowed_user_ids
                .iter()
                .any(|user| !valid_snowflake(user))
            || discord.routes.is_empty()
        {
            bail!("Discord admission requires immutable guild, bot, user, and channel route IDs");
        }
        let mut channels = BTreeSet::new();
        for route in &discord.routes {
            if !valid_snowflake(&route.channel_id) || !channels.insert(&route.channel_id) {
                bail!("Discord route channel IDs must be valid and unique");
            }
        }
        for emoji in [
            &discord.processing_emoji,
            &discord.delivered_emoji,
            &discord.failed_emoji,
            &discord.uncertain_emoji,
        ] {
            DiscordEmoji::parse(emoji)?;
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

fn valid_snowflake(value: &str) -> bool {
    value
        .parse::<u64>()
        .is_ok_and(|id| id != 0 && id.to_string() == value)
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::{
        DiscordActivation, DiscordConfig, DiscordConversation, DiscordEmoji, DiscordRoute,
    };

    #[test]
    fn legacy_discord_configuration_receives_ingress_defaults() -> anyhow::Result<()> {
        let config: DiscordConfig = toml::from_str(
            r#"
id = "discord-private"
version = "0.0.0"
listen = "127.0.0.1:43801"
guild_id = "100"
allowed_user_ids = ["200"]
token_hash = "hash"
"#,
        )?;
        assert!(config.bot_user_id.is_empty());
        assert!(config.routes.is_empty());
        assert!(config.typing);
        assert!(config.reactions);
        assert_eq!(config.processing_emoji, "👀");
        assert_eq!(config.delivered_emoji, "✅");
        assert_eq!(config.failed_emoji, "❌");
        assert_eq!(config.uncertain_emoji, "⚠️");
        Ok(())
    }

    #[test]
    fn route_requires_all_explicit_fields() -> anyhow::Result<()> {
        let route = DiscordRoute::from_str("300:mention:thread")?;
        assert_eq!(route.channel_id, "300");
        assert_eq!(route.activation, DiscordActivation::Mention);
        assert_eq!(route.conversation, DiscordConversation::Thread);
        assert!(DiscordRoute::from_str("300:mention").is_err());
        assert!(DiscordRoute::from_str("300:free:thread").is_err());
        Ok(())
    }

    #[test]
    fn emoji_accepts_unicode_and_canonical_custom_syntax() -> anyhow::Result<()> {
        assert_eq!(
            DiscordEmoji::parse("working:123")?,
            DiscordEmoji::Custom {
                name: "working".to_owned(),
                id: 123,
            }
        );
        assert!(matches!(
            DiscordEmoji::parse("⏳")?,
            DiscordEmoji::Unicode(_)
        ));
        assert!(DiscordEmoji::parse("working").is_err());
        assert!(DiscordEmoji::parse("é").is_err());
        assert!(DiscordEmoji::parse("working:not-an-id").is_err());
        assert!(DiscordEmoji::parse("x:123").is_err());
        Ok(())
    }
}
