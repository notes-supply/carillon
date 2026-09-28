//! The monitor against a local substitute serving both core's activity list
//! and Discord's API through a real Twilight client. Cases marked as using
//! the rate limiter run Twilight's own limiter, permit queue and 429 retries.
use super::{CardRecords, Outcome, active_text, activity_monitor, final_text, finishing_text};
use crate::{
    client::test_client as core_client,
    config::{DiscordActivation, DiscordConfig, DiscordConversation},
    conversations::tests::discord_config,
};
use anyhow::Context as _;
use axum::{
    Json, Router,
    body::Bytes,
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri},
    response::{IntoResponse as _, Response},
};
use carillon_protocol::model::{
    ActiveTurn, ActivityPhase, DeliveryOutcome, FinishedTool, RunningTool, TurnActivity, TurnState,
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{sync::mpsc, task::JoinHandle, time::Instant};

/// A request the substitute received.
#[derive(Clone, Debug)]
struct Seen {
    at: Instant,
    method: Method,
    path: String,
    body: Value,
}

impl Seen {
    fn is(&self, method: &Method, path: &str) -> bool {
        self.method == method && self.path == path
    }

    fn content(&self) -> &str {
        self.body["content"].as_str().unwrap_or_default()
    }
}

/// How the substitute answers one Discord request.
#[derive(Clone, Copy)]
enum Reply {
    Success,
    /// Success, reporting a bucket with this many requests remaining until it
    /// resets after this many seconds.
    Bucket(&'static str, u32, u32),
    Status(u16),
    /// A user-scoped 429 asking to wait this many seconds.
    Limited(u32),
}

type Script = Arc<dyn Fn(&Seen, usize) -> Reply + Send + Sync>;

struct Substitute {
    address: std::net::SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
    /// What core lists; `None` answers as an unavailable core.
    active: Arc<Mutex<Option<Vec<ActiveTurn>>>>,
    server: JoinHandle<()>,
}

impl Substitute {
    async fn start(script: Script) -> anyhow::Result<Self> {
        let seen = Arc::new(Mutex::new(Vec::<Seen>::new()));
        let active = Arc::new(Mutex::new(Some(Vec::new())));
        let (recorded, listed) = (seen.clone(), active.clone());
        let app = Router::new().fallback(move |method: Method, uri: Uri, body: Bytes| {
            let (seen, active, script) = (recorded.clone(), listed.clone(), script.clone());
            async move {
                if uri.path() == "/v1/activity" {
                    return match lock(&active).clone() {
                        Some(turns) => Json(turns).into_response(),
                        None => StatusCode::SERVICE_UNAVAILABLE.into_response(),
                    };
                }
                let request = Seen {
                    at: Instant::now(),
                    method,
                    path: uri.path().trim_start_matches("/api/v10").to_owned(),
                    body: serde_json::from_slice(&body).unwrap_or(Value::Null),
                };
                let (earlier, creates) = {
                    let mut seen = lock(&seen);
                    let earlier = seen
                        .iter()
                        .filter(|other| other.is(&request.method, &request.path))
                        .count();
                    seen.push(request.clone());
                    let creates = seen
                        .iter()
                        .filter(|other| {
                            other.method == Method::POST && other.path.ends_with("/messages")
                        })
                        .count();
                    (earlier, creates)
                };
                answer(&request, script(&request, earlier), creates)
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(Self {
            address,
            seen,
            active,
            server,
        })
    }

    fn list(&self, turns: Vec<ActiveTurn>) {
        *lock(&self.active) = Some(turns);
    }

    fn seen(&self) -> Vec<Seen> {
        lock(&self.seen).clone()
    }

    fn matching(&self, method: &Method, prefix: &str) -> Vec<Seen> {
        self.seen()
            .into_iter()
            .filter(|seen| seen.method == method && seen.path.starts_with(prefix))
            .collect()
    }

    /// Waits until `count` requests match, returning them.
    async fn wait_for(
        &self,
        method: &Method,
        prefix: &str,
        count: usize,
    ) -> anyhow::Result<Vec<Seen>> {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let matching = self.matching(method, prefix);
                if matching.len() >= count {
                    return matching;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .with_context(|| format!("expected {count} {method} {prefix}: {:#?}", self.seen()))
    }

    fn http(&self, limiter: bool) -> Arc<twilight_http::Client> {
        let builder = twilight_http::Client::builder()
            .token("test-only".to_owned())
            .proxy(self.address.to_string(), true);
        Arc::new(
            if limiter {
                builder
            } else {
                builder.ratelimiter(None)
            }
            .build(),
        )
    }

    fn monitor(
        &self,
        http: Arc<twilight_http::Client>,
        config: DiscordConfig,
        records: &Path,
    ) -> anyhow::Result<(mpsc::UnboundedSender<Outcome>, JoinHandle<()>)> {
        let (outcomes, observed) = mpsc::unbounded_channel();
        let monitor = activity_monitor(
            core_client(format!("http://{}", self.address)),
            http,
            config,
            Arc::new(CardRecords::load(records.to_path_buf())?),
            observed,
        );
        Ok((outcomes, tokio::spawn(monitor)))
    }
}

impl Drop for Substitute {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn answer(request: &Seen, reply: Reply, creates: usize) -> Response {
    let message = |id: String| {
        Json(json!({
            "id": id,
            "channel_id": request.path.split('/').nth(2).unwrap_or("300"),
            "author": {"id": "201", "username": "carillon", "discriminator": "0"},
            "content": request.body["content"],
            "attachments": [],
            "embeds": [],
            "mention_everyone": false,
            "mention_roles": [],
            "mentions": [],
            "pinned": false,
            "timestamp": "2026-09-28T00:00:00.000000+00:00",
            "tts": false,
            "type": 0,
        }))
    };
    let success = || match request.method {
        Method::POST if request.path.ends_with("/messages") => {
            message((900 + creates).to_string()).into_response()
        }
        Method::PATCH => {
            message(request.path.rsplit('/').next().unwrap_or("1").to_owned()).into_response()
        }
        _ => StatusCode::NO_CONTENT.into_response(),
    };
    match reply {
        Reply::Success => success(),
        Reply::Bucket(bucket, remaining, reset_after) => {
            let mut response = success();
            limit_headers(response.headers_mut(), bucket, remaining, reset_after);
            response
        }
        Reply::Status(status) => (
            StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(json!({"code": 0, "message": "substitute refusal"})),
        )
            .into_response(),
        Reply::Limited(retry_after) => {
            let mut response = (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({
                    "message": "You are being rate limited.",
                    "retry_after": retry_after,
                    "global": false,
                })),
            )
                .into_response();
            limit_headers(response.headers_mut(), "limited", 0, retry_after);
            response
                .headers_mut()
                .insert("retry-after", HeaderValue::from(retry_after));
            response
        }
    }
}

fn limit_headers(headers: &mut HeaderMap, bucket: &'static str, remaining: u32, reset_after: u32) {
    headers.insert("x-ratelimit-bucket", HeaderValue::from_static(bucket));
    headers.insert("x-ratelimit-limit", HeaderValue::from(remaining + 1));
    headers.insert("x-ratelimit-remaining", HeaderValue::from(remaining));
    headers.insert("x-ratelimit-reset-after", HeaderValue::from(reset_after));
    headers.insert("x-ratelimit-scope", HeaderValue::from_static("user"));
}

fn succeed() -> Script {
    Arc::new(|_, _| Reply::Success)
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
        .unwrap_or_default()
}

fn running(
    turn_id: &str,
    channel: &str,
    accepted_at: i64,
    activity: Option<TurnActivity>,
) -> ActiveTurn {
    ActiveTurn {
        turn_id: turn_id.to_owned(),
        state: TurnState::Running,
        target_id: channel.to_owned(),
        accepted_at,
        activity,
    }
}

fn tools(
    headline: Option<&str>,
    running: &[(&str, &str)],
    recent: &[(&str, &str, bool)],
) -> TurnActivity {
    TurnActivity {
        phase: if running.is_empty() {
            ActivityPhase::Model
        } else {
            ActivityPhase::Tools
        },
        headline: headline.map(str::to_owned),
        running: running
            .iter()
            .map(|(tool, detail)| RunningTool {
                tool: (*tool).to_owned(),
                detail: Some((*detail).to_owned()),
                started_at: 1_790_000_000,
            })
            .collect(),
        recent: recent
            .iter()
            .map(|(tool, detail, ok)| FinishedTool {
                tool: (*tool).to_owned(),
                detail: Some((*detail).to_owned()),
                ok: *ok,
            })
            .collect(),
        tool_calls: u32::try_from(running.len() + recent.len()).unwrap_or_default(),
    }
}

fn config(typing: bool) -> DiscordConfig {
    let mut config = discord_config(DiscordActivation::Always, DiscordConversation::Channel);
    config.typing = typing;
    config
}

fn settle(
    outcomes: &mpsc::UnboundedSender<Outcome>,
    turn_id: &str,
    turn_state: TurnState,
    delivery: DeliveryOutcome,
) -> anyhow::Result<()> {
    outcomes
        .send(Outcome {
            turn_id: turn_id.to_owned(),
            turn_state,
            delivery,
        })
        .map_err(|_| anyhow::anyhow!("monitor stopped"))
}

fn recorded(path: &PathBuf) -> anyhow::Result<Value> {
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

#[tokio::test]
async fn a_slow_turn_gets_one_delayed_card_that_follows_it_and_is_removed_after_delivery()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    let discord = Substitute::start(succeed()).await?;
    let accepted = now();
    let slow = |activity| running("slow", "300", accepted, Some(activity));
    let testing = || {
        let command = ("bash", "cargo test -p carillon-discord …");
        tools(Some("@everyone: checking <@200>'s tests"), &[command], &[])
    };
    discord.list(vec![
        slow(testing()),
        running("fast", "301", accepted, None),
    ]);
    let started = Instant::now();
    let (outcomes, monitor) = discord.monitor(discord.http(false), config(false), &records)?;
    // A turn that ends before the appearance delay never shows a card.
    tokio::time::sleep(Duration::from_millis(500)).await;
    discord.list(vec![slow(testing())]);
    let (completed, delivered) = (TurnState::Completed, DeliveryOutcome::Delivered);
    settle(&outcomes, "fast", completed, delivered)?;

    let created = discord
        .wait_for(&Method::POST, "/channels/300/messages", 1)
        .await?;
    let card = &created[0];
    assert!(
        card.at - started >= Duration::from_millis(900),
        "appeared too soon"
    );
    let text = card.content();
    assert!(text.starts_with(&format!("⏳ Working · started <t:{accepted}:R>")));
    assert!(text.contains("> @everyone: checking <@200>'s tests"));
    assert!(text.contains("↳ bash `cargo test -p carillon-discord …` · <t:1790000000:R>"));
    assert_eq!(card.body["allowed_mentions"], json!({"parse": []}));
    let expected = json!({"channel_id": "300", "message_id": "901"});
    assert_eq!(recorded(&records)?["slow"], expected);

    let history = [
        ("read", "src/lib.rs", true),
        ("bash", "cargo test -p carillon-discord …", false),
    ];
    discord.list(vec![slow(tools(
        None,
        &[("bash", "cargo build")],
        &history,
    ))]);
    let edited = discord
        .wait_for(&Method::PATCH, "/channels/300/messages/901", 1)
        .await?;
    assert!(
        edited[0].at - card.at >= Duration::from_millis(2_900),
        "edited too soon"
    );
    let text = edited[0].content();
    assert!(text.contains("✓ read `src/lib.rs`"));
    assert!(text.contains("✗ bash `cargo test -p carillon-discord …`"));
    assert!(text.contains("↳ bash `cargo build`"));
    assert_eq!(edited[0].body["allowed_mentions"], json!({"parse": []}));
    // An unchanged turn is not edited again.
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    assert_eq!(discord.matching(&Method::PATCH, "/channels").len(), 1);

    // Core stopped listing it: nothing is shown as still running.
    discord.list(Vec::new());
    let finishing = discord
        .wait_for(&Method::PATCH, "/channels/300/messages/901", 2)
        .await?;
    let text = finishing[1].content();
    assert!(text.starts_with("⏳ Finishing up") && text.contains("✓ read `src/lib.rs`"));
    assert!(!text.contains('↳'));

    settle(&outcomes, "slow", completed, delivered)?;
    let deleted = discord
        .wait_for(&Method::DELETE, "/channels/300/messages/901", 1)
        .await?;
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    let seen = discord.seen();
    assert!(
        seen.iter().all(|request| request.at <= deleted[0].at),
        "{seen:#?}"
    );
    assert_eq!(discord.matching(&Method::POST, "/channels").len(), 1);
    assert_eq!(recorded(&records)?, json!({}));
    monitor.abort();
    Ok(())
}

#[tokio::test]
async fn ended_turns_keep_an_honest_card_and_stale_reads_cannot_revive_them() -> anyhow::Result<()>
{
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    let discord = Substitute::start(succeed()).await?;
    let history = tools(
        None,
        &[("bash", "make deploy")],
        &[("read", "Makefile", true)],
    );
    let turns = [
        (
            "uncertain",
            "300",
            TurnState::Completed,
            DeliveryOutcome::Uncertain,
        ),
        (
            "stopped",
            "301",
            TurnState::Interrupted,
            DeliveryOutcome::Delivered,
        ),
        (
            "both",
            "302",
            TurnState::Interrupted,
            DeliveryOutcome::Uncertain,
        ),
    ];
    // Accepted long ago, as after a provider restart: cards appear at once.
    discord.list(
        turns
            .iter()
            .map(|(turn, channel, ..)| running(turn, channel, now() - 60, Some(history.clone())))
            .collect(),
    );
    let (outcomes, monitor) = discord.monitor(discord.http(false), config(true), &records)?;
    for channel in ["300", "301", "302"] {
        discord
            .wait_for(&Method::POST, &format!("/channels/{channel}/messages"), 1)
            .await?;
        discord
            .wait_for(&Method::POST, &format!("/channels/{channel}/typing"), 1)
            .await?;
    }
    // The outcomes arrive while core's reads still list the turns.
    for (turn, _, turn_state, delivery) in &turns {
        settle(&outcomes, turn, *turn_state, *delivery)?;
    }
    let settled = Instant::now();
    let finals = discord.wait_for(&Method::PATCH, "/channels/30", 3).await?;
    let text = |channel: &str| {
        finals
            .iter()
            .find(|seen| seen.path.starts_with(&format!("/channels/{channel}/")))
            .map(|seen| seen.content().to_owned())
            .unwrap_or_default()
    };
    assert!(text("300").starts_with("⚠️ Finished, but the reply may not have been delivered."));
    assert!(text("301").starts_with("⏹️ Stopped before finishing.\n"));
    assert!(text("302").starts_with(
        "⏹️ Stopped before finishing; the notice about it may not have been delivered."
    ));
    for channel in ["300", "301", "302"] {
        assert!(text(channel).contains("✓ read `Makefile`"));
        assert!(!text(channel).contains('↳'));
    }
    tokio::time::sleep(Duration::from_millis(4_000)).await;
    let seen = discord.seen();
    assert!(seen.iter().all(|request| request.method != Method::DELETE));
    assert_eq!(
        discord.matching(&Method::PATCH, "/channels").len(),
        3,
        "{seen:#?}"
    );
    assert_eq!(
        discord
            .matching(&Method::POST, "/channels/30")
            .iter()
            .filter(|seen| seen.path.ends_with("/messages"))
            .count(),
        3
    );
    assert!(
        seen.iter()
            .all(|request| request.at < settled + Duration::from_secs(1)
                || request.method == Method::PATCH),
        "typing or cards continued after the outcome: {seen:#?}"
    );
    assert_eq!(recorded(&records)?, json!({}));
    monitor.abort();
    Ok(())
}

#[tokio::test]
async fn uncertain_or_refused_cards_are_never_created_or_edited_again() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    let discord = Substitute::start(Arc::new(|seen, _| {
        match (&seen.method, seen.path.as_str()) {
            (&Method::POST, "/channels/300/messages") => Reply::Status(500),
            (&Method::POST, "/channels/301/messages") => Reply::Status(403),
            (&Method::PATCH, _) => Reply::Status(404),
            _ => Reply::Success,
        }
    }))
    .await?;
    let long_ago = now() - 60;
    let turns = |detail: &str| {
        ["300", "301", "302"]
            .iter()
            .map(|channel| {
                running(
                    channel,
                    channel,
                    long_ago,
                    Some(tools(None, &[("bash", detail)], &[])),
                )
            })
            .collect()
    };
    discord.list(turns("first"));
    let (outcomes, monitor) = discord.monitor(discord.http(false), config(true), &records)?;
    for channel in ["300", "301", "302"] {
        discord
            .wait_for(&Method::POST, &format!("/channels/{channel}/messages"), 1)
            .await?;
    }
    discord.list(turns("second"));
    discord
        .wait_for(&Method::PATCH, "/channels/302/messages", 1)
        .await?;
    discord.list(turns("third"));
    tokio::time::sleep(Duration::from_millis(8_000)).await;
    let creates = discord
        .matching(&Method::POST, "/channels/30")
        .into_iter()
        .filter(|seen| seen.path.ends_with("/messages"))
        .count();
    assert_eq!(creates, 3, "{:#?}", discord.seen());
    // A card Discord no longer has is neither edited again nor replaced.
    assert_eq!(discord.matching(&Method::PATCH, "/channels").len(), 1);
    // Typing does not depend on cards.
    assert!(
        discord
            .matching(&Method::POST, "/channels/300/typing")
            .len()
            >= 2
    );
    assert_eq!(recorded(&records)?, json!({}));
    settle(
        &outcomes,
        "300",
        TurnState::Completed,
        DeliveryOutcome::Delivered,
    )?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(discord.matching(&Method::DELETE, "/channels").is_empty());
    monitor.abort();
    Ok(())
}

#[tokio::test]
async fn a_restart_removes_recorded_cards_before_any_replacement_and_keeps_uncleared_ones()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    std::fs::write(
        &records,
        serde_json::to_vec(&json!({
            "replaced": {"channel_id": "300", "message_id": "555"},
            "finished": {"channel_id": "301", "message_id": "556"},
            "unanswered": {"channel_id": "302", "message_id": "557"},
            "forbidden": {"channel_id": "303", "message_id": "558"},
        }))?,
    )?;
    let discord = Substitute::start(Arc::new(|seen, _| match seen.path.as_str() {
        "/channels/301/messages/556" => Reply::Status(404),
        "/channels/302/messages/557" => Reply::Status(500),
        "/channels/303/messages/558" => Reply::Status(403),
        _ => Reply::Success,
    }))
    .await?;
    let long_ago = now() - 60;
    let activity = Some(tools(None, &[("bash", "sleep 60")], &[]));
    discord.list(
        [
            ("replaced", "300"),
            ("unanswered", "302"),
            ("forbidden", "303"),
        ]
        .iter()
        .map(|(turn, channel)| running(turn, channel, long_ago, activity.clone()))
        .collect(),
    );
    let (_outcomes, monitor) = discord.monitor(discord.http(false), config(false), &records)?;
    let replacement = discord
        .wait_for(&Method::POST, "/channels/300/messages", 1)
        .await?;
    let removed = discord
        .wait_for(&Method::DELETE, "/channels/300/messages/555", 1)
        .await?;
    assert!(removed[0].at < replacement[0].at);
    discord
        .wait_for(&Method::DELETE, "/channels/302/messages/557", 3)
        .await?;
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    assert_eq!(
        discord
            .matching(&Method::DELETE, "/channels/301/messages/556")
            .len(),
        1
    );
    assert_eq!(
        discord
            .matching(&Method::DELETE, "/channels/302/messages/557")
            .len(),
        3
    );
    assert_eq!(
        discord
            .matching(&Method::DELETE, "/channels/303/messages/558")
            .len(),
        1
    );
    // A card whose removal was not established is never replaced.
    for channel in ["301", "302", "303"] {
        assert!(
            discord
                .matching(&Method::POST, &format!("/channels/{channel}"))
                .is_empty()
        );
    }
    assert_eq!(
        recorded(&records)?,
        json!({
            "replaced": {"channel_id": "300", "message_id": "901"},
            "unanswered": {"channel_id": "302", "message_id": "557"},
            "forbidden": {"channel_id": "303", "message_id": "558"},
        })
    );
    monitor.abort();
    Ok(())
}

#[tokio::test]
async fn disabled_cards_still_type_while_turns_run_and_an_unreachable_core_changes_nothing()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    let discord = Substitute::start(succeed()).await?;
    let mut disabled = config(true);
    disabled.activity = false;
    discord.list(vec![running(
        "turn",
        "300",
        now() - 60,
        Some(tools(None, &[("bash", "x")], &[])),
    )]);
    let (_outcomes, monitor) = discord.monitor(discord.http(false), disabled, &records)?;
    discord
        .wait_for(&Method::POST, "/channels/300/typing", 1)
        .await?;
    *lock(&discord.active) = None;
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    assert!(
        discord
            .matching(&Method::POST, "/channels/300/messages")
            .is_empty()
    );
    assert!(
        discord
            .seen()
            .iter()
            .all(|seen| seen.path == "/channels/300/typing")
    );
    monitor.abort();

    // With cards enabled, an unreachable core neither finishes nor removes a card.
    let discord = Substitute::start(succeed()).await?;
    discord.list(vec![running(
        "turn",
        "300",
        now() - 60,
        Some(tools(None, &[("bash", "x")], &[])),
    )]);
    let (_outcomes, monitor) = discord.monitor(discord.http(false), config(false), &records)?;
    discord
        .wait_for(&Method::POST, "/channels/300/messages", 1)
        .await?;
    *lock(&discord.active) = None;
    tokio::time::sleep(Duration::from_millis(6_500)).await;
    assert_eq!(discord.seen().len(), 1, "{:#?}", discord.seen());
    discord.list(Vec::new());
    let finishing = discord
        .wait_for(&Method::PATCH, "/channels/300/messages/901", 1)
        .await?;
    assert!(finishing[0].content().starts_with("⏳ Finishing up"));
    monitor.abort();
    Ok(())
}

#[tokio::test]
async fn rate_limited_creation_retries_inside_twilight_and_makes_one_card() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    let discord = Substitute::start(Arc::new(|seen, earlier| {
        if seen.method == Method::POST && seen.path == "/channels/300/messages" && earlier < 2 {
            Reply::Limited(1)
        } else {
            Reply::Success
        }
    }))
    .await?;
    discord.list(vec![running(
        "turn",
        "300",
        now() - 60,
        Some(tools(None, &[("bash", "x")], &[])),
    )]);
    let (_outcomes, monitor) = discord.monitor(discord.http(true), config(false), &records)?;
    let attempts = discord
        .wait_for(&Method::POST, "/channels/300/messages", 3)
        .await?;
    // Twilight waited out each limit before its own retry.
    assert!(attempts[2].at - attempts[0].at >= Duration::from_millis(1_900));
    tokio::time::sleep(Duration::from_millis(4_000)).await;
    assert_eq!(
        discord
            .matching(&Method::POST, "/channels/300/messages")
            .len(),
        3
    );
    assert_eq!(recorded(&records)?["turn"]["message_id"], "903");
    monitor.abort();
    Ok(())
}

#[tokio::test]
async fn progress_stuck_behind_the_limiter_yields_to_outcomes_and_answers() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    // After its first edit, the first card's edits wait a minute for the limiter.
    let discord = Substitute::start(Arc::new(|seen, _| {
        match (&seen.method, seen.path.as_str()) {
            (&Method::PATCH, path) if path.starts_with("/channels/300/") => {
                Reply::Bucket("edits", 0, 60)
            }
            _ => Reply::Success,
        }
    }))
    .await?;
    let http = discord.http(true);
    let long_ago = now() - 60;
    let turns = |detail: &str| {
        vec![
            running(
                "blocked",
                "300",
                long_ago,
                Some(tools(None, &[("bash", detail)], &[])),
            ),
            running(
                "other",
                "301",
                long_ago,
                Some(tools(None, &[("bash", detail)], &[])),
            ),
        ]
    };
    discord.list(turns("one"));
    let (outcomes, monitor) = discord.monitor(http.clone(), config(false), &records)?;
    discord
        .wait_for(&Method::POST, "/channels/300/messages", 1)
        .await?;
    discord
        .wait_for(&Method::POST, "/channels/301/messages", 1)
        .await?;
    discord.list(turns("two"));
    discord
        .wait_for(&Method::PATCH, "/channels/300/", 1)
        .await?;
    discord
        .wait_for(&Method::PATCH, "/channels/301/", 1)
        .await?;
    // The next edit of the first card now waits for a permit.
    discord.list(turns("three"));
    tokio::time::sleep(Duration::from_millis(4_000)).await;
    assert_eq!(discord.matching(&Method::PATCH, "/channels/300/").len(), 1);

    // An answer to the same channel, on its own route, is not held by that edit.
    let started = Instant::now();
    crate::delivery::send_discord_message(&http, twilight_model::id::Id::new(300), "the answer")
        .await?;
    assert!(started.elapsed() < Duration::from_secs(1));

    // Both outcomes are acted on promptly: the waiting edit is dropped unsent.
    settle(
        &outcomes,
        "blocked",
        TurnState::Completed,
        DeliveryOutcome::Delivered,
    )?;
    settle(
        &outcomes,
        "other",
        TurnState::Completed,
        DeliveryOutcome::Delivered,
    )?;
    let settled = Instant::now();
    let blocked = discord
        .wait_for(&Method::DELETE, "/channels/300/", 1)
        .await?;
    let other = discord
        .wait_for(&Method::DELETE, "/channels/301/", 1)
        .await?;
    assert!(blocked[0].at - settled < Duration::from_secs(1));
    assert!(other[0].at - settled < Duration::from_secs(1));
    assert_eq!(discord.matching(&Method::PATCH, "/channels/300/").len(), 1);
    monitor.abort();
    Ok(())
}

#[tokio::test]
async fn an_answer_waits_for_progress_only_in_a_bucket_discord_shares() -> anyhow::Result<()> {
    // Discord may place edits and messages in one learned bucket; distinct
    // routes alone do not isolate them. This shows the cost the design accepts.
    for shared in [false, true] {
        let discord = Substitute::start(Arc::new(move |seen, _| match (&seen.method, shared) {
            (&Method::PATCH, true) => Reply::Bucket("messages", 0, 2),
            (&Method::PATCH, false) => Reply::Bucket("edits", 0, 2),
            (_, true) => Reply::Bucket("messages", 1, 2),
            (_, false) => Reply::Bucket("answers", 1, 2),
        }))
        .await?;
        let directory = tempfile::tempdir()?;
        let http = discord.http(true);
        let channel = twilight_model::id::Id::new(300);
        // Twilight learns a route's bucket from its first response.
        crate::delivery::send_discord_message(&http, channel, "first answer").await?;
        let card = super::Card {
            channel_id: channel,
            message_id: twilight_model::id::Id::new(901),
        };
        let cards = super::Cards {
            http: http.clone(),
            config: config(false),
            records: Arc::new(CardRecords::load(directory.path().join("cards.json"))?),
        };
        assert_eq!(
            cards.edit(card, "progress", None).await,
            super::Call::Done(())
        );
        let started = Instant::now();
        crate::delivery::send_discord_message(&http, channel, "second answer").await?;
        let waited = started.elapsed();
        if shared {
            assert!(waited >= Duration::from_millis(1_500), "{waited:?}");
        } else {
            assert!(waited < Duration::from_millis(500), "{waited:?}");
        }
    }
    Ok(())
}

#[test]
fn cards_show_bounded_safe_text() {
    let mut last = None;
    let queued = ActiveTurn {
        state: TurnState::Queued,
        ..running("t", "300", 1_790_000_000, None)
    };
    assert_eq!(
        active_text(&queued, &mut last),
        "⏳ Waiting for earlier work to finish · queued <t:1790000000:R>"
    );
    let mut busy = tools(
        Some(&"long commentary ".repeat(20)),
        &[
            ("bash", "echo `id` > x"),
            ("read", "a"),
            ("read", "b"),
            ("read", "c"),
            ("read", "d"),
        ],
        &[
            ("discord_messages", "messages.send channel 300", true),
            ("mcp<@1>", "x", false),
        ],
    );
    busy.tool_calls = 12;
    let text = active_text(
        &running("t", "300", 1_790_000_000, Some(busy.clone())),
        &mut last,
    );
    let lines = text.lines().collect::<Vec<_>>();
    assert_eq!(lines[0], "⏳ Working · started <t:1790000000:R>");
    assert_eq!(lines[1].chars().count(), 102);
    assert_eq!(lines[2], "… 5 earlier calls");
    assert_eq!(lines[3], "✓ discord_messages `messages.send channel 300`");
    assert_eq!(lines[4], "✗ mcp??1? `x`");
    assert_eq!(lines[5], "↳ bash `echo ˋidˋ > x` · <t:1790000000:R>");
    assert_eq!(lines[8], "↳ … and 2 more running");
    assert_eq!(lines.len(), 9);
    // Without a current snapshot the last one's history remains, nothing runs.
    let unknown = active_text(&running("t", "300", 1_790_000_000, None), &mut last);
    assert!(unknown.starts_with("⏳ Working · started <t:1790000000:R>\n… 10 earlier calls"));
    assert!(!unknown.contains('↳') && !unknown.contains("long commentary"));
    assert_eq!(
        finishing_text(last.as_ref()).lines().next(),
        Some("⏳ Finishing up")
    );
    busy.phase = ActivityPhase::Retrying;
    busy.running.clear();
    assert!(
        active_text(&running("t", "300", 1, Some(busy.clone())), &mut last)
            .starts_with("⏳ Working · Retrying the model request · started <t:1:R>")
    );
    assert_eq!(
        final_text(
            TurnState::Completed,
            DeliveryOutcome::Delivered,
            Some(&busy)
        ),
        None
    );
    busy.recent = vec![
        FinishedTool {
            tool: "bash".to_owned(),
            detail: Some("x".repeat(3_000)),
            ok: true,
        };
        5
    ];
    let long = active_text(&running("t", "300", 1, Some(busy)), &mut last);
    assert!(long.chars().count() < 2_000);
    assert!(long.lines().all(|line| line.chars().count() <= 130));
}
