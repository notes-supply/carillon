use std::{collections::BTreeMap, fmt::Display, fs, path::Path, str::FromStr};

use anyhow::Context as _;
use carillon_installation::config::{
    AgentConfig, Config, CoreConfig, LoadedConfig, Provider, ProviderRole, hash_token,
};

use super::{
    ActivityMarkers, Difference, DiscordActivation, DiscordConfig, DiscordConversation,
    DiscordEmoji, DiscordRoute, validate_activity_marker, validate_activity_markers,
};

fn refusal<T, E: Display>(result: Result<T, E>) -> Option<String> {
    result.err().map(|error| error.to_string())
}

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
            max_upload_mib: (role == ProviderRole::Interface).then_some(20),
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
        agents: vec![AgentConfig {
            id: "default".to_owned(),
            name: "Test agent".to_owned(),
            runtime: "test-runtime".to_owned(),
            workspace: root.join("workspace"),
            resources: Some(resources),
        }],
    };
    let path = root.join("carillon.toml");
    fs::write(&path, toml::to_string(&value)?)?;
    let config = LoadedConfig::load(&path)?;
    DiscordConfig::with_defaults(
        "100".to_owned(),
        "101".to_owned(),
        vec!["200".to_owned()],
        vec!["300:always:channel:default".parse()?],
    )
    .write(&config, interface_id)?;
    Ok(config)
}

/// A deadline no test reaches.
pub(crate) fn later() -> tokio::time::Instant {
    tokio::time::Instant::now() + std::time::Duration::from_secs(60)
}

/// The writer of a complete installation's Discord settings, serving them
/// as written, with a second agent, `other`, to route to; the installation
/// lives as long as the directory returned.
pub(crate) fn writer() -> anyhow::Result<(tempfile::TempDir, super::Writer)> {
    let directory = tempfile::tempdir()?;
    let mut config = installation(directory.path(), "discord-private", 46_950)?;
    let other = AgentConfig {
        id: "other".to_owned(),
        name: "Other agent".to_owned(),
        ..config.value.agents[0].clone()
    };
    config.value.agents.push(other);
    let discord = DiscordConfig::load(&config, "discord-private")?;
    Ok((
        directory,
        super::Writer::new(
            super::Settings::new(discord),
            std::sync::Arc::new(config),
            "discord-private",
        ),
    ))
}

#[test]
fn discord_feedback_defaults_leave_admission_unconfigured() -> anyhow::Result<()> {
    let config: DiscordConfig = toml::from_str(
        r#"
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

    let misspelt = refusal(toml::from_str::<ActivityMarkers>("suceeded = '✓'"));
    assert!(
        misspelt
            .as_deref()
            .is_some_and(|error| error.contains("unknown field `suceeded`")),
        "{misspelt:?}"
    );
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
    assert_eq!(
        refusal(validate_activity_marker(&"✓".repeat(22))).as_deref(),
        Some("activity marker must be at most 64 UTF-8 bytes")
    );
    Ok(())
}

#[test]
fn model_markers_validate_prefixes_and_inline_emotes() -> anyhow::Result<()> {
    const PREFIX_LENGTH: &str = "activity_markers.models prefixes must contain 1–63 UTF-8 bytes";
    let mut markers = ActivityMarkers::default();
    markers
        .models
        .insert("claude-".to_owned(), "<:claude:123>".to_owned());
    validate_activity_markers(&markers)?;
    markers
        .models
        .insert("gpt-".to_owned(), "<@123>".to_owned());
    assert_eq!(
        refusal(validate_activity_markers(&markers)).as_deref(),
        Some(r#"invalid activity_markers.models marker for "gpt-""#)
    );
    markers.models.remove("gpt-");
    markers.models.insert("x".repeat(63), "✓".to_owned());
    validate_activity_markers(&markers)?;
    markers.models.insert("x".repeat(64), "✓".to_owned());
    assert_eq!(
        refusal(validate_activity_markers(&markers)).as_deref(),
        Some(PREFIX_LENGTH)
    );
    markers.models.remove(&"x".repeat(64));
    markers.models.insert(String::new(), "✓".to_owned());
    assert_eq!(
        refusal(validate_activity_markers(&markers)).as_deref(),
        Some(PREFIX_LENGTH)
    );
    Ok(())
}

#[test]
fn activity_markers_reject_mentions_markdown_controls_and_malformed_emotes() {
    for (expected, markers) in [
        (
            "custom activity marker must use <:name:id> (static) or <a:name:id> (animated), with a 2–32 character ASCII emoji name and canonical snowflake ID",
            &[
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
                "[✓](https://example.test)",
            ][..],
        ),
        (
            "activity marker must be empty, Unicode emoji/symbols, or a custom Discord emote; whitespace, mentions, markdown, and control characters are not allowed",
            &[
                "@everyone",
                "@here",
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
                ">",
                "#",
                "-",
                "+",
                "\\",
                "word",
            ][..],
        ),
    ] {
        for marker in markers {
            assert_eq!(
                refusal(validate_activity_marker(marker)).as_deref(),
                Some(expected),
                "{marker:?}"
            );
        }
    }
}

#[test]
fn activity_marker_errors_identify_each_field_and_prevent_load_or_write() -> anyhow::Result<()> {
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
fn an_admission_change_preserves_tuned_feedback_and_rejects_unknown_providers() -> anyhow::Result<()>
{
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
    let admission = |routes: &str| -> anyhow::Result<super::FileChange> {
        Ok(super::FileChange::Admission {
            guild_id: "110".to_owned(),
            bot_user_id: "111".to_owned(),
            bot_token_file: None,
            allowed_user_ids: vec!["210".to_owned()],
            routes: super::Routes::Answered(vec![routes.parse()?]),
        })
    };
    admission("310:mention:thread:default")?.write(&config, "discord-private")?;
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
    for (provider, refused) in [
        (
            "test-runtime",
            "provider test-runtime does not have role interface",
        ),
        ("unregistered", "provider unregistered is not configured"),
    ] {
        let refusal = admission("310:mention:thread:default")?
            .write(&config, provider)
            .err()
            .context("wrote another provider's settings")?;
        assert!(format!("{refusal:#}").ends_with(refused), "{refusal:#}");
    }
    assert!(!config.provider_config_path("unregistered")?.exists());
    Ok(())
}

#[test]
fn a_custom_module_needs_its_path_and_starts_at_the_installation() -> anyhow::Result<()> {
    use crate::module::Source;

    let directory = tempfile::tempdir()?;
    let config = installation(directory.path(), "discord-private", 45_320)?;
    let mut discord = DiscordConfig::load(&config, "discord-private")?;
    assert_eq!(discord.module_source(&config.root), Source::Bundled);
    // A path alone, or a disabled module, keeps the bundled one.
    discord.module.path = Some("modules/custom.wasm".into());
    assert_eq!(discord.module_source(&config.root), Source::Bundled);
    discord.module.enabled = true;
    assert_eq!(
        discord.module_source(&config.root),
        Source::Custom {
            path: config.root.join("modules/custom.wasm")
        }
    );
    discord.module.path = Some("/elsewhere/custom.wasm".into());
    assert_eq!(
        discord.module_source(&config.root),
        Source::Custom {
            path: "/elsewhere/custom.wasm".into()
        }
    );
    discord.write(&config, "discord-private")?;
    assert_eq!(
        DiscordConfig::load(&config, "discord-private")?.module,
        discord.module
    );
    for path in [None, Some(std::path::PathBuf::new())] {
        discord.module.path = path;
        let refused = discord
            .write(&config, "discord-private")
            .err()
            .context("enabled a module without a path")?;
        assert!(refused.to_string().contains("module.path"), "{refused}");
    }
    let misspelt = refusal(toml::from_str::<super::ModuleSetting>("enable = true"));
    assert!(
        misspelt
            .as_deref()
            .is_some_and(|error| error.contains("unknown field `enable`")),
        "{misspelt:?}"
    );
    Ok(())
}

#[test]
fn the_bot_token_is_the_canonical_file_unless_the_settings_name_another() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let config = installation(directory.path(), "discord-private", 45_330)?;
    // Settings written before setup named a file, or for a token put
    // there by hand, read the canonical one, and say nothing of it.
    let mut discord = DiscordConfig::load(&config, "discord-private")?;
    assert_eq!(
        discord.bot_token_path(&config.root),
        config.root.join("credentials/discord-bot.token")
    );
    assert!(
        !fs::read_to_string(config.provider_config_path("discord-private")?)?
            .contains("bot_token_file")
    );
    discord.bot_token_file = Some("credentials/discord-bot.new.token".into());
    discord.write(&config, "discord-private")?;
    assert_eq!(
        DiscordConfig::load(&config, "discord-private")?.bot_token_path(&config.root),
        config.root.join("credentials/discord-bot.new.token")
    );
    discord.bot_token_file = Some(std::path::PathBuf::new());
    let refused = discord
        .write(&config, "discord-private")
        .err()
        .context("named no bot token file")?;
    assert!(refused.to_string().contains("bot_token_file"), "{refused}");
    Ok(())
}

#[test]
fn route_requires_all_explicit_fields() -> anyhow::Result<()> {
    let route = DiscordRoute::from_str("300:mention:thread:assistant")?;
    assert_eq!(route.channel_id, "300");
    assert_eq!(route.agent, "assistant");
    assert_eq!(route.activation, DiscordActivation::Mention);
    assert_eq!(route.conversation, DiscordConversation::Thread);
    let shape = "route must be CHANNEL_ID:mention|always:thread|channel:AGENT_ID";
    for (route, refused) in [
        ("300:mention:thread", shape),
        (
            "300:mention",
            "route conversation must be thread or channel",
        ),
        (
            "300:free:thread:assistant",
            "route activation must be mention or always",
        ),
        ("300:mention:thread:not/an/agent", shape),
        ("300:mention:thread:assistant:more", shape),
    ] {
        assert_eq!(
            refusal(DiscordRoute::from_str(route)).as_deref(),
            Some(refused),
            "{route}"
        );
    }
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
    let unknown = "Discord emoji must be Unicode emoji or canonical custom name:id";
    let custom = "custom Discord emoji must use canonical name:id syntax";
    for (emoji, refused) in [
        ("working", unknown),
        ("é", unknown),
        ("working:not-an-id", custom),
        ("x:123", custom),
    ] {
        assert_eq!(
            refusal(DiscordEmoji::parse(emoji)).as_deref(),
            Some(refused),
            "{emoji}"
        );
    }
    Ok(())
}

#[test]
fn turn_emoji_default_to_distinct_reactions_and_apply_live() -> anyhow::Result<()> {
    let serving: DiscordConfig = toml::from_str(
        r#"
guild_id = "100"
allowed_user_ids = ["200"]
"#,
    )?;
    assert_eq!(
        [
            &serving.steered_emoji,
            &serving.stopped_emoji,
            &serving.steer_emoji,
            &serving.interrupt_emoji,
            &serving.stop_emoji,
        ],
        ["↪️", "⏹️", "⏩", "⏭️", "🛑"]
    );
    let mut file = serving.clone();
    "🔀".clone_into(&mut file.steered_emoji);
    "✋".clone_into(&mut file.stopped_emoji);
    "▶️".clone_into(&mut file.steer_emoji);
    "🔁".clone_into(&mut file.interrupt_emoji);
    "🚫".clone_into(&mut file.stop_emoji);
    assert_eq!(
        Difference::between(&serving, &file),
        Difference {
            live: [
                "steered_emoji",
                "stopped_emoji",
                "steer_emoji",
                "interrupt_emoji",
                "stop_emoji"
            ]
            .map(str::to_owned)
            .to_vec(),
            restart: Vec::new(),
        }
    );
    Ok(())
}

#[test]
fn control_emoji_differ_from_each_other_and_from_the_feedback_emoji() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let config = installation(directory.path(), "discord", 47_100)?;
    let valid = DiscordConfig::load(&config, "discord")?;
    let clash = "steer_emoji, interrupt_emoji and stop_emoji must differ from each other and from the feedback emoji";
    let unknown = "Discord emoji must be Unicode emoji or canonical custom name:id";
    for (change, refused) in [
        (("steer", "⏹️"), clash),
        (("steer", "⏹"), clash),
        (("interrupt", "⏩"), clash),
        (("stop", "✅"), clash),
        (("stop", "👀"), clash),
        (("stop", "stop"), unknown),
        (("steered", "🛑"), clash),
    ] {
        let mut tuned = valid.clone();
        let (field, emoji) = change;
        let setting = match field {
            "steer" => &mut tuned.steer_emoji,
            "interrupt" => &mut tuned.interrupt_emoji,
            "stop" => &mut tuned.stop_emoji,
            _ => &mut tuned.steered_emoji,
        };
        emoji.clone_into(setting);
        assert_eq!(
            refusal(tuned.validate(&config, "discord")).as_deref(),
            Some(refused),
            "{field} {emoji}"
        );
    }
    Ok(())
}

fn settings_file(writer: &super::Writer) -> anyhow::Result<Vec<u8>> {
    Ok(fs::read(
        writer
            .installation()
            .provider_config_path("discord-private")?,
    )?)
}

#[tokio::test]
async fn the_writer_refuses_a_hand_edit_nobody_applied() -> anyhow::Result<()> {
    let (_installation, writer) = writer()?;
    let mut edited = DiscordConfig::read(writer.installation(), "discord-private")?;
    edited.reactions = !edited.reactions;
    edited.write(writer.installation(), "discord-private")?;
    let before = settings_file(&writer)?;
    let refused = writer
        .change(later(), |discord| {
            discord.typing = !discord.typing;
            Ok(())
        })
        .await
        .err()
        .context("wrote over a hand edit")?;
    let super::Refused::Unapplied(difference) = &refused else {
        anyhow::bail!("refused otherwise: {refused:?}");
    };
    assert_eq!(difference.live, ["reactions"]);
    let error = crate::InterfaceError::from(refused);
    assert_eq!(error.status, axum::http::StatusCode::PRECONDITION_FAILED);
    assert!(
        error.message.contains("`carillon discord settings apply`"),
        "{}",
        error.message
    );
    assert_eq!(settings_file(&writer)?, before);
    assert_ne!(writer.current().reactions, edited.reactions);
    Ok(())
}

#[tokio::test]
async fn a_stale_change_writes_nothing_and_is_a_conflict() -> anyhow::Result<()> {
    let (_installation, writer) = writer()?;
    let before = settings_file(&writer)?;
    let refused = writer
        .change(later(), |discord| {
            discord.typing = !discord.typing;
            Err::<(), _>(super::Refused::Stale)
        })
        .await
        .err()
        .context("a stale change applied")?;
    assert_eq!(
        crate::InterfaceError::from(refused).status,
        axum::http::StatusCode::CONFLICT
    );
    assert_eq!(settings_file(&writer)?, before);
    Ok(())
}

#[tokio::test]
async fn a_valid_change_is_written_privately_and_served() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let (_installation, writer) = writer()?;
    let written = writer
        .change(later(), |discord| {
            discord.routes[0].activation = DiscordActivation::Mention;
            Ok(discord.routes[0].clone())
        })
        .await
        .map_err(|refused| anyhow::anyhow!("{refused:?}"))?;
    assert_eq!(written.changed, ["routes"]);
    assert_eq!(written.value.activation, DiscordActivation::Mention);
    let file = DiscordConfig::load(writer.installation(), "discord-private")?;
    assert_eq!(file.routes, writer.current().routes);
    assert_eq!(file.routes[0].activation, DiscordActivation::Mention);
    let path = writer
        .installation()
        .provider_config_path("discord-private")?;
    assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);

    let before = settings_file(&writer)?;
    let refused = writer
        .change(later(), |discord| {
            discord.routes[0].agent = "nobody".to_owned();
            Ok(())
        })
        .await
        .err()
        .context("wrote a route to an unknown agent")?;
    assert!(matches!(refused, super::Refused::Invalid(_)), "{refused:?}");
    assert_eq!(settings_file(&writer)?, before);
    Ok(())
}

#[tokio::test]
async fn concurrent_changes_to_other_routes_both_survive() -> anyhow::Result<()> {
    let (_installation, writer) = writer()?;
    let first = writer.clone();
    let second = writer.clone();
    let (one, two) = tokio::join!(
        first.change(later(), |discord| {
            discord.routes[0].conversation = DiscordConversation::Thread;
            Ok(())
        }),
        second.change(later(), |discord| {
            discord.routes.push(
                "301:mention:thread:other"
                    .parse()
                    .map_err(|error: anyhow::Error| super::Refused::Invalid(error.to_string()))?,
            );
            Ok(())
        }),
    );
    one.map_err(|refused| anyhow::anyhow!("{refused:?}"))?;
    two.map_err(|refused| anyhow::anyhow!("{refused:?}"))?;
    let file = DiscordConfig::load(writer.installation(), "discord-private")?;
    assert_eq!(file.routes[0].conversation, DiscordConversation::Thread);
    assert_eq!(file.routes[1].channel_id, "301");
    assert_eq!(file.routes, writer.current().routes);
    Ok(())
}

#[tokio::test]
async fn a_change_and_its_undo_leave_the_owners_file_as_it_was() -> anyhow::Result<()> {
    let (_installation, writer) = writer()?;
    let path = writer
        .installation()
        .provider_config_path("discord-private")?;
    let owned = r#"# The team's Discord settings, kept by hand.
allowed_user_ids = ["200"] # who may talk to agents
guild_id = "100"
bot_user_id = "101"

# The support channel.
[[routes]]
channel_id = "300"
agent = "default"
activation = "always" # everyone, not only mentions
conversation = "channel"
"#;
    fs::write(&path, owned)?;
    let listens = |activation| {
        writer.change(later(), move |discord| {
            discord.routes[0].activation = activation;
            Ok(())
        })
    };
    listens(DiscordActivation::Mention)
        .await
        .map_err(|refused| anyhow::anyhow!("{refused:?}"))?;
    assert_eq!(
        fs::read_to_string(&path)?,
        owned.replace(r#"activation = "always""#, r#"activation = "mention""#)
    );
    assert_eq!(
        writer.current().routes[0].activation,
        DiscordActivation::Mention
    );
    listens(DiscordActivation::Always)
        .await
        .map_err(|refused| anyhow::anyhow!("{refused:?}"))?;
    assert_eq!(fs::read_to_string(&path)?, owned);
    Ok(())
}

#[test]
fn settings_written_over_the_owners_file_change_only_what_differs() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let config = installation(directory.path(), "discord", 47_200)?;
    let path = config.provider_config_path("discord")?;
    let owned = r#"guild_id = "100" # our server
bot_user_id = "101"
allowed_user_ids = ["200"]
typing = false # quiet, please

[[routes]] # support
channel_id = "300"
agent = "default"
activation = "always"
conversation = "channel"
"#;
    fs::write(&path, owned)?;
    let original = DiscordConfig::load(&config, "discord")?;
    let mut changed = original.clone();
    changed.routes.push("301:mention:thread:default".parse()?);
    changed.module.enabled = true;
    changed.module.path = Some("custom.wasm".into());
    changed.write(&config, "discord")?;
    let written = fs::read_to_string(&path)?;
    assert!(written.starts_with(owned), "{written}");
    let reread = DiscordConfig::load(&config, "discord")?;
    assert_eq!(reread.routes, changed.routes);
    assert_eq!(reread.module, changed.module);
    assert!(!reread.typing);
    original.write(&config, "discord")?;
    assert_eq!(fs::read_to_string(&path)?, owned);
    Ok(())
}

#[tokio::test]
async fn a_module_committed_in_place_but_not_synced_serves_its_file() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let (_installation, writer) = writer()?;
    let path = writer
        .installation()
        .provider_config_path("discord-private")?;
    let providers = path.parent().context("settings directory")?;
    // Writable but not readable: the new file is renamed into place, and
    // the directory cannot be opened to sync it.
    fs::set_permissions(providers, fs::Permissions::from_mode(0o300))?;
    let written = writer
        .declare(
            |discord| {
                discord.routes[0].activation = DiscordActivation::Mention;
                Ok(())
            },
            |_, commit| async move {
                commit.write().map_err(crate::module::ChangeError::Commit)?;
                let summary = crate::module::ModuleSummary {
                    source: crate::module::Source::Bundled,
                    commands: Vec::new(),
                    digest: String::new(),
                };
                Ok(crate::module::Change {
                    serving: summary.clone(),
                    previous: summary,
                    discord: crate::module::Registration::NotConnected,
                })
            },
        )
        .await;
    fs::set_permissions(providers, fs::Permissions::from_mode(0o700))?;
    let written = written.map_err(|refused| anyhow::anyhow!("{refused:?}"))?;
    assert!(written.unsynced.is_some());
    let file = DiscordConfig::load(writer.installation(), "discord-private")?;
    assert_eq!(file.routes[0].activation, DiscordActivation::Mention);
    assert_eq!(file.routes, writer.current().routes);
    // What serves is the file, so the next change builds on it.
    writer
        .change(later(), |discord| {
            discord.routes[0].activation = DiscordActivation::Always;
            Ok(())
        })
        .await
        .map_err(|refused| anyhow::anyhow!("{refused:?}"))?;
    Ok(())
}
mod owners_file;
