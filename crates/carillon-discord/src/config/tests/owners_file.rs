use std::fs;

use super::{installation, later, writer};
use crate::config::{DiscordActivation, DiscordConfig, DiscordRoute, FileChange, Routes};

async fn listens_and_back(owned: &str) -> anyhow::Result<(String, String)> {
    let (_installation, writer) = writer()?;
    let path = writer
        .installation()
        .provider_config_path("discord-private")?;
    fs::write(&path, owned)?;
    let mut files = Vec::new();
    for activation in [DiscordActivation::Mention, DiscordActivation::Always] {
        writer
            .change(later(), move |discord| {
                discord.routes[0].activation = activation;
                Ok(())
            })
            .await
            .map_err(|refused| anyhow::anyhow!("{refused:?}"))?;
        files.push(fs::read_to_string(&path)?);
    }
    let undone = files.pop().unwrap_or_default();
    let changed = files.pop().unwrap_or_default();
    Ok((changed, undone))
}

#[tokio::test]
async fn a_changed_string_keeps_its_quotes_and_comment() -> anyhow::Result<()> {
    let owned = r#"guild_id = "100"
bot_user_id = "101"
allowed_user_ids = ["200"]

[[routes]]
channel_id = '300'
agent = 'default'
activation = 'always' # everyone
conversation = 'channel'
"#;
    let (changed, undone) = listens_and_back(owned).await?;
    assert_eq!(
        changed,
        owned.replace("'always' # everyone", "'mention' # everyone")
    );
    assert_eq!(undone, owned);
    Ok(())
}

#[tokio::test]
async fn inline_routes_keep_their_comments_through_a_change_and_its_undo() -> anyhow::Result<()> {
    let owned = r#"guild_id = "100"
bot_user_id = "101"
allowed_user_ids = ["200"]
routes = [
    # The support channel.
    { channel_id = "300", agent = "default", activation = "always", conversation = "channel" },
] # Every admitted channel.
"#;
    let (changed, undone) = listens_and_back(owned).await?;
    assert_eq!(
        changed,
        owned.replace(r#"activation = "always""#, r#"activation = "mention""#)
    );
    assert_eq!(undone, owned);
    Ok(())
}

#[tokio::test]
async fn a_crlf_file_keeps_its_line_endings_through_a_change_and_its_undo() -> anyhow::Result<()> {
    let (_installation, writer) = writer()?;
    let path = writer
        .installation()
        .provider_config_path("discord-private")?;
    let owned = "guild_id = \"100\"\r\nbot_user_id = \"101\"\r\nallowed_user_ids = [\"200\"]\r\n\r\n[[routes]]\r\nchannel_id = \"300\"\r\nagent = \"default\"\r\nactivation = \"always\"\r\nconversation = \"channel\"\r\n";
    fs::write(&path, owned)?;
    let routed: DiscordRoute = "301:mention:thread:other".parse()?;
    writer
        .change(later(), |discord| {
            discord.routes[0].activation = DiscordActivation::Mention;
            discord.routes.push(routed);
            Ok(())
        })
        .await
        .map_err(|refused| anyhow::anyhow!("{refused:?}"))?;
    let changed = fs::read_to_string(&path)?;
    assert!(changed.contains("channel_id = \"301\"\r\n"), "{changed:?}");
    assert_eq!(
        changed.matches('\n').count(),
        changed.matches("\r\n").count(),
        "{changed:?}"
    );
    writer
        .change(later(), |discord| {
            discord.routes[0].activation = DiscordActivation::Always;
            discord.routes.truncate(1);
            Ok(())
        })
        .await
        .map_err(|refused| anyhow::anyhow!("{refused:?}"))?;
    assert_eq!(fs::read_to_string(&path)?, owned);
    Ok(())
}

#[test]
fn a_module_table_that_states_its_defaults_is_edited_in_place() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let config = installation(directory.path(), "discord", 47_300)?;
    let path = config.provider_config_path("discord")?;
    let owned = r#"guild_id = "100"
bot_user_id = "101"
allowed_user_ids = ["200"]
[[routes]]
channel_id = "300"
agent = "default"
activation = "always"
conversation = "channel"

[module] # Module selection, kept by the owner.
enabled = false # Use the bundled commands.
"#;
    fs::write(&path, owned)?;
    let original = DiscordConfig::load(&config, "discord")?;
    let mut changed = original.clone();
    changed.module.enabled = true;
    changed.module.path = Some("custom.wasm".into());
    changed.write(&config, "discord")?;
    let written = fs::read_to_string(&path)?;
    assert!(
        written.ends_with(
            "[module] # Module selection, kept by the owner.\nenabled = true # Use the bundled commands.\npath = \"custom.wasm\"\n"
        ),
        "{written}"
    );
    original.write(&config, "discord")?;
    assert_eq!(fs::read_to_string(&path)?, owned);
    Ok(())
}

#[test]
fn allowed_users_keep_their_comments_as_users_come_and_go() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let config = installation(directory.path(), "discord", 47_250)?;
    let path = config.provider_config_path("discord")?;
    let owned = r#"guild_id = "100"
bot_user_id = "101"
allowed_user_ids = [
    "200", # me
    "201", # my friend
]

[[routes]]
channel_id = "300"
agent = "default"
activation = "always"
conversation = "channel"
"#;
    fs::write(&path, owned)?;
    let original = DiscordConfig::load(&config, "discord")?;
    let mut changed = original.clone();
    changed.allowed_user_ids.push("202".to_owned());
    changed.write(&config, "discord")?;
    assert_eq!(
        fs::read_to_string(&path)?,
        owned.replace(
            "\"201\", # my friend\n",
            "\"201\", # my friend\n    \"202\",\n"
        )
    );
    original.write(&config, "discord")?;
    assert_eq!(fs::read_to_string(&path)?, owned);
    let mut changed = original;
    changed.allowed_user_ids.remove(0);
    changed.write(&config, "discord")?;
    assert_eq!(
        fs::read_to_string(&path)?,
        owned.replace("    \"200\", # me\n", "")
    );
    Ok(())
}

#[test]
fn setup_answers_keep_the_files_order_and_each_routes_comment() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let config = installation(directory.path(), "discord", 47_350)?;
    let path = config.provider_config_path("discord")?;
    let owned = r#"guild_id = "100"
bot_user_id = "101"
allowed_user_ids = ["200", "201"] # me and my friend

# Support: everyone may talk here.
[[routes]]
channel_id = "300"
agent = "default"
activation = "always"
conversation = "channel"

# Private: mentions only.
[[routes]]
channel_id = "301"
agent = "default"
activation = "mention"
conversation = "thread"
"#;
    fs::write(&path, owned)?;
    let answer = |routes: Vec<DiscordRoute>| FileChange::Admission {
        guild_id: "100".to_owned(),
        bot_user_id: "101".to_owned(),
        bot_token_file: None,
        allowed_user_ids: vec!["201".to_owned(), "200".to_owned()],
        routes: Routes::Answered(routes),
    };
    let mut routes = DiscordConfig::load(&config, "discord")?.routes;
    routes.reverse();
    answer(routes.clone()).write(&config, "discord")?;
    assert_eq!(fs::read_to_string(&path)?, owned);

    routes[0] = "302:mention:thread:default".parse()?;
    answer(routes).write(&config, "discord")?;
    let (support, _) = owned
        .split_once("\n# Private")
        .ok_or_else(|| anyhow::anyhow!("no private route"))?;
    assert_eq!(
        fs::read_to_string(&path)?,
        format!(
            "{support}\n[[routes]]\nchannel_id = \"302\"\nagent = \"default\"\nactivation = \"mention\"\nconversation = \"thread\"\n"
        )
    );
    Ok(())
}
