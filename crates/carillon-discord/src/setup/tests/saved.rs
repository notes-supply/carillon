//! Setup over saved settings: whether they can be kept, and the
//! direct-message route they may lack.

use super::*;
use crate::config::DirectMessages;

/// Saved settings from before direct messages, of an installation whose
/// registry has Carillon's assistant.
fn saved_without_direct_messages(root: &Path, token: &str) -> anyhow::Result<LoadedConfig> {
    let config = crate::config::tests::installation(root, "discord-private", 46_600)?;
    let mut registry = config.value.clone();
    registry.agents.push(AgentConfig {
        id: carillon_installation::config::ASSISTANT.to_owned(),
        name: "Assistant".to_owned(),
        runtime: "test-runtime".to_owned(),
        workspace: config.root.join("workspace"),
        resources: None,
    });
    std::fs::write(&config.path, toml::to_string(&registry)?)?;
    let config = config.reload()?;
    let mut settings = DiscordConfig::read(&config, "discord-private")?;
    "201".clone_into(&mut settings.bot_user_id);
    settings.write(&config, "discord-private")?;
    std::fs::write(config.root.join(BOT_TOKEN_FILE), format!("{token}\n"))?;
    Ok(config)
}

fn saved_direct_messages(config: &LoadedConfig) -> anyhow::Result<Option<DirectMessages>> {
    Ok(DiscordConfig::load(config, "discord-private")?.direct_messages)
}

fn assistant() -> DirectMessages {
    DirectMessages {
        agent: "assistant".to_owned(),
    }
}

#[tokio::test]
async fn keeping_saved_settings_names_the_assistant_for_direct_messages() -> anyhow::Result<()> {
    let address = discord(Arc::new(Discord::default())).await?;
    let directory = tempfile::tempdir()?;
    let config = saved_without_direct_messages(directory.path(), TOKEN)?;
    let (result, shown) = run(&config, address, &[""]).await;
    result?;
    assert_eq!(saved_direct_messages(&config)?, Some(assistant()));
    assert_eq!(
        shown.trim_end().lines().last(),
        Some("Direct messages from allowlisted people go to Assistant.")
    );

    let before = saved_files(&config)?;
    let (result, shown) = run(&config, address, &[""]).await;
    result?;
    assert_eq!(saved_files(&config)?, before, "kept again, nothing changes");
    assert!(!shown.contains("Direct messages"), "{shown}");
    Ok(())
}

#[tokio::test]
async fn setup_says_who_answers_direct_messages() -> anyhow::Result<()> {
    let address = discord(Arc::new(Discord::default())).await?;
    let directory = tempfile::tempdir()?;
    let config = saved_without_direct_messages(directory.path(), "old-token")?;
    let (result, shown) = run(
        &config,
        address,
        &["", TOKEN, "", "", "", "", "", "", "", ""],
    )
    .await;
    result?;
    assert_eq!(saved_direct_messages(&config)?, Some(assistant()));
    let closing = shown.trim_end().lines().last().unwrap_or_default();
    assert!(
        closing.ends_with(" Direct messages from them go to Assistant."),
        "{closing}"
    );
    Ok(())
}

#[tokio::test]
async fn saved_settings_are_offered_for_keeping_only_while_their_bot_signs_in() -> anyhow::Result<()>
{
    let address = discord(Arc::new(Discord::default())).await?;
    let installation = |root: &Path, bot: &str, token: &str| -> anyhow::Result<LoadedConfig> {
        let config = crate::config::tests::installation(root, "discord-private", 46_600)?;
        let mut settings = DiscordConfig::read(&config, "discord-private")?;
        bot.clone_into(&mut settings.bot_user_id);
        settings.write(&config, "discord-private")?;
        std::fs::write(config.root.join(BOT_TOKEN_FILE), format!("{token}\n"))?;
        Ok(config)
    };
    let keep = "Keep the saved Discord bot and settings?";

    let directory = tempfile::tempdir()?;
    let config = installation(directory.path(), "201", TOKEN)?;
    let before = saved_files(&config)?;
    let (result, shown) = run(&config, address, &[""]).await;
    result?;
    assert!(shown.contains(keep), "{shown}");
    assert_eq!(saved_files(&config)?, before);

    // Revoked, or another bot's: said, not offered, and a token asked for;
    // nothing is written while the question goes unanswered.
    for (bot, token, said) in [
        (
            "201",
            "old-token",
            "Discord refuses the saved bot token, so the saved settings cannot be kept.",
        ),
        (
            "101",
            TOKEN,
            "The saved bot token signs in as carillon-bot (201), not the bot the saved settings \
             name (101), so they cannot be kept.",
        ),
    ] {
        let directory = tempfile::tempdir()?;
        let config = installation(directory.path(), bot, token)?;
        let before = saved_files(&config)?;
        let (result, shown) = run(&config, address, &[]).await;
        assert_eq!(
            result.map_err(|error| error.to_string()),
            Err("no answer to Bot token (Enter keeps the saved one):".to_owned())
        );
        assert!(shown.contains(said), "{shown}");
        assert!(!shown.contains(keep), "{shown}");
        assert_eq!(saved_files(&config)?, before);
    }

    // Discord not answering says so, asks nothing and changes nothing.
    let directory = tempfile::tempdir()?;
    let config = installation(directory.path(), "201", TOKEN)?;
    let before = saved_files(&config)?;
    let (result, shown) = run(&config, "127.0.0.1:1".parse()?, &[]).await;
    let failure = result
        .err()
        .map(|error| format!("{error:#}"))
        .unwrap_or_default();
    assert!(
        failure.starts_with(
            "Discord did not say whether the saved bot token still works; nothing was changed: "
        ),
        "{failure}"
    );
    assert!(shown.is_empty(), "{shown}");
    assert_eq!(saved_files(&config)?, before);
    Ok(())
}
