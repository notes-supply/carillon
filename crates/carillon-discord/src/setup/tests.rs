use std::{
    collections::{HashMap, VecDeque},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use anyhow::{Context as _, anyhow};
use axum::{
    Json, Router,
    extract::{Path as Segments, Query},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse as _, Response},
    routing::get,
};
use carillon_installation::{
    config::{AgentConfig, LoadedConfig},
    prompt::Questions,
};
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
/// sees #agent, or with `admin` only the administrator role `401`. It is in
/// `servers` more servers after its usual three, listed a page at a time;
/// `pages` records each listing's `after` and `limit`. With `team`, a team
/// owns the application: two accepted members shown as Sam (`200`, `210`),
/// Kim (`230`), and Pat (`220`), who is only invited.
#[derive(Default)]
struct Discord {
    intent: AtomicUsize,
    invited: AtomicUsize,
    unroled: AtomicUsize,
    admin: bool,
    team: bool,
    servers: u64,
    pages: Mutex<Vec<(Option<String>, Option<String>)>>,
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
/// are `200`, `250` and another bot, `260`.
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
                    let member = |id: &str, name: &str, membership: u8| {
                        let mut person = user(id, &format!("{name}-{id}"), false);
                        person["global_name"] = json!(name);
                        json!({"membership_state": membership, "permissions": ["*"],
                               "team_id": "600", "user": person})
                    };
                    let team = if state.team {
                        json!({"icon": null, "id": "600", "name": "Team", "owner_user_id": "200",
                               "members": [member("200", "Sam", 2), member("210", "Sam", 2),
                                           member("230", "Kim", 2), member("220", "Pat", 1)]})
                    } else {
                        Value::Null
                    };
                    Json(json!({
                        "id": "500", "name": "Carillon", "icon": null, "description": "",
                        "bot_public": false, "bot_require_code_grant": false,
                        "verify_key": "key", "flags": flags,
                        "owner": user("200", "owner", false), "team": team,
                    }))
                    .into_response()
                }
            }),
        )
        .route(
            "/api/v10/users/@me/guilds",
            get(
                move |headers: HeaderMap, Query(query): Query<HashMap<String, String>>| {
                    let state = guilds.clone();
                    async move {
                        if !authorized(&headers) {
                            return refused();
                        }
                        let (after, limit) = (query.get("after"), query.get("limit"));
                        if let Ok(mut pages) = state.pages.lock() {
                            pages.push((after.cloned(), limit.cloned()));
                        }
                        if !shown(&state.invited) {
                            return Json(json!([])).into_response();
                        }
                        let mut all = vec![
                            (100, "Notes".to_owned()),
                            (101, "Other".to_owned()),
                            (102, "Notes".to_owned()),
                        ];
                        all.extend((0..state.servers).map(|n| (1000 + n, format!("Server {n}"))));
                        let after: u64 = after.map_or(Ok(0), |after| after.parse()).unwrap_or(0);
                        let limit: usize =
                            limit.map_or(Ok(200), |limit| limit.parse()).unwrap_or(200);
                        let page: Vec<Value> = all
                            .into_iter()
                            .filter(|(id, _)| *id > after)
                            .take(limit)
                            .map(|(id, name)| {
                                json!({"id": id.to_string(), "name": name, "icon": null,
                                       "owner": false, "permissions": "0", "features": []})
                            })
                            .collect();
                        Json(page).into_response()
                    }
                },
            ),
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

/// A change someone else makes while the questions are open.
type Meanwhile = Box<dyn FnOnce() -> anyhow::Result<()>>;

/// Answers given in order, as a person would give them; when none is left,
/// the question goes unanswered and setup ends. `meanwhile` is done once,
/// when the question it names is asked.
struct Scripted {
    answers: VecDeque<&'static str>,
    shown: String,
    meanwhile: Option<(&'static str, Meanwhile)>,
}

impl Scripted {
    fn new(answers: &[&'static str]) -> Self {
        Self {
            answers: answers.iter().copied().collect(),
            shown: String::new(),
            meanwhile: None,
        }
    }

    fn next(&mut self, question: &str) -> anyhow::Result<&'static str> {
        if self
            .meanwhile
            .as_ref()
            .is_some_and(|(at, _)| question.starts_with(at))
            && let Some((_, change)) = self.meanwhile.take()
        {
            change()?;
        }
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
    let (name, bot) = match (guild.as_str(), member.as_str()) {
        ("100", "200") => ("owner", false),
        ("100", "210") => ("Sam-210", false),
        (_, "250") => ("friend", false),
        (_, "260") => ("other-bot", true),
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"code": 10007, "message": "Unknown Member"})),
            )
                .into_response();
        }
    };
    Json(json!({
        "user": user(&member, name, bot), "nick": null, "roles": [],
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

/// An installation without Discord settings whose registry has a second
/// agent, plain T3 Code, beside the test agent.
fn with_two_agents(root: &Path) -> anyhow::Result<LoadedConfig> {
    let config = fresh(root)?;
    let mut registry = config.value.clone();
    registry.agents.push(AgentConfig {
        id: "code".to_owned(),
        name: "T3 Code · Notes".to_owned(),
        runtime: "test-runtime".to_owned(),
        workspace: config.root.join("workspace"),
        resources: None,
    });
    std::fs::write(&config.path, toml::to_string(&registry)?)?;
    config.reload()
}

/// An installation set up by hand: settings naming no token file, and in the
/// canonical one a bot token Discord no longer accepts.
fn by_hand(root: &Path) -> anyhow::Result<LoadedConfig> {
    let config = crate::config::tests::installation(root, "discord-private", 46_600)?;
    std::fs::write(config.root.join(BOT_TOKEN_FILE), "old-token\n")?;
    Ok(config)
}

/// Setting an installation [`by_hand`] up again with `TOKEN`: the refused
/// saved token, which is not offered for keeping, then every default.
const REPLACE: &[&str] = &["", TOKEN, "", "", "", "", ""];

/// Runs setup with `answers`, returning what it showed.
async fn run(
    config: &LoadedConfig,
    address: std::net::SocketAddr,
    answers: &[&'static str],
) -> (anyhow::Result<()>, String) {
    run_with(Scripted::new(answers), config, address).await
}

/// Runs setup asking `questions`, returning what it showed.
async fn run_with(
    mut questions: Scripted,
    config: &LoadedConfig,
    address: std::net::SocketAddr,
) -> (anyhow::Result<()>, String) {
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

/// The settings file and every bot token file, by name, as they are.
fn saved_files(
    config: &LoadedConfig,
) -> anyhow::Result<std::collections::BTreeMap<String, Vec<u8>>> {
    let mut files = std::collections::BTreeMap::new();
    files.insert(
        "settings".to_owned(),
        std::fs::read(config.provider_config_path("discord-private")?)?,
    );
    for name in bot_tokens(config)? {
        let bytes = std::fs::read(config.root.join("credentials").join(&name))?;
        files.insert(name, bytes);
    }
    Ok(files)
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
        agent: "default".to_owned(),
        activation: DiscordActivation::Mention,
        conversation: DiscordConversation::Thread,
    };
    assert_eq!(settings.routes, [route("300"), route("301")]);
    assert!(
        !std::fs::read_to_string(config.provider_config_path("discord-private")?)?.contains(TOKEN)
    );

    // Saved settings whose token Discord still accepts are kept when asked.
    let before = std::fs::read(config.provider_config_path("discord-private")?)?;
    let (result, _) = run(&config, address, &[""]).await;
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
            agent: "default".to_owned(),
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

mod routing;

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
    // file, are refused before any token is written.
    let directory = tempfile::tempdir()?;
    let config = by_hand(directory.path())?;
    let file = config.provider_config_path("discord-private")?;
    fs::write(
        &file,
        fs::read_to_string(&file)?
            .replace("processing_emoji = \"👀\"", "processing_emoji = \"eyes\""),
    )?;
    let previous = fs::read(&file)?;
    let (result, _) = run(&config, address, REPLACE).await;
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

#[tokio::test]
async fn every_server_is_offered_page_after_page() -> anyhow::Result<()> {
    let pages = |state: &Discord| {
        state
            .pages
            .lock()
            .map(|pages| pages.clone())
            .map_err(|_| anyhow!("the recorded pages are poisoned"))
    };
    let page = |after: Option<&str>| (after.map(str::to_owned), Some("200".to_owned()));

    // The last of 253 servers, on the second page, with someone in it.
    let directory = tempfile::tempdir()?;
    let config = fresh(directory.path())?;
    let state = Arc::new(Discord {
        servers: 250,
        ..Discord::default()
    });
    let address = discord(state.clone()).await?;
    let (result, shown) = run(&config, address, &[TOKEN, "253", "", "", "none", "250"]).await;
    result?;
    assert!(shown.contains("Notes (102)\nServer 0\n"), "{shown}");
    assert!(shown.contains("Server 249\n"), "{shown}");
    assert_eq!(
        DiscordConfig::load(&config, "discord-private")?.guild_id,
        "1249"
    );
    // The second page starts after the 200th server's ID.
    assert_eq!(pages(&state)?, [page(None), page(Some("1196"))]);

    // Servers that fit one page take one request.
    let directory = tempfile::tempdir()?;
    let config = fresh(directory.path())?;
    let state = Arc::new(Discord::default());
    let address = discord(state.clone()).await?;
    let (result, _) = run(&config, address, &[TOKEN, "", "", "", "", ""]).await;
    result?;
    assert_eq!(pages(&state)?, [page(None)]);
    Ok(())
}

#[tokio::test]
async fn bot_accounts_are_not_admitted_and_people_are_asked_for_again() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let config = fresh(directory.path())?;
    let address = discord(Arc::new(Discord::default())).await?;
    let (result, shown) = run(
        &config,
        address,
        &[
            TOKEN, "", "", "", //
            "none", "201", // this bot,
            "none", "250 260", // a person and another bot,
            "none", "250", // then the person alone
        ],
    )
    .await;
    result?;
    assert!(
        shown.contains("carillon-bot (201) is a bot; name the people the agent answers."),
        "{shown}"
    );
    assert!(
        shown.contains("other-bot (260) is a bot; name the people the agent answers."),
        "{shown}"
    );
    assert_eq!(
        DiscordConfig::load(&config, "discord-private")?.allowed_user_ids,
        ["250"]
    );
    Ok(())
}

#[tokio::test]
async fn owners_sharing_a_name_are_told_apart_and_the_one_chosen_is_admitted() -> anyhow::Result<()>
{
    let directory = tempfile::tempdir()?;
    let config = fresh(directory.path())?;
    let address = discord(Arc::new(Discord {
        team: true,
        ..Discord::default()
    }))
    .await?;
    // Only the second Sam, and nobody else.
    let (result, shown) = run(&config, address, &[TOKEN, "", "", "", "2", ""]).await;
    result?;
    assert!(
        shown.contains(
            "Sam (200) (owns the bot's application)\nSam (210) (owns the bot's application)\n\
             Kim (owns the bot's application)\n"
        ),
        "{shown}"
    );
    assert!(!shown.contains("Pat"), "{shown}");
    assert_eq!(
        DiscordConfig::load(&config, "discord-private")?.allowed_user_ids,
        ["210"]
    );
    Ok(())
}

#[tokio::test]
async fn saved_settings_are_offered_for_keeping_only_while_their_bot_signs_in() -> anyhow::Result<()>
{
    let address = discord(Arc::new(Discord::default())).await?;
    // Settings for bot `bot`, saved with `token` in the canonical file.
    let installation = |root: &Path, bot: &str, token: &str| -> anyhow::Result<LoadedConfig> {
        let config = crate::config::tests::installation(root, "discord-private", 46_600)?;
        let mut settings = DiscordConfig::read(&config, "discord-private")?;
        bot.clone_into(&mut settings.bot_user_id);
        settings.write(&config, "discord-private")?;
        std::fs::write(config.root.join(BOT_TOKEN_FILE), format!("{token}\n"))?;
        Ok(config)
    };
    let keep = "Keep the saved Discord bot and settings?";

    // The token signs in as the saved bot: kept as it is.
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
        assert!(result.is_err());
        assert!(shown.contains(said), "{shown}");
        assert!(!shown.contains(keep), "{shown}");
        assert!(
            shown.contains("Bot token (Enter keeps the saved one):"),
            "{shown}"
        );
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

#[tokio::test]
async fn publication_follows_the_registry_as_it_is_when_the_questions_end() -> anyhow::Result<()> {
    use carillon_installation::config::Config;

    let address = discord(Arc::new(Discord::default())).await?;
    // Setting an installation [`by_hand`] up again while `carillon.toml` is
    // changed, as another operator would, before the last question.
    let edited = |change: fn(&mut Config)| async move {
        let directory = tempfile::tempdir()?;
        let config = by_hand(directory.path())?;
        let before = saved_files(&config)?;
        let path = config.path.clone();
        let mut questions = Scripted::new(REPLACE);
        questions.meanwhile = Some((
            "Anyone else",
            Box::new(move || {
                let mut registry = LoadedConfig::load(&path)?.value;
                change(&mut registry);
                std::fs::write(&path, toml::to_string(&registry)?)?;
                Ok(())
            }),
        ));
        let (result, _) = run_with(questions, &config, address).await;
        anyhow::Ok((directory, config, before, result))
    };

    // An interface disabled or removed, or the agent renamed, meanwhile is
    // refused, with nothing written: the routes name the agent as asked.
    let disable: fn(&mut Config) = |registry| {
        for provider in &mut registry.providers {
            if provider.id == "discord-private" {
                provider.enabled = false;
            }
        }
    };
    let remove: fn(&mut Config) = |registry| {
        registry
            .providers
            .retain(|provider| provider.id != "discord-private");
    };
    let rename: fn(&mut Config) = |registry| {
        "other".clone_into(&mut registry.agents[0].id);
    };
    for (change, reason) in [
        (disable, "discord-private is disabled in carillon.toml"),
        (remove, "discord-private"),
        (rename, "route 300 names an unknown agent"),
    ] {
        let (_directory, config, before, result) = edited(change).await?;
        let refusal = format!("{:#}", result.err().context("setup ignored the registry")?);
        assert!(
            refusal
                .starts_with("the Discord settings and the bot token they name were not changed: ")
                && refusal.contains(reason),
            "{refusal}"
        );
        assert_eq!(saved_files(&config)?, before);
    }
    Ok(())
}
