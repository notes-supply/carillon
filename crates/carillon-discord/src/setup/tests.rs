use std::{
    collections::VecDeque,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use anyhow::anyhow;
use axum::{
    Json, Router,
    extract::Path as Segments,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse as _, Response},
    routing::get,
};
use carillon_installation::{config::LoadedConfig, prompt::Questions};
use serde_json::{Value, json};

use crate::config::{DiscordActivation, DiscordConfig, DiscordConversation, DiscordRoute};

const TOKEN: &str = "good-token";

/// What the Discord stand-in shows: for its first `intent` reads, the
/// application lacks Message Content Intent; for its first `invited` reads,
/// the bot is in no server.
#[derive(Default)]
struct Discord {
    intent: AtomicUsize,
    invited: AtomicUsize,
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
/// servers `100` (Notes) and `101` (Other) whose members are `200` and `250`.
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
                        json!([guild("100", "Notes"), guild("101", "Other")])
                    } else {
                        json!([])
                    })
                    .into_response()
                }
            }),
        )
        .route("/api/v10/guilds/{guild}/channels", get(channels))
        .route("/api/v10/guilds/{guild}/members/{user}", get(member));
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

/// A server's channels: two text channels, out of order, and a voice one.
async fn channels(headers: HeaderMap) -> Response {
    if !authorized(&headers) {
        return refused();
    }
    let channel = |id: &str, kind: u8, name: &str, position: i32| json!({"id": id, "type": kind, "guild_id": "100", "name": name, "position": position});
    Json(json!([
        channel("301", 0, "agent", 2),
        channel("302", 2, "voice", 1),
        channel("300", 0, "general", 0),
    ]))
    .into_response()
}

/// A server member: `200` (the owner) and `250`, and nobody else.
async fn member(headers: HeaderMap, Segments((_, member)): Segments<(String, String)>) -> Response {
    if !authorized(&headers) {
        return refused();
    }
    let name = match member.as_str() {
        "200" => "owner",
        "250" => "friend",
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

fn bot_token(config: &LoadedConfig) -> std::path::PathBuf {
    config.root.join("credentials/discord-bot.token")
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
            "",        // the application's owner
            "999",     // not a member
            "x",       // not an ID
            "250 250", // friend, once
        ],
    )
    .await;
    result?;
    assert!(shown.contains("Discord refused this token."), "{shown}");
    assert!(shown.contains("Choose at least one channel."), "{shown}");
    assert!(shown.contains("999 is not a member of Other."), "{shown}");
    assert!(shown.contains("x is not a Discord user ID."), "{shown}");
    assert!(shown.contains("#general\n#agent\n"), "{shown}");
    assert!(
        !shown.contains(TOKEN) && !shown.contains("wrong-token"),
        "{shown}"
    );
    assert_eq!(
        std::fs::read_to_string(bot_token(&config))?,
        format!("{TOKEN}\n")
    );
    let settings = DiscordConfig::load(&config, "discord-private")?;
    assert_eq!(settings.guild_id, "101");
    assert_eq!(settings.bot_user_id, "201");
    assert_eq!(settings.allowed_user_ids, ["200", "250"]);
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
    });
    let address = discord(state).await?;
    // Every other answer is Enter: the first server, its first channel,
    // every message in its own thread, the application's owner and nobody else.
    let (result, shown) = run(&config, address, &[TOKEN, "", "", "", "", "", "", ""]).await;
    result?;
    assert!(shown.contains("Turn on Message Content Intent"), "{shown}");
    assert!(
        shown.contains(&format!(
            "https://discord.com/oauth2/authorize?client_id=500&scope=bot%20applications.commands&permissions={}",
            super::PERMISSIONS.bits()
        )),
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
    assert!(!bot_token(&config).exists());
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
    assert!(!bot_token(&config).exists());
    assert!(!config.provider_config_path("discord-private")?.exists());
    drop(held);
    // Once it ends, the same answers are written, and the lock is free again.
    let (result, _) = run(&config, address, &[TOKEN, "", "", "", "", ""]).await;
    result?;
    assert!(bot_token(&config).exists());
    assert!(
        carillon_installation::maintenance::MaintenanceLock::try_acquire(&config.root)?.is_some()
    );
    Ok(())
}
