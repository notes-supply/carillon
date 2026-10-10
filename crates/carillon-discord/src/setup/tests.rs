use std::{
    collections::VecDeque,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use anyhow::{Context as _, anyhow};
use axum::{
    Json, Router,
    extract::Path as Segments,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse as _, Response},
    routing::get,
};
use carillon_installation::{config::LoadedConfig, prompt::Questions};
use serde_json::{Value, json};
use twilight_model::guild::Permissions;

use crate::config::{
    BOT_TOKEN_FILE, Difference, DiscordActivation, DiscordConfig, DiscordConversation, DiscordRoute,
};

const TOKEN: &str = "good-token";

/// What the Discord stand-in shows: for its first `intent` reads, the
/// application lacks Message Content Intent; for its first `invited` reads,
/// the bot is in no server; for its first `unroled` reads of the bot as a
/// member, it has no role. Its role is then `400`, which may use threads and
/// sees #agent, or with `admin` only the administrator role `401`.
#[derive(Default)]
struct Discord {
    intent: AtomicUsize,
    invited: AtomicUsize,
    unroled: AtomicUsize,
    admin: bool,
}

/// Counts a read of what `pending` holds back, saying whether it is shown.
fn shown(pending: &AtomicUsize) -> bool {
    pending
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
            left.checked_sub(1)
        })
        .is_err()
}

fn user(id: &str, name: &str, bot: bool) -> Value {
    json!({"id": id, "username": name, "discriminator": "0", "avatar": null, "bot": bot})
}

fn bits(permissions: Permissions) -> String {
    permissions.bits().to_string()
}

fn authorized(headers: &HeaderMap) -> bool {
    headers
        .get("authorization")
        .is_some_and(|value| value == &format!("Bot {TOKEN}"))
}

fn refused() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"code": 0, "message": "401: Unauthorized"})),
    )
        .into_response()
}

/// A Discord API answering the bot token `TOKEN` only, for a bot `201` in
/// servers `100` (Notes), `101` (Other) and `102` (Notes again) whose members
/// are `200` and `250`.
async fn discord(state: Arc<Discord>) -> anyhow::Result<std::net::SocketAddr> {
    let application = state.clone();
    let guilds = state.clone();
    let router = Router::new()
        .route(
            "/api/v10/users/@me",
            get(move |headers: HeaderMap| async move {
                if !authorized(&headers) {
                    return refused();
                }
                let mut bot = user("201", "carillon-bot", true);
                bot["mfa_enabled"] = json!(false);
                Json(bot).into_response()
            }),
        )
        .route(
            "/api/v10/applications/@me",
            get(move |headers: HeaderMap| {
                let state = application.clone();
                async move {
                    if !authorized(&headers) {
                        return refused();
                    }
                    let flags = if shown(&state.intent) { 1 << 19 } else { 0 };
                    Json(json!({
                        "id": "500", "name": "Carillon", "icon": null, "description": "",
                        "bot_public": false, "bot_require_code_grant": false,
                        "verify_key": "key", "flags": flags,
                        "owner": user("200", "owner", false), "team": null,
                    }))
                    .into_response()
                }
            }),
        )
        .route(
            "/api/v10/users/@me/guilds",
            get(move |headers: HeaderMap| {
                let state = guilds.clone();
                async move {
                    if !authorized(&headers) {
                        return refused();
                    }
                    let guild = |id: &str, name: &str| {
                        json!({"id": id, "name": name, "icon": null, "owner": false,
                               "permissions": "0", "features": []})
                    };
                    Json(if shown(&state.invited) {
                        json!([
                            guild("100", "Notes"),
                            guild("101", "Other"),
                            guild("102", "Notes")
                        ])
                    } else {
                        json!([])
                    })
                    .into_response()
                }
            }),
        )
        .route("/api/v10/guilds/{guild}/channels", get(channels))
        .route("/api/v10/guilds/{guild}/roles", get(roles))
        .route(
            "/api/v10/guilds/{guild}/members/{user}",
            get(move |headers: HeaderMap, segments| {
                let state = state.clone();
                async move { member(&state, &headers, segments) }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    tokio::spawn(async move { axum::serve(listener, router).await });
    Ok(address)
}

/// Answers given in order, as a person would give them; when none is left,
/// the question goes unanswered and setup ends.
struct Scripted {
    answers: VecDeque<&'static str>,
    shown: String,
}

impl Scripted {
    fn new(answers: &[&'static str]) -> Self {
        Self {
            answers: answers.iter().copied().collect(),
            shown: String::new(),
        }
    }

    fn next(&mut self, question: &str) -> anyhow::Result<&'static str> {
        self.shown.push_str(question);
        self.shown.push('\n');
        self.answers
            .pop_front()
            .ok_or_else(|| anyhow!("no answer to {question}"))
    }
}

/// 1-based numbers in `answer`, as indexes.
fn indexes(answer: &str) -> anyhow::Result<Vec<usize>> {
    answer
        .split_whitespace()
        .map(|number| Ok(number.parse::<usize>()? - 1))
        .collect()
}

impl Questions for Scripted {
    fn say(&mut self, text: &str) -> anyhow::Result<()> {
        self.shown.push_str(text);
        self.shown.push('\n');
        Ok(())
    }

    fn answer(&mut self, question: &str, default: Option<&str>) -> anyhow::Result<String> {
        let answer = self.next(question)?;
        Ok(if answer.is_empty() {
            default.unwrap_or_default().to_owned()
        } else {
            answer.to_owned()
        })
    }

    fn secret(&mut self, question: &str) -> anyhow::Result<String> {
        Ok(self.next(question)?.to_owned())
    }

    fn confirm(&mut self, question: &str, default: bool) -> anyhow::Result<bool> {
        Ok(match self.next(question)? {
            "" => default,
            answer => answer == "y",
        })
    }

    fn choose(
        &mut self,
        question: &str,
        options: &[String],
        default: usize,
    ) -> anyhow::Result<usize> {
        self.shown.push_str(&options.join("\n"));
        self.shown.push('\n');
        match self.next(question)? {
            "" => Ok(default),
            answer => Ok(indexes(answer)?[0]),
        }
    }

    fn choose_many(
        &mut self,
        question: &str,
        options: &[String],
        chosen: &[usize],
    ) -> anyhow::Result<Vec<usize>> {
        self.shown.push_str(&options.join("\n"));
        self.shown.push('\n');
        match self.next(question)? {
            "" => Ok(chosen.to_vec()),
            "none" => Ok(Vec::new()),
            answer => indexes(answer),
        }
    }
}

/// A server's channels, out of order: #general, where nobody may embed
/// links or react; a voice channel; #agent, which only role `400` sees; and
/// another #general.
async fn channels(headers: HeaderMap, Segments(guild): Segments<String>) -> Response {
    if !authorized(&headers) {
        return refused();
    }
    let channel = |id: &str, kind: u8, name: &str, position: i32, overwrites: Value| {
        json!({"id": id, "type": kind, "guild_id": guild, "name": name, "position": position,
               "permission_overwrites": overwrites})
    };
    let overwrite = |id: &str, allow: Permissions, deny: Permissions| json!({"id": id, "type": 0, "allow": bits(allow), "deny": bits(deny)});
    let none = Permissions::empty();
    Json(json!([
        channel(
            "301",
            0,
            "agent",
            2,
            json!([
                overwrite(&guild, none, Permissions::VIEW_CHANNEL),
                overwrite("400", Permissions::VIEW_CHANNEL, none),
            ])
        ),
        channel("302", 2, "voice", 1, json!([])),
        channel(
            "300",
            0,
            "general",
            0,
            json!([overwrite(
                &guild,
                none,
                Permissions::EMBED_LINKS | Permissions::ADD_REACTIONS
            )])
        ),
        channel("303", 0, "general", 3, json!([])),
    ]))
    .into_response()
}

/// A server's roles: everyone may read, answer, react and show cards and
/// files; `400` may also start and answer in threads; `401` administers.
async fn roles(headers: HeaderMap, Segments(guild): Segments<String>) -> Response {
    if !authorized(&headers) {
        return refused();
    }
    let role = |id: &str, name: &str, permissions: Permissions| {
        json!({"id": id, "name": name, "permissions": bits(permissions), "position": 0,
               "color": 0, "colors": {"primary_color": 0}, "hoist": false,
               "managed": false, "mentionable": false, "flags": 0})
    };
    Json(json!([
        role(
            &guild,
            "@everyone",
            Permissions::VIEW_CHANNEL
                | Permissions::SEND_MESSAGES
                | Permissions::READ_MESSAGE_HISTORY
                | Permissions::ADD_REACTIONS
                | Permissions::EMBED_LINKS
                | Permissions::ATTACH_FILES
        ),
        role(
            "400",
            "Carillon",
            Permissions::CREATE_PUBLIC_THREADS | Permissions::SEND_MESSAGES_IN_THREADS
        ),
        role("401", "Admin", Permissions::ADMINISTRATOR),
    ]))
    .into_response()
}

/// A server member: the bot, with its roles; `250` in any server; and `200`
/// (the application's owner) only in Notes `100`.
fn member(
    state: &Discord,
    headers: &HeaderMap,
    Segments((guild, member)): Segments<(String, String)>,
) -> Response {
    if !authorized(headers) {
        return refused();
    }
    if member == "201" {
        let roles = match (shown(&state.unroled), state.admin) {
            (false, _) => json!([]),
            (true, false) => json!(["400"]),
            (true, true) => json!(["401"]),
        };
        return Json(json!({
            "user": user("201", "carillon-bot", true), "nick": null, "roles": roles,
            "joined_at": "2026-01-01T00:00:00.000000+00:00",
            "deaf": false, "mute": false, "flags": 0,
        }))
        .into_response();
    }
    let name = match (guild.as_str(), member.as_str()) {
        ("100", "200") => "owner",
        (_, "250") => "friend",
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"code": 10007, "message": "Unknown Member"})),
            )
                .into_response();
        }
    };
    Json(json!({
        "user": user(&member, name, false), "nick": null, "roles": [],
        "joined_at": "2026-01-01T00:00:00.000000+00:00",
        "deaf": false, "mute": false, "flags": 0,
    }))
    .into_response()
}

/// An installation without Discord settings or a bot token yet.
fn fresh(root: &Path) -> anyhow::Result<LoadedConfig> {
    let config = crate::config::tests::installation(root, "discord-private", 46_600)?;
    std::fs::remove_file(config.provider_config_path("discord-private")?)?;
    Ok(config)
}

/// An installation set up by hand: settings naming no token file, and in the
/// canonical one a bot token Discord no longer accepts.
fn by_hand(root: &Path) -> anyhow::Result<LoadedConfig> {
    let config = crate::config::tests::installation(root, "discord-private", 46_600)?;
    std::fs::write(config.root.join(BOT_TOKEN_FILE), "old-token\n")?;
    Ok(config)
}

/// Setting an installation [`by_hand`] up again with `TOKEN`: declining to
/// keep it, the refused saved token, then every default.
const REPLACE: &[&str] = &["n", "", TOKEN, "", "", "", "", ""];

/// Runs setup with `answers`, returning what it showed.
async fn run(
    config: &LoadedConfig,
    address: std::net::SocketAddr,
    answers: &[&'static str],
) -> (anyhow::Result<()>, String) {
    let mut questions = Scripted::new(answers);
    let result = super::setup(&mut questions, config, "discord-private", |token| {
        twilight_http::Client::builder()
            .token(token)
            .proxy(address.to_string(), true)
            .ratelimiter(None)
            .build()
    })
    .await;
    (result, questions.shown)
}

/// The bot token files in the installation, by name.
fn bot_tokens(config: &LoadedConfig) -> anyhow::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(config.root.join("credentials"))? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if name.starts_with("discord-bot") {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

/// The bot token in the file the saved settings name.
fn named_token(config: &LoadedConfig) -> anyhow::Result<String> {
    let settings = DiscordConfig::load(config, "discord-private")?;
    Ok(std::fs::read_to_string(
        settings.bot_token_path(&config.root),
    )?)
}

#[tokio::test]
async fn refused_tokens_and_unknown_people_are_asked_again_and_only_named_people_are_admitted()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let config = fresh(directory.path())?;
    let address = discord(Arc::new(Discord::default())).await?;
    let (result, shown) = run(
        &config,
        address,
        &[
            "wrong-token", // refused
            TOKEN,
            "2",       // Other
            "none",    // no channel: asked again
            "1 2",     // #general and #agent, listed by position
            "3",       // only when mentioned, each in its own thread
            "",        // the application's owner, who is not in Other,
            "",        //   and nobody else
            "",        // the owner still,
            "x",       //   and not an ID
            "none",    // the owner corrected away,
            "999 250", //   and someone not in Other
            "none",    //
            "250 250", //   friend, once
        ],
    )
    .await;
    result?;
    assert!(shown.contains("Discord refused this token."), "{shown}");
    assert!(shown.contains("Choose at least one channel."), "{shown}");
    assert!(
        shown.contains("owner (200) is not a member of Other."),
        "{shown}"
    );
    assert!(shown.contains("x is not a Discord user ID."), "{shown}");
    assert!(shown.contains("999 is not a member of Other."), "{shown}");
    assert!(
        shown.contains("#general (300)\n#agent\n#general (303)\n"),
        "{shown}"
    );
    assert!(
        !shown.contains(TOKEN) && !shown.contains("wrong-token"),
        "{shown}"
    );
    assert_eq!(named_token(&config)?, format!("{TOKEN}\n"));
    let settings = DiscordConfig::load(&config, "discord-private")?;
    assert_eq!(settings.guild_id, "101");
    assert_eq!(settings.bot_user_id, "201");
    assert_eq!(settings.allowed_user_ids, ["250"]);
    let route = |channel: &str| DiscordRoute {
        channel_id: channel.to_owned(),
        activation: DiscordActivation::Mention,
        conversation: DiscordConversation::Thread,
    };
    assert_eq!(settings.routes, [route("300"), route("301")]);
    assert!(
        !std::fs::read_to_string(config.provider_config_path("discord-private")?)?.contains(TOKEN)
    );

    // Saved settings are kept when asked, and nothing is asked of Discord.
    let before = std::fs::read(config.provider_config_path("discord-private")?)?;
    let (result, _) = run(&config, "127.0.0.1:1".parse()?, &[""]).await;
    result?;
    assert_eq!(
        std::fs::read(config.provider_config_path("discord-private")?)?,
        before
    );
    Ok(())
}

#[tokio::test]
async fn setup_waits_for_message_content_and_an_invitation() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let config = fresh(directory.path())?;
    let state = Arc::new(Discord {
        intent: AtomicUsize::new(1),
        invited: AtomicUsize::new(1),
        ..Discord::default()
    });
    let address = discord(state).await?;
    // Every other answer is Enter: the first server, its first channel,
    // every message in its own thread, the application's owner and nobody else.
    let (result, shown) = run(&config, address, &[TOKEN, "", "", "", "", "", "", ""]).await;
    result?;
    assert!(shown.contains("Turn on Message Content Intent"), "{shown}");
    assert!(
        shown.contains(
            "https://discord.com/oauth2/authorize?client_id=500&scope=bot%20applications.commands&permissions=309237763136"
        ),
        "{shown}"
    );
    let settings = DiscordConfig::load(&config, "discord-private")?;
    assert_eq!(settings.guild_id, "100");
    assert_eq!(settings.allowed_user_ids, ["200"]);
    assert_eq!(
        settings.routes,
        [DiscordRoute {
            channel_id: "300".to_owned(),
            activation: DiscordActivation::Always,
            conversation: DiscordConversation::Thread,
        }]
    );
    Ok(())
}

#[tokio::test]
async fn setup_that_ends_unanswered_writes_nothing() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let config = fresh(directory.path())?;
    let address = discord(Arc::new(Discord::default())).await?;
    let (result, _) = run(&config, address, &[TOKEN, "1"]).await;
    assert!(result.is_err());
    assert!(bot_tokens(&config)?.is_empty());
    assert!(!config.provider_config_path("discord-private")?.exists());
    Ok(())
}

#[tokio::test]
async fn setup_during_other_maintenance_changes_neither_token_nor_settings() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let config = fresh(directory.path())?;
    let address = discord(Arc::new(Discord::default())).await?;
    let held = carillon_installation::maintenance::MaintenanceLock::acquire(&config.root)?;
    let (result, _) = run(&config, address, &[TOKEN, "", "", "", "", ""]).await;
    let refusal = result
        .err()
        .map(|error| format!("{error:#}"))
        .unwrap_or_default();
    assert!(
        refusal.contains("were not changed") && refusal.contains("another Carillon maintenance"),
        "{refusal}"
    );
    assert!(bot_tokens(&config)?.is_empty());
    assert!(!config.provider_config_path("discord-private")?.exists());
    drop(held);
    // Once it ends, the same answers are written, and the lock is free again.
    let (result, _) = run(&config, address, &[TOKEN, "", "", "", "", ""]).await;
    result?;
    assert_eq!(named_token(&config)?, format!("{TOKEN}\n"));
    assert!(
        carillon_installation::maintenance::MaintenanceLock::try_acquire(&config.root)?.is_some()
    );
    Ok(())
}

#[tokio::test]
async fn servers_and_channels_of_the_same_name_are_told_apart_by_id() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let config = fresh(directory.path())?;
    let address = discord(Arc::new(Discord::default())).await?;
    // The second Notes, its second #general, every message in its own
    // thread, and the application's owner, who is not in that server, then
    // someone who is.
    let (result, shown) = run(
        &config,
        address,
        &[TOKEN, "3", "3", "", "", "", "none", "250"],
    )
    .await;
    result?;
    assert!(
        shown.contains("owner (200) is not a member of Notes (102)."),
        "{shown}"
    );
    assert!(
        shown.contains("Notes (100)\nOther\nNotes (102)\n"),
        "{shown}"
    );
    assert!(
        shown.contains("#general (300)\n#agent\n#general (303)\n"),
        "{shown}"
    );
    let settings = DiscordConfig::load(&config, "discord-private")?;
    assert_eq!(settings.guild_id, "102");
    assert_eq!(settings.routes[0].channel_id, "303");
    Ok(())
}

#[tokio::test]
async fn a_route_is_saved_only_once_the_bot_may_answer_there_as_its_mode_needs()
-> anyhow::Result<()> {
    let without_role = || Discord {
        unroled: AtomicUsize::new(usize::MAX),
        ..Discord::default()
    };

    // Without its role, the bot cannot see #agent; answering there ends
    // setup unanswered, with nothing written.
    let directory = tempfile::tempdir()?;
    let config = fresh(directory.path())?;
    let address = discord(Arc::new(without_role())).await?;
    let (result, shown) = run(&config, address, &[TOKEN, "1", "2", "2"]).await;
    assert!(result.is_err());
    assert!(
        shown.contains(
            "The bot cannot answer in #agent: allow it View Channel in the channel's settings."
        ),
        "{shown}"
    );
    assert!(bot_tokens(&config)?.is_empty());
    assert!(!config.provider_config_path("discord-private")?.exists());

    // #general needs no role for the channel as one conversation, and lacking
    // cards and reactions there is only said.
    let directory = tempfile::tempdir()?;
    let config = fresh(directory.path())?;
    let address = discord(Arc::new(without_role())).await?;
    let (result, shown) = run(&config, address, &[TOKEN, "1", "1", "2", "", ""]).await;
    result?;
    assert!(!shown.contains("cannot answer"), "{shown}");
    assert!(
        shown.contains(
            "In #general (300) the bot lacks Embed Links, Add Reactions, so the agent answers \
             there without cards or reactions."
        ),
        "{shown}"
    );
    assert_eq!(
        DiscordConfig::load(&config, "discord-private")?.routes[0].channel_id,
        "300"
    );

    // A thread per conversation needs the role in both; once it is granted,
    // the next check passes and both are saved.
    let directory = tempfile::tempdir()?;
    let config = fresh(directory.path())?;
    let address = discord(Arc::new(Discord {
        unroled: AtomicUsize::new(2),
        ..Discord::default()
    }))
    .await?;
    let (result, shown) = run(&config, address, &[TOKEN, "1", "1 2", "1", "", "", "", ""]).await;
    result?;
    assert!(
        shown.contains(
            "The bot cannot answer in #general (300): allow it Create Public Threads, Send \
             Messages in Threads in the channel's settings.\nThe bot cannot answer in #agent: \
             allow it View Channel, Create Public Threads, Send Messages in Threads in the \
             channel's settings.\nPress Enter to check again."
        ),
        "{shown}"
    );
    assert_eq!(shown.matches("Press Enter to check again.").count(), 2);
    let routes = DiscordConfig::load(&config, "discord-private")?.routes;
    assert_eq!(
        routes
            .iter()
            .map(|route| route.channel_id.as_str())
            .collect::<Vec<_>>(),
        ["300", "301"]
    );

    // An administrator role passes every overwrite.
    let directory = tempfile::tempdir()?;
    let config = fresh(directory.path())?;
    let address = discord(Arc::new(Discord {
        admin: true,
        ..Discord::default()
    }))
    .await?;
    let (result, shown) = run(&config, address, &[TOKEN, "1", "1 2", "1", "", ""]).await;
    result?;
    assert!(
        !shown.contains("lacks") && !shown.contains("cannot answer"),
        "{shown}"
    );
    assert_eq!(
        DiscordConfig::load(&config, "discord-private")?
            .routes
            .len(),
        2
    );
    Ok(())
}

#[tokio::test]
async fn a_new_bot_token_gets_a_file_of_its_own_and_the_same_one_keeps_its_file()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let config = by_hand(directory.path())?;
    let address = discord(Arc::new(Discord::default())).await?;
    let before = DiscordConfig::load(&config, "discord-private")?;
    let (result, _) = run(&config, address, REPLACE).await;
    result?;
    // The settings name the new token's file; the one put there by hand stays.
    let after = DiscordConfig::load(&config, "discord-private")?;
    assert_ne!(after.bot_token_file, before.bot_token_file);
    assert_eq!(named_token(&config)?, format!("{TOKEN}\n"));
    assert_eq!(
        std::fs::read_to_string(config.root.join(BOT_TOKEN_FILE))?,
        "old-token\n"
    );
    // A running provider takes it only at a restart.
    assert!(
        Difference::between(&before, &after)
            .restart
            .contains(&"bot_token_file".to_owned())
    );
    // The same token again, kept or typed, keeps its file, so the settings
    // apply live.
    for token in ["", TOKEN] {
        let (result, _) = run(&config, address, &["n", token, "", "", "", "", ""]).await;
        result?;
        let again = DiscordConfig::load(&config, "discord-private")?;
        assert_eq!(again.bot_token_file, after.bot_token_file);
        assert!(Difference::between(&after, &again).restart.is_empty());
    }
    assert_eq!(bot_tokens(&config)?.len(), 2);
    Ok(())
}

#[tokio::test]
async fn setup_of_a_disabled_provider_is_refused_before_any_question() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let mut config = by_hand(directory.path())?;
    for provider in &mut config.value.providers {
        if provider.id == "discord-private" {
            provider.enabled = false;
        }
    }
    let (result, shown) = run(&config, "127.0.0.1:1".parse()?, &[]).await;
    let refusal = result
        .err()
        .map(|error| format!("{error:#}"))
        .unwrap_or_default();
    assert_eq!(
        refusal,
        "discord-private is disabled in carillon.toml; enable it with `carillon provider \
         discord-private enable`, then run setup again"
    );
    assert!(shown.is_empty(), "{shown}");
    Ok(())
}

#[tokio::test]
async fn a_failed_setup_leaves_settings_that_name_a_token_written_before_them() -> anyhow::Result<()>
{
    use std::{fs, os::unix::fs::PermissionsExt as _};

    let address = discord(Arc::new(Discord::default())).await?;
    let mode = |path: &Path, mode: u32| fs::set_permissions(path, fs::Permissions::from_mode(mode));

    // Settings that would not be valid, here a tuned emoji kept from the
    // file, are refused before any token is written. Without valid settings
    // to keep, nothing asks to keep them.
    let directory = tempfile::tempdir()?;
    let config = by_hand(directory.path())?;
    let file = config.provider_config_path("discord-private")?;
    fs::write(
        &file,
        fs::read_to_string(&file)?
            .replace("processing_emoji = \"👀\"", "processing_emoji = \"eyes\""),
    )?;
    let previous = fs::read(&file)?;
    let (result, _) = run(&config, address, &REPLACE[1..]).await;
    let refusal = format!(
        "{:#}",
        result.err().context("invalid settings were written")?
    );
    assert!(
        refusal.contains("were not changed") && refusal.contains("emoji"),
        "{refusal}"
    );
    assert_eq!(fs::read(&file)?, previous);
    assert_eq!(bot_tokens(&config)?, ["discord-bot.token"]);

    // A token file that cannot be written leaves the settings as they were.
    let directory = tempfile::tempdir()?;
    let config = by_hand(directory.path())?;
    let file = config.provider_config_path("discord-private")?;
    let previous = fs::read(&file)?;
    let credentials = config.root.join("credentials");
    mode(&credentials, 0o500)?;
    let (result, _) = run(&config, address, REPLACE).await;
    mode(&credentials, 0o700)?;
    let refusal = format!(
        "{:#}",
        result.err().context("an unwritable token was accepted")?
    );
    assert!(refusal.contains("were not changed"), "{refusal}");
    assert_eq!(fs::read(&file)?, previous);
    assert_eq!(bot_tokens(&config)?, ["discord-bot.token"]);

    // Settings that cannot replace the file leave it naming the old token;
    // the new token's file is kept, named by nothing.
    let directory = tempfile::tempdir()?;
    let config = by_hand(directory.path())?;
    let file = config.provider_config_path("discord-private")?;
    let previous = fs::read(&file)?;
    let providers = file.parent().context("settings directory")?.to_owned();
    mode(&providers, 0o500)?;
    let (result, _) = run(&config, address, REPLACE).await;
    mode(&providers, 0o700)?;
    let refusal = format!(
        "{:#}",
        result.err().context("unwritable settings were accepted")?
    );
    assert!(refusal.contains("not changed"), "{refusal}");
    assert_eq!(fs::read(&file)?, previous);
    assert_eq!(named_token(&config)?, "old-token\n");
    assert_eq!(bot_tokens(&config)?.len(), 2);

    // Settings that replace the file but whose directory cannot be synced
    // are in place, naming the new token's file, which was synced first; a
    // crash could bring back only the previous settings, naming the old
    // token's file, which nothing changed.
    let directory = tempfile::tempdir()?;
    let config = by_hand(directory.path())?;
    let file = config.provider_config_path("discord-private")?;
    let providers = file.parent().context("settings directory")?.to_owned();
    mode(&providers, 0o300)?;
    let (result, _) = run(&config, address, REPLACE).await;
    mode(&providers, 0o700)?;
    let refusal = format!(
        "{:#}",
        result.err().context("an unsynced change was confirmed")?
    );
    assert!(
        refusal.contains("changed, but not confirmed durable"),
        "{refusal}"
    );
    assert_eq!(named_token(&config)?, format!("{TOKEN}\n"));
    assert_eq!(
        fs::read_to_string(config.root.join(BOT_TOKEN_FILE))?,
        "old-token\n"
    );
    Ok(())
}
