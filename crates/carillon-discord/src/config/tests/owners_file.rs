use std::fs;

use carillon_installation::config::LoadedConfig;

use super::{installation, later, writer};
use crate::config::{DiscordActivation, DiscordConfig, DiscordRoute, FileChange, Routes};

const ROUTED: &str = r#"guild_id = "100"
bot_user_id = "101"
allowed_user_ids = ["200"]

[[routes]]
channel_id = "300"
agent = "default"
activation = "always"
conversation = "channel"
"#;

fn change_and_undo(
    owned: &str,
    change: impl FnOnce(&mut DiscordConfig),
) -> anyhow::Result<(String, String)> {
    let directory = tempfile::tempdir()?;
    let config = installation(directory.path(), "discord", 47_500)?;
    let path = config.provider_config_path("discord")?;
    fs::write(&path, owned)?;
    let original = DiscordConfig::load(&config, "discord")?;
    let mut changed = original.clone();
    change(&mut changed);
    changed.write(&config, "discord")?;
    let written = fs::read_to_string(&path)?;
    original.write(&config, "discord")?;
    Ok((written, fs::read_to_string(&path)?))
}

fn module(config: &LoadedConfig, enabled: bool, path: Option<&str>) -> anyhow::Result<()> {
    FileChange::Module {
        enabled,
        path: path.map(Into::into),
    }
    .write(config, "discord")
}

fn answer(users: &[&str], routes: Vec<DiscordRoute>) -> FileChange {
    FileChange::Admission {
        guild_id: "100".to_owned(),
        bot_user_id: "101".to_owned(),
        bot_token_file: None,
        allowed_user_ids: users.iter().map(|&user| user.to_owned()).collect(),
        routes: Routes::Answered(routes),
    }
}

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

#[test]
fn a_comment_above_enabled_stays_as_the_module_is_enabled_and_disabled() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let config = installation(directory.path(), "discord", 47_550)?;
    let path = config.provider_config_path("discord")?;
    let owned = format!(
        "{ROUTED}\n[module]\n# Off: the bundled commands serve.\nenabled = false\npath = 'custom.wasm'\n"
    );
    fs::write(&path, &owned)?;
    module(&config, true, None)?;
    assert_eq!(
        fs::read_to_string(&path)?,
        owned.replace("enabled = false", "enabled = true")
    );
    module(&config, false, None)?;
    assert_eq!(fs::read_to_string(&path)?, owned);

    let enabled = owned.replace("enabled = false", "enabled = true");
    fs::write(&path, &enabled)?;
    module(&config, false, None)?;
    assert_eq!(fs::read_to_string(&path)?, owned);
    Ok(())
}

#[test]
fn a_dotted_module_key_keeps_its_comment_through_a_change_and_its_undo() -> anyhow::Result<()> {
    let owned = ROUTED.replace(
        "\n[[routes]]",
        "# Keep the bundled commands by default.\nmodule.enabled = false\n\n[[routes]]",
    );
    let enable = |settings: &mut DiscordConfig| {
        settings.module.enabled = true;
        settings.module.path = Some("custom.wasm".into());
    };
    let (_, undone) = change_and_undo(&owned, enable)?;
    assert_eq!(undone, owned);
    let uncommented = owned.replace("# Keep the bundled commands by default.\n", "");
    let (_, undone) = change_and_undo(&uncommented, enable)?;
    assert_eq!(undone, ROUTED);
    Ok(())
}

#[test]
fn a_module_disabled_and_enabled_again_keeps_its_table_as_it_was() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let config = installation(directory.path(), "discord", 47_600)?;
    let path = config.provider_config_path("discord")?;
    fs::write(&path, ROUTED)?;
    module(&config, true, Some("/opt/custom.wasm"))?;
    let enabled = fs::read_to_string(&path)?;
    assert_eq!(
        enabled,
        format!("{ROUTED}\n[module]\nenabled = true\npath = \"/opt/custom.wasm\"\n")
    );
    module(&config, false, None)?;
    assert_eq!(
        fs::read_to_string(&path)?,
        enabled.replace("enabled = true", "enabled = false")
    );
    module(&config, true, None)?;
    assert_eq!(fs::read_to_string(&path)?, enabled);
    Ok(())
}

#[test]
fn dropping_a_user_keeps_the_line_and_comment_of_a_user_it_shared_with() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let config = installation(directory.path(), "discord", 47_650)?;
    let path = config.provider_config_path("discord")?;
    let owned = ROUTED.replace(
        r#"["200"]"#,
        "[\n    \"200\", \"201\", # the two of us\n    \"250\", # a friend\n]",
    );
    fs::write(&path, &owned)?;
    let routes = DiscordConfig::load(&config, "discord")?.routes;
    answer(&["200", "250"], routes.clone()).write(&config, "discord")?;
    assert_eq!(
        fs::read_to_string(&path)?,
        owned.replace(r#""200", "201","#, r#""200","#)
    );
    fs::write(&path, &owned)?;
    answer(&["201", "250"], routes).write(&config, "discord")?;
    assert_eq!(
        fs::read_to_string(&path)?,
        owned.replace(r#""200", "201","#, r#""201","#)
    );
    Ok(())
}

#[test]
fn dropping_a_route_keeps_the_section_comment_above_it() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let config = installation(directory.path(), "discord", 47_700)?;
    let path = config.provider_config_path("discord")?;
    let support = "# Support: everyone may talk here.\n[[routes]]\nchannel_id = \"300\"\nagent = \"default\"\nactivation = \"always\"\nconversation = \"channel\"\n\n";
    let private = "# Private: mentions only.\n[[routes]]\nchannel_id = \"301\"\nagent = \"default\"\nactivation = \"mention\"\nconversation = \"thread\"\n";
    let section = "guild_id = \"100\"\nbot_user_id = \"101\"\nallowed_user_ids = [\"200\"]\n\n# ---- Channels the agent answers in ----\n\n";
    let owned = format!("{section}{support}{private}");
    fs::write(&path, &owned)?;
    let routes = DiscordConfig::load(&config, "discord")?.routes;
    answer(&["200"], routes[1..].to_vec()).write(&config, "discord")?;
    assert_eq!(fs::read_to_string(&path)?, format!("{section}{private}"));
    answer(&["200"], vec!["302:mention:thread:default".parse()?]).write(&config, "discord")?;
    assert_eq!(
        fs::read_to_string(&path)?,
        format!(
            "{section}[[routes]]\nchannel_id = \"302\"\nagent = \"default\"\nactivation = \"mention\"\nconversation = \"thread\"\n"
        )
    );
    Ok(())
}

#[test]
fn setup_answers_that_only_reorder_a_repeated_user_leave_the_file() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let config = installation(directory.path(), "discord", 47_750)?;
    let path = config.provider_config_path("discord")?;
    let owned = ROUTED.replace(
        r#"["200"]"#,
        "[\n    \"200\", # primary\n    \"201\", # colleague\n    \"200\", # also primary\n]",
    );
    fs::write(&path, &owned)?;
    let routes = DiscordConfig::load(&config, "discord")?.routes;
    answer(&["201", "200", "200"], routes).write(&config, "discord")?;
    assert_eq!(fs::read_to_string(&path)?, owned);
    Ok(())
}

#[test]
fn a_file_without_a_final_newline_keeps_it_missing() -> anyhow::Result<()> {
    let owned = ROUTED.trim_end();
    let (changed, undone) = change_and_undo(owned, |settings| {
        settings.routes[0].activation = DiscordActivation::Mention;
    })?;
    assert_eq!(changed, owned.replace("\"always\"", "\"mention\""));
    assert_eq!(undone, owned);
    Ok(())
}

#[test]
fn dropping_an_inline_route_keeps_the_section_comment_above_it() -> anyhow::Result<()> {
    let owned = r#"guild_id = "100"
bot_user_id = "101"
allowed_user_ids = ["200"]
routes = [
    # ---- Channels the agent answers in ----

    # Support: everyone may talk here.
    { channel_id = "300", agent = "default", activation = "always", conversation = "channel" },
    { channel_id = "301", agent = "default", activation = "mention", conversation = "thread" },
]
"#;
    let (changed, _) = change_and_undo(owned, |settings| {
        settings.routes.remove(0);
    })?;
    assert_eq!(
        changed,
        owned.replace(
            "    # Support: everyone may talk here.\n    { channel_id = \"300\", agent = \"default\", activation = \"always\", conversation = \"channel\" },\n",
            ""
        )
    );
    Ok(())
}

#[test]
fn a_changed_string_comes_back_in_the_form_operations_lists() -> anyhow::Result<()> {
    for (owned, intermediate, undone) in [
        (r#""""always""""#, "mention", r#""always""#),
        ("'''always'''", "mention", "'always'"),
        (r#""always""#, "mention", r#""always""#),
        ("\"\"\"al\\\n  ways\"\"\"", "mention", r#""always""#),
        (
            "'''\nfirst\nsecond\n'''",
            "other",
            "\"\"\"\nfirst\nsecond\n\"\"\"",
        ),
        ("'always'", "owner's", r#""always""#),
        (r#""a\"b""#, "other", r#"'a"b'"#),
        (r#""tab\tbed""#, "other", r#""tab\tbed""#),
    ] {
        let file = format!("{ROUTED}\n[module]\nenabled = false\npath = {owned} # mine\n");
        let (_, after_undo) = change_and_undo(&file, |settings| {
            settings.module.path = Some(intermediate.into());
        })?;
        assert_eq!(
            after_undo,
            file.replace(&format!("= {owned} #"), &format!("= {undone} #")),
            "{owned}"
        );
    }
    Ok(())
}

#[test]
fn removed_users_and_routes_come_back_as_operations_lists() -> anyhow::Result<()> {
    let owned = r#"guild_id = "100"
bot_user_id = "101"
allowed_user_ids = [
    "200", "201", # the two of us
    "202", # a friend
]

[[routes]]
channel_id = "300"
agent = "default"
activation = "always"
conversation = "channel"

# ---- Private ----

[[routes]]
channel_id = "301"
agent = "default"
activation = "mention"
conversation = "thread"
"#;
    let (_, undone) = change_and_undo(owned, |settings| {
        settings.allowed_user_ids.remove(1);
        settings.routes.pop();
    })?;
    assert_eq!(
        undone,
        owned
            .replace(
                "\"200\", \"201\", # the two of us\n",
                "\"200\", # the two of us\n    \"201\",\n"
            )
            .replace("\n# ---- Private ----\n", "")
    );
    let duplicates = ROUTED.replace(
        r#"["200"]"#,
        "[\n    \"200\", # me\n    \"200\", # me again\n]",
    );
    let (changed, _) = change_and_undo(&duplicates, |settings| {
        settings.allowed_user_ids.pop();
    })?;
    assert_eq!(changed, ROUTED);
    Ok(())
}
