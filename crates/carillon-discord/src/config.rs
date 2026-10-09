//! The Discord provider's own configuration: `providers/<id>.toml` beside
//! `carillon.toml`. Core never reads it; only the provider and its
//! `configure` command know the schema.

use std::{collections::BTreeSet, fs, path::PathBuf, str::FromStr};

use anyhow::{Context as _, Result, bail};
use carillon_installation::config::{LoadedConfig, ProviderRole, component_directory};
use carillon_installation::filesystem::atomic_write;
use serde::{Deserialize, Serialize};
use unicode_general_category::{GeneralCategory, get_general_category};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DiscordConfig {
    pub agent: String,
    pub guild_id: String,
    #[serde(default)]
    pub bot_user_id: String,
    pub allowed_user_ids: Vec<String>,
    #[serde(default)]
    pub routes: Vec<DiscordRoute>,
    #[serde(default = "enabled")]
    pub typing: bool,
    /// Show a card of what each job is doing, kept as its summary.
    #[serde(default = "enabled")]
    pub activity: bool,
    #[serde(default)]
    pub activity_markers: ActivityMarkers,
    /// Files uploaded with admitted messages.
    #[serde(default)]
    pub attachments: AttachmentPolicy,
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

/// Inline card markers, independent of the provider's reaction emoji. The
/// Discord module shows them; this configuration validates them.
pub use carillon_discord_module::contract::ActivityMarkers;

fn validate_activity_markers(markers: &ActivityMarkers) -> Result<()> {
    for (field, value) in [
        ("working", &markers.working),
        ("queued", &markers.queued),
        ("finishing", &markers.finishing),
        ("running", &markers.running),
        ("succeeded", &markers.succeeded),
        ("failed", &markers.failed),
        ("uncertain", &markers.uncertain),
        ("stopped", &markers.stopped),
    ] {
        validate_activity_marker(value)
            .with_context(|| format!("invalid Discord activity_markers.{field}"))?;
    }
    if markers.models.len() > 32 {
        bail!("activity_markers.models must contain at most 32 prefixes");
    }
    for (prefix, marker) in &markers.models {
        if prefix.is_empty() || prefix.len() > 63 {
            bail!("activity_markers.models prefixes must contain 1–63 UTF-8 bytes");
        }
        validate_activity_marker(marker)
            .with_context(|| format!("invalid activity_markers.models marker for {prefix:?}"))?;
    }
    Ok(())
}

fn validate_activity_marker(value: &str) -> Result<()> {
    if value.len() > 64 {
        bail!("activity marker must be at most 64 UTF-8 bytes");
    }
    if value.is_empty() || emojis::get(value).is_some() {
        return Ok(());
    }
    if value.contains(':') || value.starts_with('<') {
        let emoji = value
            .strip_prefix("<a:")
            .or_else(|| value.strip_prefix("<:"))
            .and_then(|emoji| emoji.strip_suffix('>'))
            .map(DiscordEmoji::parse);
        if matches!(emoji, Some(Ok(DiscordEmoji::Custom { .. }))) {
            return Ok(());
        }
        bail!(
            "custom activity marker must use <:name:id> (static) or <a:name:id> (animated), with a 2–32 character ASCII emoji name and canonical snowflake ID"
        );
    }
    // Recognized emoji above may contain joiners or variation selectors. Other
    // markers must be visible symbols, without Discord mention/markdown syntax.
    if !value.chars().all(|character| {
        !"<>@`*_~|\\[]()#+-".contains(character)
            && matches!(
                get_general_category(character),
                GeneralCategory::MathSymbol
                    | GeneralCategory::CurrencySymbol
                    | GeneralCategory::ModifierSymbol
                    | GeneralCategory::OtherSymbol
                    | GeneralCategory::DashPunctuation
                    | GeneralCategory::OtherPunctuation
                    | GeneralCategory::OpenPunctuation
                    | GeneralCategory::ClosePunctuation
                    | GeneralCategory::InitialPunctuation
                    | GeneralCategory::FinalPunctuation
            )
    }) {
        bail!(
            "activity marker must be empty, Unicode emoji/symbols, or a custom Discord emote; \
             whitespace, mentions, markdown, and control characters are not allowed"
        );
    }
    Ok(())
}

/// What the provider downloads from Discord for an admitted message before
/// core captures it. Sizes are MiB. Downloads wait for staging room; core
/// applies its own limits again. Changes apply when the provider restarts.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct AttachmentPolicy {
    /// Without it, a message carrying files is refused with an explanation
    /// rather than accepted without them.
    pub enabled: bool,
    /// Discord allows at most 10 files on one message.
    pub max_files: u32,
    pub max_file_mib: u64,
    pub max_message_mib: u64,
    /// Messages downloading files at once, across conversations.
    pub concurrent_messages: u32,
    /// Bytes reserved for downloaded files until core has captured them.
    pub staging_mib: u64,
}

impl Default for AttachmentPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            max_files: 10,
            max_file_mib: 1024,
            max_message_mib: 2048,
            concurrent_messages: 2,
            staging_mib: 2048,
        }
    }
}

impl AttachmentPolicy {
    fn validate(&self) -> Result<()> {
        const MAX_MIB: u64 = 1024 * 1024;
        if !(1..=10).contains(&self.max_files) {
            bail!("attachments.max_files must be between 1 and 10");
        }
        if !(1..=MAX_MIB).contains(&self.max_file_mib) {
            bail!("attachments.max_file_mib must be between 1 and {MAX_MIB}");
        }
        if !(self.max_file_mib..=MAX_MIB).contains(&self.max_message_mib) {
            bail!("attachments.max_message_mib must be between max_file_mib and {MAX_MIB}");
        }
        if !(self.max_message_mib..=MAX_MIB).contains(&self.staging_mib) {
            bail!("attachments.staging_mib must be between max_message_mib and {MAX_MIB}");
        }
        if !(1..=16).contains(&self.concurrent_messages) {
            bail!("attachments.concurrent_messages must be between 1 and 16");
        }
        Ok(())
    }
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
pub(crate) enum DiscordEmoji {
    Unicode(String),
    Custom { name: String, id: u64 },
}

impl DiscordEmoji {
    pub(crate) fn parse(value: &str) -> Result<Self> {
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

impl DiscordConfig {
    /// A fresh configuration carrying the same feedback defaults that
    /// deserialization applies.
    #[must_use]
    pub fn with_defaults(
        agent: String,
        guild_id: String,
        bot_user_id: String,
        allowed_user_ids: Vec<String>,
        routes: Vec<DiscordRoute>,
    ) -> Self {
        Self {
            agent,
            guild_id,
            bot_user_id,
            allowed_user_ids,
            routes,
            typing: enabled(),
            activity: enabled(),
            activity_markers: ActivityMarkers::default(),
            attachments: AttachmentPolicy::default(),
            reactions: enabled(),
            processing_emoji: processing_emoji(),
            delivered_emoji: delivered_emoji(),
            failed_emoji: failed_emoji(),
            uncertain_emoji: uncertain_emoji(),
        }
    }

    /// Reads and validates the provider's file for a registered interface
    /// provider.
    pub(crate) fn load(config: &LoadedConfig, provider_id: &str) -> Result<Self> {
        let discord = Self::read(config, provider_id)?;
        discord.validate(config, provider_id)?;
        Ok(discord)
    }

    /// Parses the provider's file without validating it against the registry.
    pub(crate) fn read(config: &LoadedConfig, provider_id: &str) -> Result<Self> {
        let path = config.provider_config_path(provider_id)?;
        let text = fs::read_to_string(&path)
            .with_context(|| format!("read Discord configuration {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))
    }

    /// Writes the file for a registered interface provider, readable only by
    /// the installation owner like the rest of the installation.
    pub(crate) fn write(&self, config: &LoadedConfig, provider_id: &str) -> Result<()> {
        self.validate(config, provider_id)?;
        let path = config.provider_config_path(provider_id)?;
        let parent = path
            .parent()
            .context("provider configuration has no parent")?;
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        atomic_write(&path, toml::to_string_pretty(self)?.as_bytes(), 0o600)
            .with_context(|| format!("write {}", path.display()))
    }

    pub(crate) fn validate(&self, config: &LoadedConfig, provider_id: &str) -> Result<()> {
        if !valid_snowflake(&self.guild_id) {
            bail!("Discord guild ID must be an immutable snowflake ID");
        }
        if !valid_snowflake(&self.bot_user_id) {
            bail!("Discord bot user ID must be an immutable snowflake ID");
        }
        if self.allowed_user_ids.is_empty() {
            bail!("Discord admission requires at least one allowlisted user ID");
        }
        if let Some(user) = self
            .allowed_user_ids
            .iter()
            .find(|user| !valid_snowflake(user))
        {
            bail!("Discord allowlisted user ID {user:?} is not a snowflake ID");
        }
        if self.routes.is_empty() {
            bail!("Discord admission requires at least one channel route");
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
        validate_activity_markers(&self.activity_markers)?;
        self.attachments
            .validate()
            .context("invalid Discord attachments settings")?;
        config.provider(provider_id, ProviderRole::Interface)?;
        if self.agent != config.value.agent.id {
            bail!("Discord interface selects an unknown agent");
        }
        Ok(())
    }
}

/// Replaces the admission settings of an existing file, or creates the file,
/// keeping any feedback settings the operator tuned by hand.
pub(crate) fn configure(
    config: &LoadedConfig,
    provider_id: &str,
    guild_id: &str,
    bot_user_id: &str,
    allowed_user_ids: &[String],
    routes: &[DiscordRoute],
) -> Result<()> {
    let mut discord = if config.provider_config_path(provider_id)?.exists() {
        DiscordConfig::read(config, provider_id)?
    } else {
        DiscordConfig::with_defaults(
            config.value.agent.id.clone(),
            String::new(),
            String::new(),
            Vec::new(),
            Vec::new(),
        )
    };
    discord.agent.clone_from(&config.value.agent.id);
    guild_id.clone_into(&mut discord.guild_id);
    bot_user_id.clone_into(&mut discord.bot_user_id);
    discord.allowed_user_ids = allowed_user_ids.to_vec();
    discord.routes = routes.to_vec();
    discord.write(config, provider_id)
}

/// The instance's private state: participation, the installed command
/// module, the command registration record and attachment staging.
pub(crate) fn state_directory(config: &LoadedConfig, provider_id: &str) -> PathBuf {
    component_directory(&config.value.data_dir, provider_id)
}

fn valid_snowflake(value: &str) -> bool {
    value
        .parse::<u64>()
        .is_ok_and(|id| id != 0 && id.to_string() == value)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{collections::BTreeMap, fs, path::Path, str::FromStr};

    use anyhow::Context as _;
    use carillon_installation::config::{
        AgentConfig, Config, CoreConfig, LoadedConfig, Provider, ProviderRole, hash_token,
    };

    use super::{
        ActivityMarkers, DiscordActivation, DiscordConfig, DiscordConversation, DiscordEmoji,
        DiscordRoute, validate_activity_marker, validate_activity_markers,
    };

    /// A complete installation on disk: `carillon.toml`, credentials, and
    /// the Discord provider file, with loopback provider endpoints on
    /// unbound ports.
    pub(crate) fn installation(
        root: &Path,
        interface_id: &str,
        core_port: u16,
    ) -> anyhow::Result<LoadedConfig> {
        let resources = root.join("resources");
        fs::create_dir_all(&resources)?;
        fs::write(resources.join("SYSTEM.md"), "test instructions")?;
        fs::create_dir_all(root.join("credentials"))?;
        fs::create_dir_all(root.join("state"))?;
        fs::create_dir_all(root.join("workspace"))?;
        let mut providers = Vec::new();
        for (port, (id, role)) in [
            (interface_id, ProviderRole::Interface),
            ("test-runtime", ProviderRole::Runtime),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, provider)| {
            (
                core_port + 1 + u16::try_from(index).unwrap_or(u16::MAX),
                provider,
            )
        }) {
            fs::write(root.join("credentials").join(format!("{id}.token")), id)?;
            providers.push(Provider {
                id: id.to_owned(),
                role,
                version: carillon_installation::config::RELEASE_VERSION.to_owned(),
                url: format!("http://127.0.0.1:{port}"),
                token_hash: hash_token(id),
                enabled: true,
                parallel_turns: None,
            });
        }
        fs::write(root.join("credentials").join("operator.token"), "operator")?;
        let value = Config {
            data_dir: root.join("state"),
            core: CoreConfig {
                listen: format!("127.0.0.1:{core_port}").parse()?,
                operator_token_hash: hash_token("operator"),
                attachments: carillon_installation::config::AttachmentLimits::default(),
            },
            providers,
            agent: AgentConfig {
                id: "default".to_owned(),
                name: "Test agent".to_owned(),
                resource_root: resources,
                workspace: root.join("workspace"),
                runtime_provider: "test-runtime".to_owned(),
            },
        };
        let path = root.join("carillon.toml");
        fs::write(&path, toml::to_string(&value)?)?;
        let config = LoadedConfig::load(&path)?;
        DiscordConfig::with_defaults(
            "default".to_owned(),
            "100".to_owned(),
            "101".to_owned(),
            vec!["200".to_owned()],
            vec!["300:always:channel".parse()?],
        )
        .write(&config, interface_id)?;
        Ok(config)
    }

    #[test]
    fn discord_feedback_defaults_leave_admission_unconfigured() -> anyhow::Result<()> {
        let config: DiscordConfig = toml::from_str(
            r#"
agent = "default"
guild_id = "100"
allowed_user_ids = ["200"]
"#,
        )?;
        assert!(config.bot_user_id.is_empty());
        assert!(config.routes.is_empty());
        assert!(config.typing);
        assert!(config.activity);
        assert!(config.reactions);
        assert_eq!(config.processing_emoji, "👀");
        assert_eq!(config.delivered_emoji, "✅");
        assert_eq!(config.failed_emoji, "❌");
        assert_eq!(config.uncertain_emoji, "⚠️");
        assert_eq!(
            config.activity_markers,
            ActivityMarkers {
                working: "⏳".to_owned(),
                queued: "⏳".to_owned(),
                finishing: "⏳".to_owned(),
                running: "↳".to_owned(),
                succeeded: "✓".to_owned(),
                failed: "✗".to_owned(),
                uncertain: "⚠️".to_owned(),
                stopped: "⏹️".to_owned(),
                models: BTreeMap::new(),
            }
        );
        Ok(())
    }

    #[test]
    fn activity_markers_default_individually() -> anyhow::Result<()> {
        let partial: ActivityMarkers = toml::from_str(
            r#"
working = "<a:working:123>"
failed = ""
"#,
        )?;
        assert_eq!(partial.working, "<a:working:123>");
        assert_eq!(partial.queued, "⏳");
        assert_eq!(partial.finishing, "⏳");
        assert_eq!(partial.running, "↳");
        assert_eq!(partial.succeeded, "✓");
        assert_eq!(partial.failed, "");
        assert_eq!(partial.uncertain, "⚠️");
        assert_eq!(partial.stopped, "⏹️");
        validate_activity_markers(&partial)?;

        assert!(toml::from_str::<ActivityMarkers>("suceeded = '✓'").is_err());
        Ok(())
    }

    #[test]
    fn activity_markers_accept_symbols_emoji_and_canonical_inline_emotes() -> anyhow::Result<()> {
        for marker in [
            "",
            "✓",
            "↳",
            "◇",
            "•",
            "⚠️",
            "👩🏽‍💻",
            "🇺🇦",
            "1️⃣",
            "<:tool:456>",
            "<a:working:123>",
        ] {
            validate_activity_marker(marker)
                .with_context(|| format!("rejected valid marker {marker:?}"))?;
        }
        validate_activity_marker(&"✓".repeat(21))?;
        assert!(validate_activity_marker(&"✓".repeat(22)).is_err());
        Ok(())
    }

    #[test]
    fn model_markers_validate_prefixes_and_inline_emotes() {
        let mut markers = ActivityMarkers::default();
        markers
            .models
            .insert("claude-".to_owned(), "<:claude:123>".to_owned());
        assert!(validate_activity_markers(&markers).is_ok());
        markers
            .models
            .insert("gpt-".to_owned(), "<@123>".to_owned());
        assert!(validate_activity_markers(&markers).is_err());
        markers.models.remove("gpt-");
        markers.models.insert("x".repeat(63), "✓".to_owned());
        assert!(validate_activity_markers(&markers).is_ok());
        markers.models.insert("x".repeat(64), "✓".to_owned());
        assert!(validate_activity_markers(&markers).is_err());
        markers.models.remove(&"x".repeat(64));
        markers.models.insert(String::new(), "✓".to_owned());
        assert!(validate_activity_markers(&markers).is_err());
    }

    #[test]
    fn activity_markers_reject_mentions_markdown_controls_and_malformed_emotes() {
        for marker in [
            "@everyone",
            "@here",
            "<@123>",
            "<@&123>",
            "<#123>",
            "working:123",
            "<:x:123>",
            "<:working:0123>",
            "<a:working:0>",
            "<:working:18446744073709551616>",
            "<a:working:not-an-id>",
            "<a:bad-name:123>",
            "<A:working:123>",
            "<:✓>",
            "<:working:123><:tool:456>",
            "<:working:123>suffix",
            "✓\n↳",
            "✓\r↳",
            "✓\t↳",
            " ✓",
            "✓ ",
            "✓\u{2028}↳",
            "✓\u{2029}↳",
            "✓\u{0000}",
            "✓\u{007f}",
            "✓\u{0085}",
            "\u{202e}✓",
            "\u{2066}✓\u{2069}",
            "\u{200b}",
            "\u{200d}",
            "\u{feff}",
            "`✓`",
            "**✓**",
            "__✓__",
            "~~✓~~",
            "||✓||",
            "[✓](https://example.test)",
            ">",
            "#",
            "-",
            "+",
            "\\",
            "word",
        ] {
            assert!(
                validate_activity_marker(marker).is_err(),
                "accepted invalid marker {marker:?}"
            );
        }
    }

    #[test]
    fn activity_marker_errors_identify_each_field_and_prevent_load_or_write() -> anyhow::Result<()>
    {
        for field in [
            "working",
            "queued",
            "finishing",
            "running",
            "succeeded",
            "failed",
            "uncertain",
            "stopped",
        ] {
            let markers: ActivityMarkers = toml::from_str(&format!("{field} = '@everyone'"))?;
            let error = validate_activity_markers(&markers)
                .err()
                .context("accepted mention")?;
            assert!(
                error
                    .to_string()
                    .contains(&format!("activity_markers.{field}"))
            );
        }

        let directory = tempfile::tempdir()?;
        let config = installation(directory.path(), "discord-private", 45_310)?;
        let path = config.provider_config_path("discord-private")?;
        let original = fs::read_to_string(&path)?;
        let mut invalid = DiscordConfig::load(&config, "discord-private")?;
        invalid.activity = false;
        "working:123".clone_into(&mut invalid.activity_markers.working);
        let error = invalid
            .write(&config, "discord-private")
            .err()
            .context("wrote invalid marker")?;
        assert!(error.to_string().contains("activity_markers.working"));
        assert!(format!("{error:#}").contains("<a:name:id>"));
        assert_eq!(fs::read_to_string(&path)?, original);

        fs::write(&path, toml::to_string(&invalid)?)?;
        let error = DiscordConfig::load(&config, "discord-private")
            .err()
            .context("loaded invalid marker")?;
        assert!(error.to_string().contains("activity_markers.working"));
        Ok(())
    }

    #[test]
    fn configure_preserves_tuned_feedback_and_rejects_unknown_providers() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir()?;
        let config = installation(directory.path(), "discord-private", 45_300)?;
        let path = config.provider_config_path("discord-private")?;
        assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
        let mut tuned = DiscordConfig::load(&config, "discord-private")?;
        tuned.reactions = false;
        "⏳".clone_into(&mut tuned.processing_emoji);
        tuned.activity_markers = toml::from_str(
            r#"
working = "<a:working:123>"
queued = "⌛"
finishing = "◇"
running = "<:tool:456>"
succeeded = "✔"
failed = ""
uncertain = "？"
stopped = "■"
"#,
        )?;
        tuned.write(&config, "discord-private")?;
        super::configure(
            &config,
            "discord-private",
            "110",
            "111",
            &["210".to_owned()],
            &["310:mention:thread".parse()?],
        )?;
        let reloaded = DiscordConfig::load(&config, "discord-private")?;
        assert_eq!(reloaded.guild_id, "110");
        assert_eq!(reloaded.bot_user_id, "111");
        assert_eq!(reloaded.allowed_user_ids, ["210"]);
        assert_eq!(reloaded.routes[0].channel_id, "310");
        assert!(!reloaded.reactions);
        assert_eq!(reloaded.processing_emoji, "⏳");
        assert_eq!(
            reloaded.activity_markers,
            ActivityMarkers {
                working: "<a:working:123>".to_owned(),
                queued: "⌛".to_owned(),
                finishing: "◇".to_owned(),
                running: "<:tool:456>".to_owned(),
                succeeded: "✔".to_owned(),
                failed: String::new(),
                uncertain: "？".to_owned(),
                stopped: "■".to_owned(),
                models: BTreeMap::new(),
            }
        );
        assert!(
            super::configure(
                &config,
                "test-runtime",
                "110",
                "111",
                &["210".to_owned()],
                &["310:mention:thread".parse()?],
            )
            .is_err()
        );
        assert!(
            super::configure(
                &config,
                "unregistered",
                "110",
                "111",
                &["210".to_owned()],
                &["310:mention:thread".parse()?],
            )
            .is_err()
        );
        assert!(!config.provider_config_path("unregistered")?.exists());
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
