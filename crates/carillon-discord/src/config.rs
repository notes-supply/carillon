//! The Discord provider's own configuration: `providers/<id>.toml` beside
//! `carillon.toml`. Core never reads it; only the provider and its
//! `configure` command know the schema.

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};

use anyhow::{Context as _, Result, bail};
use carillon_installation::config::{
    LoadedConfig, ProviderRole, absolutize, component_directory, validate_agent_identity,
};
use carillon_installation::{filesystem::atomic_write, maintenance::MaintenanceLock};
use carillon_protocol::turns::ControlAction;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, watch};
use twilight_model::channel::message::EmojiReactionType;
use unicode_general_category::{GeneralCategory, get_general_category};

use crate::module::{Change, ChangeError, Source};

/// The provider's settings file. It rejects fields it does not define, so a
/// misspelt setting fails validation instead of being dropped.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiscordConfig {
    pub(crate) guild_id: String,
    #[serde(default)]
    pub(crate) bot_user_id: String,
    /// The bot token's file, relative to the installation unless absolute;
    /// without it, `credentials/discord-bot.token`. Setup names a new file
    /// whenever the token changes, so the file these settings name is never
    /// rewritten.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) bot_token_file: Option<PathBuf>,
    pub(crate) allowed_user_ids: Vec<String>,
    #[serde(default)]
    pub(crate) routes: Vec<DiscordRoute>,
    #[serde(default = "enabled")]
    pub(crate) typing: bool,
    /// Show a card of what each job is doing, kept as its summary.
    #[serde(default = "enabled")]
    pub(crate) activity: bool,
    #[serde(default)]
    pub(crate) activity_markers: ActivityMarkers,
    #[serde(default)]
    pub(crate) attachments: AttachmentPolicy,
    #[serde(default = "enabled")]
    pub(crate) reactions: bool,
    #[serde(default = "processing_emoji")]
    pub(crate) processing_emoji: String,
    #[serde(default = "delivered_emoji")]
    pub(crate) delivered_emoji: String,
    #[serde(default = "failed_emoji")]
    pub(crate) failed_emoji: String,
    #[serde(default = "uncertain_emoji")]
    pub(crate) uncertain_emoji: String,
    /// Shown on a message its runtime steered into the turn running before
    /// it, whose answer covers it.
    #[serde(default = "steered_emoji")]
    pub(crate) steered_emoji: String,
    /// Shown on a message whose turn was stopped.
    #[serde(default = "stopped_emoji")]
    pub(crate) stopped_emoji: String,
    /// Offered on a message waiting behind a running turn: hand it to that
    /// turn now.
    #[serde(default = "steer_emoji")]
    pub(crate) steer_emoji: String,
    /// Offered likewise: end the running turn and run this message next.
    #[serde(default = "interrupt_emoji")]
    pub(crate) interrupt_emoji: String,
    /// Offered likewise: do not run this message.
    #[serde(default = "stop_emoji")]
    pub(crate) stop_emoji: String,
    #[serde(default, skip_serializing_if = "ModuleSetting::is_unset")]
    pub(crate) module: ModuleSetting,
}

/// A custom Discord module: served while `enabled`, from `path`, relative to
/// the installation unless absolute. Without it the release's bundled module
/// serves.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModuleSetting {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

impl ModuleSetting {
    fn is_unset(&self) -> bool {
        *self == Self::default()
    }
}

/// The bot token's file when the settings name none, as for a token put
/// there by hand.
pub(crate) const BOT_TOKEN_FILE: &str = "credentials/discord-bot.token";

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

fn steered_emoji() -> String {
    "↪️".to_owned()
}

fn stopped_emoji() -> String {
    "⏹️".to_owned()
}

fn steer_emoji() -> String {
    "⏩".to_owned()
}

fn interrupt_emoji() -> String {
    "⏭️".to_owned()
}

fn stop_emoji() -> String {
    "🛑".to_owned()
}

/// Inline card markers, independent of the provider's reaction emoji. The
/// Discord module shows them; this configuration validates them.
pub(crate) use carillon_discord_module::contract::ActivityMarkers;

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
pub(crate) struct AttachmentPolicy {
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
#[serde(deny_unknown_fields)]
pub struct DiscordRoute {
    pub channel_id: String,
    /// The agent the channel's conversations belong to.
    pub agent: String,
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
        let agent = fields.next().unwrap_or_default();
        if fields.next().is_some()
            || !valid_snowflake(channel_id)
            || validate_agent_identity(agent).is_err()
        {
            bail!("route must be CHANNEL_ID:mention|always:thread|channel:AGENT_ID");
        }
        Ok(Self {
            channel_id: channel_id.to_owned(),
            agent: agent.to_owned(),
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

    /// The same reaction to Discord: a custom emoji by its ID, a Unicode
    /// one with or without its variation selector.
    fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Unicode(one), Self::Unicode(other)) => {
                one.replace('\u{fe0f}', "") == other.replace('\u{fe0f}', "")
            }
            (Self::Custom { id: one, .. }, Self::Custom { id: other, .. }) => one == other,
            _ => false,
        }
    }
}

impl DiscordConfig {
    /// A fresh configuration carrying the same feedback defaults that
    /// deserialization applies.
    #[must_use]
    pub fn with_defaults(
        guild_id: String,
        bot_user_id: String,
        allowed_user_ids: Vec<String>,
        routes: Vec<DiscordRoute>,
    ) -> Self {
        Self {
            guild_id,
            bot_user_id,
            bot_token_file: None,
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
            steered_emoji: steered_emoji(),
            stopped_emoji: stopped_emoji(),
            steer_emoji: steer_emoji(),
            interrupt_emoji: interrupt_emoji(),
            stop_emoji: stop_emoji(),
            module: ModuleSetting::default(),
        }
    }

    /// The reaction offered for `action`.
    pub(crate) fn control_emoji(&self, action: ControlAction) -> &str {
        match action {
            ControlAction::Steer => &self.steer_emoji,
            ControlAction::Interrupt => &self.interrupt_emoji,
            ControlAction::Stop => &self.stop_emoji,
        }
    }

    /// The action a person's reaction asks for, when it is one of the
    /// controls.
    pub(crate) fn control_action(&self, reaction: &EmojiReactionType) -> Option<ControlAction> {
        let reaction = match reaction {
            EmojiReactionType::Unicode { name } => DiscordEmoji::Unicode(name.clone()),
            EmojiReactionType::Custom { id, name, .. } => DiscordEmoji::Custom {
                name: name.clone().unwrap_or_default(),
                id: id.get(),
            },
        };
        [
            ControlAction::Steer,
            ControlAction::Interrupt,
            ControlAction::Stop,
        ]
        .into_iter()
        .find(|action| {
            DiscordEmoji::parse(self.control_emoji(*action))
                .is_ok_and(|emoji| emoji.same(&reaction))
        })
    }

    /// The bot token's file these settings name, resolved against the
    /// installation `root`.
    #[must_use]
    pub(crate) fn bot_token_path(&self, root: &Path) -> PathBuf {
        absolutize(
            root,
            self.bot_token_file
                .as_deref()
                .unwrap_or_else(|| Path::new(BOT_TOKEN_FILE)),
        )
    }

    /// The module these settings serve, with a custom path resolved against
    /// the installation `root`.
    #[must_use]
    pub(crate) fn module_source(&self, root: &Path) -> Source {
        match &self.module {
            ModuleSetting {
                enabled: true,
                path: Some(path),
            } => Source::Custom {
                path: root.join(path),
            },
            ModuleSetting { .. } => Source::Bundled,
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
        if self
            .bot_token_file
            .as_ref()
            .is_some_and(|path| path.as_os_str().is_empty())
        {
            bail!("bot_token_file must name the bot token's file");
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
        let feedback = [
            &self.processing_emoji,
            &self.delivered_emoji,
            &self.failed_emoji,
            &self.uncertain_emoji,
            &self.steered_emoji,
            &self.stopped_emoji,
        ]
        .map(|emoji| DiscordEmoji::parse(emoji))
        .into_iter()
        .collect::<Result<Vec<_>>>()?;
        let controls = [&self.steer_emoji, &self.interrupt_emoji, &self.stop_emoji]
            .map(|emoji| DiscordEmoji::parse(emoji))
            .into_iter()
            .collect::<Result<Vec<_>>>()?;
        for (index, control) in controls.iter().enumerate() {
            if feedback
                .iter()
                .chain(&controls[..index])
                .any(|other| other.same(control))
            {
                bail!(
                    "steer_emoji, interrupt_emoji and stop_emoji must differ from each other and from the feedback emoji"
                );
            }
        }
        validate_activity_markers(&self.activity_markers)?;
        if self.module.enabled
            && self
                .module
                .path
                .as_ref()
                .is_none_or(|path| path.as_os_str().is_empty())
        {
            bail!("module.enabled needs module.path, the custom Discord module to serve");
        }
        self.attachments
            .validate()
            .context("invalid Discord attachments settings")?;
        config.provider(provider_id, ProviderRole::Interface)?;
        for route in &self.routes {
            config
                .value
                .agent(&route.agent)
                .with_context(|| format!("route {} names an unknown agent", route.channel_id))?;
        }
        Ok(())
    }
}

/// How a settings file differs from the settings that serve, by field:
/// what an apply would change, and what only a restart applies.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct Difference {
    pub live: Vec<String>,
    pub restart: Vec<String>,
}

impl Difference {
    /// Every field that differs, classified. Every field is listed here, so
    /// a new one must be classified before it compiles. Identity and the
    /// capacity the provider set up at start apply at a restart; who is
    /// admitted where, and how work is shown, apply live.
    #[must_use]
    pub(crate) fn between(serving: &DiscordConfig, file: &DiscordConfig) -> Self {
        let DiscordConfig {
            guild_id,
            bot_user_id,
            bot_token_file,
            allowed_user_ids,
            routes,
            typing,
            activity,
            activity_markers,
            attachments,
            reactions,
            processing_emoji,
            delivered_emoji,
            failed_emoji,
            uncertain_emoji,
            steered_emoji,
            stopped_emoji,
            steer_emoji,
            interrupt_emoji,
            stop_emoji,
            module,
        } = file;
        let mut difference = Self::default();
        for (changed, field, live) in [
            (*guild_id != serving.guild_id, "guild_id", false),
            (*bot_user_id != serving.bot_user_id, "bot_user_id", false),
            (
                *bot_token_file != serving.bot_token_file,
                "bot_token_file",
                false,
            ),
            (*activity != serving.activity, "activity", false),
            (*attachments != serving.attachments, "attachments", false),
            (
                *allowed_user_ids != serving.allowed_user_ids,
                "allowed_user_ids",
                true,
            ),
            (*routes != serving.routes, "routes", true),
            (*typing != serving.typing, "typing", true),
            (
                *activity_markers != serving.activity_markers,
                "activity_markers",
                true,
            ),
            (*reactions != serving.reactions, "reactions", true),
            (
                *processing_emoji != serving.processing_emoji,
                "processing_emoji",
                true,
            ),
            (
                *delivered_emoji != serving.delivered_emoji,
                "delivered_emoji",
                true,
            ),
            (*failed_emoji != serving.failed_emoji, "failed_emoji", true),
            (
                *uncertain_emoji != serving.uncertain_emoji,
                "uncertain_emoji",
                true,
            ),
            (
                *steered_emoji != serving.steered_emoji,
                "steered_emoji",
                true,
            ),
            (
                *stopped_emoji != serving.stopped_emoji,
                "stopped_emoji",
                true,
            ),
            (*steer_emoji != serving.steer_emoji, "steer_emoji", true),
            (
                *interrupt_emoji != serving.interrupt_emoji,
                "interrupt_emoji",
                true,
            ),
            (*stop_emoji != serving.stop_emoji, "stop_emoji", true),
            (*module != serving.module, "module", true),
        ] {
            if changed {
                if live {
                    &mut difference.live
                } else {
                    &mut difference.restart
                }
                .push(field.to_owned());
            }
        }
        difference
    }
}

/// The settings the provider serves: one validated file version at a time.
/// Each message or interaction keeps the snapshot it was admitted with, so
/// an apply changes only what is admitted after it, and shows work as it
/// says from then on.
#[derive(Clone)]
pub(crate) struct Settings(Arc<Serving>);

struct Serving {
    current: watch::Sender<Arc<DiscordConfig>>,
    /// One apply at a time, so the file each reads is the one that serves.
    applying: Mutex<()>,
}

impl Settings {
    #[must_use]
    pub(crate) fn new(config: DiscordConfig) -> Self {
        Self(Arc::new(Serving {
            current: watch::Sender::new(Arc::new(config)),
            applying: Mutex::new(()),
        }))
    }

    #[must_use]
    pub(crate) fn current(&self) -> Arc<DiscordConfig> {
        self.0.current.borrow().clone()
    }

    /// What serves, and how the file `read` returns differs from it.
    /// `module_differs` says whether the module the file declares is not the
    /// one serving, which its bytes decide as well as its declaration.
    #[must_use]
    pub(crate) fn status(
        &self,
        read: impl FnOnce() -> Result<DiscordConfig>,
        module_differs: impl FnOnce(&DiscordConfig) -> bool,
    ) -> Status {
        let serving = self.current();
        let file = match read() {
            Ok(file) => {
                let mut difference = Difference::between(&serving, &file);
                if module_differs(&file) && !difference.live.iter().any(|field| field == "module") {
                    difference.live.push("module".to_owned());
                }
                FileState::Read(difference)
            }
            Err(error) => FileState::Invalid {
                error: format!("{error:#}"),
            },
        };
        Status {
            serving: DiscordConfig::clone(&serving),
            file,
        }
    }

    /// Serves the file `read` returns in place of what serves, whole, when
    /// everything that differs applies live: first the module it declares,
    /// through `module`, then the rest. Returns the fields it changed and
    /// what became of the module.
    ///
    /// # Errors
    ///
    /// Returns why what serves stays in place: the file did not read as
    /// valid settings, it changes what only a restart applies, or its module
    /// could not serve.
    pub(crate) async fn apply<F>(
        &self,
        read: impl FnOnce() -> Result<DiscordConfig>,
        module: impl FnOnce(&DiscordConfig) -> F,
    ) -> Result<Applied, Refusal>
    where
        F: Future<Output = Result<Change, ChangeError>>,
    {
        let _one = self.0.applying.lock().await;
        let file = read().map_err(Refusal::Invalid)?;
        let mut difference = Difference::between(&self.current(), &file);
        if !difference.restart.is_empty() {
            return Err(Refusal::Restart(difference));
        }
        let module = module(&file).await.map_err(Refusal::Module)?;
        if module.serving != module.previous
            && !difference.live.iter().any(|field| field == "module")
        {
            difference.live.push("module".to_owned());
        }
        if !difference.live.is_empty() {
            self.0.current.send_replace(Arc::new(file));
        }
        Ok(Applied {
            changed: difference.live,
            module,
        })
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Status {
    pub serving: DiscordConfig,
    pub file: FileState,
}

/// The settings file as an apply would read it now.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum FileState {
    /// Valid settings, differing from what serves by these fields.
    Read(Difference),
    /// Not valid settings for this provider; an apply is refused.
    Invalid { error: String },
}

/// The fields an apply changed, none when the file already served, and the
/// module serving after it.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Applied {
    pub changed: Vec<String>,
    pub module: Change,
}

/// Why an apply left the serving settings and module in place.
#[derive(Debug)]
pub(crate) enum Refusal {
    Invalid(anyhow::Error),
    /// The file changes fields only a restart applies.
    Restart(Difference),
    /// The module the file declares could not serve.
    Module(ChangeError),
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
    admitted(
        config,
        provider_id,
        guild_id,
        bot_user_id,
        allowed_user_ids,
        routes,
    )?
    .write(config, provider_id)
}

/// The settings [`configure`] writes, not yet validated: the existing file,
/// or defaults, with these admission settings.
pub(crate) fn admitted(
    config: &LoadedConfig,
    provider_id: &str,
    guild_id: &str,
    bot_user_id: &str,
    allowed_user_ids: &[String],
    routes: &[DiscordRoute],
) -> Result<DiscordConfig> {
    let mut discord = if config.provider_config_path(provider_id)?.exists() {
        DiscordConfig::read(config, provider_id)?
    } else {
        DiscordConfig::with_defaults(String::new(), String::new(), Vec::new(), Vec::new())
    };
    guild_id.clone_into(&mut discord.guild_id);
    bot_user_id.clone_into(&mut discord.bot_user_id);
    discord.allowed_user_ids = allowed_user_ids.to_vec();
    discord.routes = routes.to_vec();
    Ok(discord)
}

/// Changes the module an existing settings file declares, keeping the rest,
/// under the installation's maintenance lock: while another maintenance
/// operation holds it, the file is left as it is.
pub(crate) fn declare_module(
    config: &LoadedConfig,
    provider_id: &str,
    change: impl FnOnce(&mut ModuleSetting),
) -> Result<()> {
    let _maintenance = MaintenanceLock::acquire_unless_held(&config.root)
        .context("the settings file was not changed")?;
    let mut discord = DiscordConfig::read(config, provider_id)?;
    change(&mut discord.module);
    discord.write(config, provider_id)
}

/// The instance's private state: participation, the command registration
/// record and attachment staging.
pub(crate) fn state_directory(config: &LoadedConfig, provider_id: &str) -> PathBuf {
    component_directory(&config.value.data_dir, provider_id)
}

fn valid_snowflake(value: &str) -> bool {
    value
        .parse::<u64>()
        .is_ok_and(|id| id != 0 && id.to_string() == value)
}

#[cfg(test)]
pub(crate) mod tests;
