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
        &["310:mention:thread:default".parse()?],
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
    for (provider, refused) in [
        (
            "test-runtime",
            "provider test-runtime does not have role interface",
        ),
        ("unregistered", "provider unregistered is not configured"),
    ] {
        assert_eq!(
            refusal(super::configure(
                &config,
                provider,
                "110",
                "111",
                &["210".to_owned()],
                &["310:mention:thread:default".parse()?],
            ))
            .as_deref(),
            Some(refused)
        );
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
