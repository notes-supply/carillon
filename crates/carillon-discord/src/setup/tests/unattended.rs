use super::*;

fn route(text: &str) -> anyhow::Result<DiscordRoute> {
    text.parse()
}

async fn unattended(config: &LoadedConfig, users: &[&str], routes: &[&str]) -> anyhow::Result<()> {
    super::super::unattended(
        config,
        "discord-private",
        "100".to_owned(),
        "201".to_owned(),
        users.iter().map(|user| (*user).to_owned()).collect(),
        routes
            .iter()
            .map(|text| route(text))
            .collect::<anyhow::Result<_>>()?,
    )
    .await
}

#[tokio::test]
async fn unattended_setup_writes_the_first_routes_and_repeats_them_unchanged() -> anyhow::Result<()>
{
    let directory = tempfile::tempdir()?;
    let config = fresh(directory.path())?;
    unattended(&config, &["200"], &["300:always:thread:default"]).await?;
    let written = DiscordConfig::load(&config, "discord-private")?;
    assert_eq!(written.guild_id, "100");
    assert_eq!(written.bot_user_id, "201");
    assert_eq!(written.routes, [route("300:always:thread:default")?]);

    // A repeated install names the same routes: they stay, and the rest of
    // the admission follows the command line.
    unattended(&config, &["200", "250"], &["300:always:thread:default"]).await?;
    let repeated = DiscordConfig::load(&config, "discord-private")?;
    assert_eq!(repeated.allowed_user_ids, ["200", "250"]);
    assert_eq!(repeated.routes, written.routes);
    Ok(())
}

#[tokio::test]
async fn unattended_setup_refuses_other_routes_once_channels_are_routed() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let config = fresh(directory.path())?;
    unattended(&config, &["200"], &["300:always:thread:default"]).await?;
    let path = config.provider_config_path("discord-private")?;
    let before = std::fs::read(&path)?;
    for routes in [
        &["300:mention:thread:default"][..],
        &["300:always:thread:default", "301:always:thread:default"],
    ] {
        let refusal = unattended(&config, &["200", "250"], routes)
            .await
            .err()
            .with_context(|| format!("accepted {routes:?} over existing routes"))?;
        assert!(
            format!("{refusal:#}").contains("`carillon settings <channel>"),
            "{refusal:#}"
        );
        assert_eq!(std::fs::read(&path)?, before);
    }
    Ok(())
}
