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
    pub providers: Vec<Provider>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discord: Option<DiscordConfig>,
    pub agent: AgentConfig,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CoreConfig {
    pub listen: SocketAddr,
    pub operator_token_hash: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DiscordConfig {
    pub provider: String,
    pub agent: String,
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
    pub workspace: PathBuf,
    pub runtime_provider: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Provider {
    pub id: String,
    pub role: ProviderRole,
    pub version: String,
    pub url: String,
    pub token_hash: String,
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderRole {
    Interface,
    Runtime,
}

#[derive(Clone, Debug)]
pub struct LoadedConfig {
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
        value.agent.workspace = absolutize(&root, &value.agent.workspace);
        value.validate()?;
        Ok(Self { root, value })
    }
    pub fn provider(&self, id: &str, role: ProviderRole) -> Result<&Provider> {
        self.value.provider(id, role)
    }

    pub fn discord(&self) -> Result<&DiscordConfig> {
        self.value.discord()
    }

    pub fn credential_path(&self, id: &str) -> Result<PathBuf> {
        validate_provider_identity(id)?;
        Ok(self.root.join("credentials").join(format!("{id}.token")))
    }

    pub fn credential(&self, id: &str) -> Result<String> {
        let path = self.credential_path(id)?;
        Ok(fs::read_to_string(&path)
            .with_context(|| format!("read provider credential {}", path.display()))?
            .trim()
            .to_owned())
    }
}

impl Config {
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

    pub(crate) fn validate(&self) -> Result<()> {
        if self.core.operator_token_hash.is_empty() {
            bail!("operator diagnostics credential is not configured");
        }
        let mut provider_ids = BTreeSet::new();
        for provider in &self.providers {
            validate_provider_identity(&provider.id)?;
            if provider.version.trim().is_empty()
                || provider.url.trim().is_empty()
                || provider.token_hash.trim().is_empty()
                || !provider_ids.insert(&provider.id)
            {
                bail!("provider versions, URLs, credentials, and IDs must be explicit and unique");
            }
        }
        self.provider(&self.agent.runtime_provider, ProviderRole::Runtime)?;
        Ok(())
    }

    pub fn discord(&self) -> Result<&DiscordConfig> {
        let discord = self
            .discord
            .as_ref()
            .context("Discord interface configuration is absent")?;
        discord.validate(self)?;
        Ok(discord)
    }

    pub fn sole_enabled_interface(&self) -> Option<&str> {
        let mut interfaces = self
            .providers
            .iter()
            .filter(|provider| provider.role == ProviderRole::Interface && provider.enabled);
        let interface = interfaces.next()?;
        interfaces.next().is_none().then_some(interface.id.as_str())
    }
}

impl DiscordConfig {
    fn validate(&self, config: &Config) -> Result<()> {
        if !valid_snowflake(&self.guild_id)
            || !valid_snowflake(&self.bot_user_id)
            || self.allowed_user_ids.is_empty()
            || self
                .allowed_user_ids
                .iter()
                .any(|user| !valid_snowflake(user))
            || self.routes.is_empty()
        {
            bail!("Discord admission requires immutable guild, bot, user, and channel route IDs");
        }
        let mut channels = BTreeSet::new();
        for route in &self.routes {
            if !valid_snowflake(&route.channel_id) || !channels.insert(&route.channel_id) {
                bail!("Discord route channel IDs must be valid and unique");
            }
        }
        for emoji in [
            &self.processing_emoji,
            &self.delivered_emoji,
            &self.failed_emoji,
            &self.uncertain_emoji,
        ] {
            DiscordEmoji::parse(emoji)?;
        }
        config.provider(&self.provider, ProviderRole::Interface)?;
        if self.agent != config.agent.id {
            bail!("Discord interface selects an unknown agent");
        }
        Ok(())
    }
}

impl ProviderRole {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Interface => "interface",
            Self::Runtime => "runtime",
        }
    }
}

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
        validate_provider_identity,
    };

    #[test]
    fn discord_configuration_receives_ingress_defaults() -> anyhow::Result<()> {
        let config: DiscordConfig = toml::from_str(
            r#"
provider = "discord-private"
agent = "default"
guild_id = "100"
allowed_user_ids = ["200"]
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
    fn provider_identity_uses_one_path_safe_slug_grammar() {
        for valid in ["a", "runtime-pi", "interface_2", "A9"] {
            assert!(
                validate_provider_identity(valid).is_ok(),
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
            assert!(
                validate_provider_identity(invalid).is_err(),
                "{invalid:?} should be invalid"
            );
        }
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
