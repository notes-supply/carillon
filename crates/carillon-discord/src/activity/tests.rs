//! The monitor against a local substitute serving core's activity list and
//! deliveries and Discord's API through a real Twilight client, beside the
//! delivery loop where a case needs it. Cases marked as using the rate
//! limiter run Twilight's own limiter, permit queue and 429 retries.
use super::{CardRecords, Outcome, Summaries, activity_monitor};
use crate::{
    client::test_client as core_client,
    config::{DiscordActivation, DiscordConfig, DiscordConversation},
    conversations::tests::discord_config,
    delivery::{Reactions, delivery_loop},
};
use anyhow::Context as _;
use axum::{
    Json, Router,
    body::Bytes,
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri},
    response::{IntoResponse as _, Response},
};
use carillon_protocol::model::{
    ActiveTurn, ActivityPhase, Delivery, DeliveryOutcome, FinishedTool, Origin, RunningTool,
    TurnActivity, TurnState, WorkItem, WorkKind, WorkState, WorkSummary,
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{OwnedMutexGuard, mpsc},
    task::JoinHandle,
    time::Instant,
};
use tokio_util::sync::CancellationToken;

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

    /// What the message shows: its card's description and each field under
    /// its name, or its text when it has no card.
    fn content(&self) -> String {
        let Some(embed) = self.body["embeds"].get(0) else {
            return self.body["content"].as_str().unwrap_or_default().to_owned();
        };
        let mut parts = vec![embed["description"].as_str().unwrap_or_default().to_owned()];
        for field in embed["fields"].as_array().into_iter().flatten() {
            parts.push(format!(
                "{}:\n{}",
                field["name"].as_str().unwrap_or_default(),
                field["value"].as_str().unwrap_or_default()
            ));
        }
        parts.join("\n")
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
    /// Discord performed the request, but its answer was lost.
    Lost,
    /// Success, answered this much later.
    Delayed(Duration),
}

type Script = Arc<dyn Fn(&Seen, usize) -> Reply + Send + Sync>;

struct Substitute {
    address: std::net::SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
    /// What core lists; `None` answers as an unavailable core.
    active: Arc<Mutex<Option<Vec<ActiveTurn>>>>,
    /// What core hands the delivery loop next, in order.
    deliveries: Arc<Mutex<VecDeque<Delivery>>>,
    /// Core records no delivery while a case holds this.
    recording: Arc<tokio::sync::Mutex<()>>,
    server: JoinHandle<()>,
}

impl Substitute {
    async fn start(script: Script) -> anyhow::Result<Self> {
        let seen = Arc::new(Mutex::new(Vec::<Seen>::new()));
        let active = Arc::new(Mutex::new(Some(Vec::new())));
        let deliveries = Arc::new(Mutex::new(VecDeque::new()));
        let recording = Arc::new(tokio::sync::Mutex::new(()));
        let (recorded, listed) = (seen.clone(), active.clone());
        let (queued, acknowledged) = (deliveries.clone(), recording.clone());
        let app = Router::new().fallback(move |method: Method, uri: Uri, body: Bytes| {
            let (seen, active, script) = (recorded.clone(), listed.clone(), script.clone());
            let (deliveries, recording) = (queued.clone(), acknowledged.clone());
            async move {
                if uri.path() == "/v1/activity" {
                    return match lock(&active).clone() {
                        Some(turns) => Json(turns).into_response(),
                        None => StatusCode::SERVICE_UNAVAILABLE.into_response(),
                    };
                }
                if uri.path() == "/v1/deliveries" {
                    return Json(lock(&deliveries).pop_front()).into_response();
                }
                if uri.path().starts_with("/v1/deliveries/") {
                    let _recorded = recording.lock().await;
                    return StatusCode::NO_CONTENT.into_response();
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
                let reply = script(&request, earlier);
                if let Reply::Delayed(after) = reply {
                    tokio::time::sleep(after).await;
                }
                answer(&request, reply, creates)
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
            deliveries,
            recording,
            server,
        })
    }

    fn list(&self, turns: Vec<ActiveTurn>) {
        *lock(&self.active) = Some(turns);
    }

    /// Core hands `delivery` to the delivery loop's next claim.
    fn deliver(&self, delivery: Delivery) {
        lock(&self.deliveries).push_back(delivery);
    }

    /// Holds every acknowledgement until the guard is dropped, as a core
    /// slow to record deliveries would.
    async fn hold_recording(&self) -> OwnedMutexGuard<()> {
        self.recording.clone().lock_owned().await
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

    /// Waits until a request to `prefix` shows `text`, returning it.
    async fn wait_for_text(
        &self,
        method: &Method,
        prefix: &str,
        text: &str,
    ) -> anyhow::Result<Seen> {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Some(seen) = self
                    .matching(method, prefix)
                    .into_iter()
                    .find(|seen| seen.content().contains(text))
                {
                    return seen;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .with_context(|| {
            format!(
                "expected {method} {prefix} showing {text}: {:#?}",
                self.seen()
            )
        })
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
    ) -> (mpsc::UnboundedSender<Outcome>, JoinHandle<()>) {
        let (outcomes, observed) = mpsc::unbounded_channel();
        let monitor = activity_monitor(
            core_client(format!("http://{}", self.address)),
            http,
            config,
            CardRecords::open(records.to_path_buf()).map(Arc::new),
            observed,
            CancellationToken::new(),
        );
        (outcomes, tokio::spawn(monitor))
    }

    /// The monitor beside the delivery loop, sharing the card records and
    /// the stop as the provider does. Deliveries add no reactions.
    fn provider(
        &self,
        http: Arc<twilight_http::Client>,
        mut config: DiscordConfig,
        records: &Path,
    ) -> Provider {
        config.reactions = false;
        let core = core_client(format!("http://{}", self.address));
        let records = CardRecords::open(records.to_path_buf()).map(Arc::new);
        let (outcomes, observed) = mpsc::unbounded_channel();
        let stopping = CancellationToken::new();
        let delivering = delivery_loop(
            core.clone(),
            Reactions::new(http.clone(), config.clone()),
            http.clone(),
            Summaries::new(records.clone(), &config),
            outcomes,
            stopping.clone(),
        );
        let monitor = activity_monitor(core, http, config, records, observed, stopping.clone());
        Provider {
            monitor: tokio::spawn(monitor),
            delivering: tokio::spawn(delivering),
            stopping,
        }
    }
}

/// The provider's monitor and delivery loop, aborted when dropped.
struct Provider {
    monitor: JoinHandle<()>,
    delivering: JoinHandle<anyhow::Result<()>>,
    stopping: CancellationToken,
}

impl Provider {
    /// Stops as the provider's connection does: the deliveries end first,
    /// then the monitor that takes their outcomes.
    async fn stop(&mut self) -> anyhow::Result<()> {
        self.stopping.cancel();
        (&mut self.delivering).await??;
        Ok((&mut self.monitor).await?)
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.monitor.abort();
        self.delivering.abort();
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
            // A card has no text: Discord answers with an empty one.
            "content": request.body["content"].as_str().unwrap_or_default(),
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
        Reply::Success | Reply::Delayed(_) => success(),
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
        Reply::Lost => (StatusCode::OK, "{truncated").into_response(),
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

/// A running message turn, the origin of its own job.
fn running(
    turn_id: &str,
    channel: &str,
    accepted_at: i64,
    activity: Option<TurnActivity>,
) -> ActiveTurn {
    continuing(turn_id, turn_id, channel, accepted_at, activity)
}

/// A running turn of the job `origin` began, such as an observation.
fn continuing(
    turn_id: &str,
    origin: &str,
    channel: &str,
    accepted_at: i64,
    activity: Option<TurnActivity>,
) -> ActiveTurn {
    ActiveTurn {
        turn_id: turn_id.to_owned(),
        state: TurnState::Running,
        target_id: channel.to_owned(),
        accepted_at,
        origin: Origin {
            turn_id: origin.to_owned(),
            accepted_at,
        },
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
                estimated: false,
            })
            .collect(),
        recent: recent
            .iter()
            .map(|(tool, detail, ok)| FinishedTool {
                tool: (*tool).to_owned(),
                detail: Some((*detail).to_owned()),
                ok: *ok,
                duration_ms: None,
            })
            .collect(),
        tool_calls: u32::try_from(running.len() + recent.len()).unwrap_or_default(),
        work: None,
        notes: Vec::new(),
        agent: None,
        partial: false,
    }
}

/// `activity` with one delegated child in `state`.
fn delegating(mut activity: TurnActivity, state: WorkState) -> TurnActivity {
    activity.work = Some(WorkSummary {
        items: vec![WorkItem {
            id: "child".to_owned(),
            kind: WorkKind::Delegation,
            label: "research".to_owned(),
            state,
            parent: None,
            actor: None,
            started_at: None,
            estimated: false,
            duration_ms: (state == WorkState::Succeeded).then_some(61_000),
        }],
        partial: false,
        omitted: None,
    });
    activity
}

fn config(typing: bool) -> DiscordConfig {
    let mut config = discord_config(DiscordActivation::Always, DiscordConversation::Channel);
    config.typing = typing;
    config
}

/// The delivery of a turn of the job `origin`, finished now.
fn delivered(turn_id: &str, origin: &str, turn_state: TurnState) -> Delivery {
    Delivery {
        turn_state,
        id: 1,
        turn_id: turn_id.to_owned(),
        target_id: "300".to_owned(),
        feedback_target_id: "300:400".to_owned(),
        content: "answer".to_owned(),
        origin: Origin {
            turn_id: origin.to_owned(),
            accepted_at: now() - 60,
        },
        finished_at: now(),
        activity: None,
        continued_by: None,
    }
}

fn report(
    outcomes: &mpsc::UnboundedSender<Outcome>,
    delivery: Delivery,
    outcome: DeliveryOutcome,
) -> anyhow::Result<()> {
    outcomes
        .send(Outcome {
            delivery,
            result: outcome,
            hosted: None,
        })
        .map_err(|_| anyhow::anyhow!("monitor stopped"))
}

/// The last outcome of a message turn's job, without a report.
fn settle(
    outcomes: &mpsc::UnboundedSender<Outcome>,
    turn_id: &str,
    turn_state: TurnState,
    delivery: DeliveryOutcome,
) -> anyhow::Result<()> {
    report(outcomes, delivered(turn_id, turn_id, turn_state), delivery)
}

fn recorded(path: &PathBuf) -> anyhow::Result<Value> {
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

/// Waits until `turn`'s record is `card`. A card is recorded once Discord
/// has answered its creation, after the request a test saw arrive.
async fn recorded_soon(path: &PathBuf, turn: &str, card: &Value) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !recorded(path).is_ok_and(|records| records[turn] == *card) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await?;
    Ok(())
}

/// Cards with configured custom and empty markers.
fn with_markers() -> anyhow::Result<DiscordConfig> {
    let mut configured = config(false);
    configured.activity_markers = toml::from_str(
        r#"
working = "<a:working:123>"
running = "<:tool:456>"
succeeded = "✔"
failed = ""
finishing = "◇"
"#,
    )?;
    Ok(configured)
}

#[tokio::test]
async fn a_slow_turn_gets_one_delayed_card_that_follows_it_and_is_kept_as_its_summary()
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
    // Exercise configured inline presentation through the real outbound requests.
    let configured = with_markers()?;
    let (outcomes, monitor) = discord.monitor(discord.http(false), configured, &records);
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
    assert!(text.starts_with(&format!(
        "<a:working:123> Working · started <t:{accepted}:R>"
    )));
    // What the model wrote cannot mention, format or link anything.
    assert!(
        text.contains(r"> \@everyone\: checking \<\@200\>'s tests"),
        "{text}"
    );
    assert!(
        text.contains("<:tool:456> bash `cargo test -p carillon-discord …` · <t:1790000000:R>")
    );
    assert_eq!(card.body["allowed_mentions"], json!({"parse": []}));
    let expected = json!({"channel_id": "300", "message_id": "901"});
    recorded_soon(&records, "slow", &expected).await?;
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
    assert!(text.lines().any(|line| line == "✔ read `src/lib.rs`"));
    assert!(
        text.lines()
            .any(|line| line == "bash `cargo test -p carillon-discord …`")
    );
    assert!(text.contains("<:tool:456> bash `cargo build`"));
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
    assert!(text.starts_with("◇ Finishing up") && text.contains("read `src/lib.rs`"));
    assert!(!text.contains("cargo build"));

    settle(&outcomes, "slow", completed, delivered)?;
    // The card stays, as the job's summary: what it did, and how long it took.
    let kept = discord
        .wait_for(&Method::PATCH, "/channels/300/messages/901", 3)
        .await?;
    let text = kept[2].content();
    assert!(
        text.starts_with("✔ Done · answered ") && text.contains("after the request"),
        "{text}"
    );
    assert!(text.contains("✔ read `src/lib.rs`") && !text.contains("cargo build"));
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    let seen = discord.seen();
    assert!(
        seen.iter()
            .all(|request| request.at <= kept[2].at && request.method != Method::DELETE),
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
    let (outcomes, monitor) = discord.monitor(discord.http(false), config(true), &records);
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
            .map(Seen::content)
            .unwrap_or_default()
    };
    assert!(text("300").contains("reply may not have been delivered"));
    assert!(text("301").contains("Stopped before finishing"));
    assert!(!text("301").contains("may not have been delivered"));
    assert!(text("302").contains("Stopped before finishing"));
    assert!(text("302").contains("notice about it may not have been delivered"));
    for channel in ["300", "301", "302"] {
        assert!(text(channel).contains("read `Makefile`"));
        assert!(!text(channel).contains("make deploy"));
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
    let (outcomes, monitor) = discord.monitor(discord.http(false), config(true), &records);
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
    // The uncertain creation and the deleted card are remembered without a
    // card to name, so their jobs get no other.
    assert_eq!(
        recorded(&records)?,
        json!({"300": {"channel_id": "300"}, "302": {"channel_id": "302"}})
    );
    settle(
        &outcomes,
        "300",
        TurnState::Completed,
        DeliveryOutcome::Delivered,
    )?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(discord.matching(&Method::DELETE, "/channels").is_empty());
    // Only a settled job's record is forgotten.
    assert_eq!(recorded(&records)?, json!({"302": {"channel_id": "302"}}));
    monitor.abort();
    Ok(())
}

#[tokio::test]
async fn a_restart_goes_on_with_recorded_cards_and_says_what_it_does_not_know() -> anyhow::Result<()>
{
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    std::fs::write(
        &records,
        serde_json::to_vec(&json!({
            "running": {"channel_id": "300", "message_id": "555"},
            "ended": {"channel_id": "301", "message_id": "556"},
            "unnamed": {"channel_id": "302"},
            "answered": {"channel_id": "303", "message_id": "557", "hosted": true},
        }))?,
    )?;
    let discord = Substitute::start(succeed()).await?;
    let long_ago = now() - 60;
    discord.list(vec![running(
        "running",
        "300",
        long_ago,
        Some(tools(None, &[("bash", "sleep 60")], &[])),
    )]);
    let (outcomes, monitor) = discord.monitor(discord.http(false), config(false), &records);
    // A job still running is shown on its card as it was, never on another.
    let live = discord
        .wait_for(&Method::PATCH, "/channels/300/messages/555", 1)
        .await?;
    assert!(live[0].content().contains("↳ bash `sleep 60`"));
    // A job nothing is heard of says so, rather than claiming work.
    let unknown = discord
        .wait_for(&Method::PATCH, "/channels/301/messages/556", 1)
        .await?;
    assert!(
        unknown[0]
            .content()
            .starts_with("⚠️ Carillon restarted while this was in progress")
    );
    let answer = discord
        .wait_for(&Method::PATCH, "/channels/303/messages/557", 1)
        .await?;
    assert!(
        answer[0].body.get("content").is_none(),
        "an answer's text is left alone"
    );
    // Its final report survives the restart, in the delivery that brings it.
    let mut ended = delivered("ended", "ended", TurnState::Completed);
    ended.activity = Some(tools(None, &[], &[("read", "notes.md", true)]));
    report(&outcomes, ended, DeliveryOutcome::Delivered)?;
    let summary = discord
        .wait_for(&Method::PATCH, "/channels/301/messages/556", 2)
        .await?;
    let text = summary[1].content();
    assert!(
        text.starts_with("✓ Done · answered 1m 0s after the request"),
        "{text}"
    );
    assert!(text.contains("✓ read `notes.md`"));
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    // Nothing recorded was removed or replaced, and a card nobody can name
    // holds its job until the job settles.
    let seen = discord.seen();
    assert!(
        seen.iter().all(|request| request.method == Method::PATCH),
        "{seen:#?}"
    );
    assert_eq!(
        recorded(&records)?,
        json!({
            "running": {"channel_id": "300", "message_id": "555"},
            "unnamed": {"channel_id": "302"},
            "answered": {"channel_id": "303", "message_id": "557", "hosted": true},
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
    let (_outcomes, monitor) = discord.monitor(discord.http(false), disabled, &records);
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
    let (_outcomes, monitor) = discord.monitor(discord.http(false), config(false), &records);
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
    assert!(finishing[0].content().contains("Finishing up"));
    assert!(!finishing[0].content().contains("bash `x`"));
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
    let (_outcomes, monitor) = discord.monitor(discord.http(true), config(false), &records);
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
    let (outcomes, monitor) = discord.monitor(http.clone(), config(false), &records);
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
    crate::delivery::send_discord_message(
        &http,
        twilight_model::id::Id::new(300),
        "the answer",
        None,
    )
    .await
    .map_err(|unsent| unsent.failure)?;
    assert!(started.elapsed() < Duration::from_secs(1));

    // Both outcomes are acted on promptly: the waiting edit is dropped
    // unsent, and the other card shows its summary at once.
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
    let other = discord
        .wait_for_text(&Method::PATCH, "/channels/301/", "✓ Done")
        .await?;
    assert!(other.at - settled < Duration::from_secs(1));
    // The blocked card's summary waits for its own bucket; the progress
    // edit that waited there was never sent.
    tokio::time::sleep(Duration::from_millis(1_000)).await;
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
        crate::delivery::send_discord_message(&http, channel, "first answer", None)
            .await
            .map_err(|unsent| unsent.failure)?;
        let card = super::Card {
            channel_id: channel,
            message_id: twilight_model::id::Id::new(901),
            hosted: false,
        };
        let cards = super::Cards {
            http: http.clone(),
            config: config(false),
            records: Arc::new(
                CardRecords::open(directory.path().join("cards.json")).context("records")?,
            ),
            stopping: CancellationToken::new(),
        };
        let progress = super::render::card(
            &super::render::Shown {
                status: super::render::Status::Finishing,
                requested_at: None,
                reports: Vec::new(),
                live: None,
            },
            &cards.config.activity_markers,
        );
        assert_eq!(
            cards.edit(card, &progress, None).await,
            super::Call::Done(())
        );
        let started = Instant::now();
        crate::delivery::send_discord_message(&http, channel, "second answer", None)
            .await
            .map_err(|unsent| unsent.failure)?;
        let waited = started.elapsed();
        if shared {
            assert!(waited >= Duration::from_millis(1_500), "{waited:?}");
        } else {
            assert!(waited < Duration::from_millis(500), "{waited:?}");
        }
    }
    Ok(())
}

#[tokio::test]
async fn a_card_refused_its_edits_is_still_removed_and_a_deleted_one_is_never_replaced()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    let discord = Substitute::start(Arc::new(|seen, _| {
        match (&seen.method, seen.path.as_str()) {
            (&Method::PATCH, path) if path.starts_with("/channels/300/") => Reply::Status(403),
            (&Method::PATCH, _) => Reply::Status(404),
            _ => Reply::Success,
        }
    }))
    .await?;
    let long_ago = now() - 60;
    let job = |channel: &str, detail: &str| {
        let activity = tools(None, &[("bash", detail)], &[]);
        running(channel, channel, long_ago, Some(activity))
    };
    discord.list(vec![job("300", "first"), job("301", "first")]);
    let _provider = discord.provider(discord.http(false), config(false), &records);
    discord
        .wait_for(&Method::POST, "/channels/300/messages", 1)
        .await?;
    discord
        .wait_for(&Method::POST, "/channels/301/messages", 1)
        .await?;
    discord.list(vec![job("300", "second"), job("301", "second")]);
    discord
        .wait_for(&Method::PATCH, "/channels/300/", 1)
        .await?;
    discord
        .wait_for(&Method::PATCH, "/channels/301/", 1)
        .await?;
    // The deleted card's turn ends; core lists nothing of its job meanwhile.
    discord.list(vec![job("300", "third")]);
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    // Refused edits are not retried.
    assert_eq!(discord.matching(&Method::PATCH, "/channels").len(), 2);
    let answer = |turn: &str, origin: &str, continued_by: Option<&str>| {
        let mut answer = delivered(turn, origin, TurnState::Completed);
        answer.target_id = origin.to_owned();
        answer.activity = Some(tools(None, &[], &[("bash", "make", true)]));
        answer.continued_by = continued_by.map(str::to_owned);
        answer
    };
    // Its answer shows work, and an observation goes on with the job.
    discord.deliver(answer("301", "301", Some("report")));
    discord
        .wait_for(&Method::POST, "/channels/301/messages", 2)
        .await?;
    let reporting = tools(Some("Reporting."), &[("read", "notes.md")], &[]);
    discord.list(vec![
        job("300", "third"),
        continuing("report", "301", "301", long_ago, Some(reporting)),
    ]);
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    discord.deliver(answer("300", "300", None));
    discord.deliver(answer("report", "301", None));
    discord.list(Vec::new());
    // A refused edit does not show that removal is refused too.
    discord
        .wait_for(&Method::DELETE, "/channels/300/messages/", 1)
        .await?;
    discord
        .wait_for(&Method::POST, "/channels/301/messages", 3)
        .await?;
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    // A card Discord no longer has is neither removed nor replaced: not by
    // the observation that goes on with its job, nor on the job's answers.
    assert!(
        discord
            .matching(&Method::DELETE, "/channels/301/")
            .is_empty()
    );
    assert_eq!(discord.matching(&Method::PATCH, "/channels").len(), 2);
    let created = discord.matching(&Method::POST, "/channels/30");
    let cards = created
        .iter()
        .filter(|seen| seen.body["embeds"].get(0).is_some())
        .count();
    assert_eq!((created.len(), cards), (5, 2), "{created:#?}");
    assert_eq!(recorded(&records)?, json!({}));
    Ok(())
}

#[tokio::test]
async fn a_quick_answers_summary_stays_its_jobs_one_card_while_core_records_it_slowly()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    let discord = Substitute::start(succeed()).await?;
    // Core lists the turn as it starts, and it answers before its card would appear.
    let accepted_at = now();
    let activity = tools(None, &[("bash", "make")], &[]);
    discord.list(vec![running("quick", "300", accepted_at, Some(activity))]);
    let mut answer = delivered("quick", "quick", TurnState::Completed);
    answer.origin.accepted_at = accepted_at;
    answer.activity = Some(tools(None, &[], &[("bash", "make", true)]));
    discord.deliver(answer);
    // Core takes longer to record how the answer went than the card takes to appear.
    let recording = discord.hold_recording().await;
    let started = Instant::now();
    let _provider = discord.provider(discord.http(false), config(false), &records);
    // The answer goes out at once, carrying its job's summary.
    let sent = discord
        .wait_for(&Method::POST, "/channels/300/messages", 1)
        .await?;
    assert!(sent[0].at.duration_since(started) < Duration::from_secs(1));
    assert_eq!(sent[0].body["content"], "answer");
    assert!(
        sent[0].content().starts_with("✓ Done"),
        "{:?}",
        sent[0].body
    );
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    assert_eq!(
        discord.matching(&Method::POST, "/channels").len(),
        1,
        "{:#?}",
        discord.seen()
    );
    discord.list(Vec::new());
    drop(recording);
    // Once recorded, the answer is finished as the job's one card.
    let finished = discord
        .wait_for(&Method::PATCH, "/channels/300/messages/901", 1)
        .await?;
    assert!(finished[0].content().starts_with("✓ Done"));
    assert!(finished[0].body.get("content").is_none());
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    assert_eq!(discord.matching(&Method::POST, "/channels").len(), 1);
    assert!(discord.matching(&Method::DELETE, "/channels").is_empty());
    // A job whose answer ended it never needed a record.
    assert!(!records.exists());
    Ok(())
}

#[tokio::test]
async fn a_stopping_provider_finishes_the_card_its_last_answer_settles() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    // Discord is slow to finish the answered job's card.
    let discord = Substitute::start(Arc::new(|seen, _| {
        if seen.method == Method::PATCH && seen.path.starts_with("/channels/300/") {
            Reply::Delayed(Duration::from_millis(1_500))
        } else {
            Reply::Success
        }
    }))
    .await?;
    let long_ago = now() - 60;
    let job = |channel: &str, detail: &str| {
        let activity = tools(None, &[("bash", detail)], &[]);
        running(channel, channel, long_ago, Some(activity))
    };
    // Two jobs have cards: one is answered as the provider stops, one goes on.
    discord.list(vec![job("300", "make"), job("301", "make")]);
    let mut provider = discord.provider(discord.http(false), config(false), &records);
    for channel in ["300", "301"] {
        let created = format!("/channels/{channel}/messages");
        discord.wait_for(&Method::POST, &created, 1).await?;
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        while !recorded(&records)
            .is_ok_and(|cards| cards.as_object().is_some_and(|cards| cards.len() == 2))
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await?;
    // Core is slow to record the answer, which goes out plainly: its job has a card.
    let recording = discord.hold_recording().await;
    let mut answer = delivered("300", "300", TurnState::Completed);
    let report = [("bash", "make", true), ("read", "notes.md", true)];
    answer.activity = Some(tools(None, &[], &report));
    discord.deliver(answer);
    let sent = discord
        .wait_for(&Method::POST, "/channels/300/messages", 2)
        .await?;
    assert!(sent[1].body.get("embeds").is_none());
    // The provider stops; what core lists from now on is not shown.
    provider.stopping.cancel();
    discord.list(vec![job("301", "make check"), job("302", "make")]);
    let started = Instant::now();
    let (stopped, ()) = tokio::join!(
        tokio::time::timeout(Duration::from_secs(20), provider.stop()),
        async {
            tokio::time::sleep(Duration::from_millis(3_500)).await;
            drop(recording);
        }
    );
    stopped??;
    assert!(started.elapsed() < Duration::from_secs(10));
    // Before the monitor returned, the answered job's card showed its
    // summary, and its record went with it.
    let finished = discord.matching(&Method::PATCH, "/channels/300/messages/");
    let summary = finished.last().map(Seen::content).unwrap_or_default();
    assert!(summary.starts_with("✓ Done · answered"), "{summary}");
    assert!(summary.contains("✓ read `notes.md`"), "{summary}");
    assert!(discord.matching(&Method::POST, "/channels/302/").is_empty());
    assert!(
        discord
            .matching(&Method::PATCH, "/channels/301/")
            .is_empty()
    );
    assert!(discord.matching(&Method::DELETE, "/channels").is_empty());
    // The job that goes on keeps its card for the next start.
    let left = recorded(&records)?;
    assert!(left.get("300").is_none(), "{left}");
    assert!(left["301"]["message_id"].is_string(), "{left}");
    Ok(())
}

#[tokio::test]
async fn a_stopping_provider_makes_no_new_card_but_takes_an_answer_card() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let discord = Substitute::start(succeed()).await?;
    let stopping = CancellationToken::new();
    stopping.cancel();
    let cards = super::Cards {
        http: discord.http(false),
        config: config(false),
        records: Arc::new(
            CardRecords::open(directory.path().join("cards.json")).context("records")?,
        ),
        stopping,
    };
    // A job read before the stop, long past its card's appearance.
    let activity = tools(None, &[("bash", "make")], &[]);
    let turn = running("job", "300", now() - 60, Some(activity));
    let (view, mut watched) = tokio::sync::watch::channel(super::Job {
        state: super::State::Active(Box::new(turn)),
        ..super::Job::default()
    });
    let obtaining = tokio::spawn(async move { super::obtain(&cards, "job", &mut watched).await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(discord.seen().is_empty(), "{:#?}", discord.seen());
    // Its outcome still brings the answer that carries its summary.
    let answer = super::Card {
        channel_id: twilight_model::id::Id::new(300),
        message_id: twilight_model::id::Id::new(777),
        hosted: true,
    };
    view.send_modify(|job| job.hosted = Some(answer));
    let obtained = tokio::time::timeout(Duration::from_secs(5), obtaining).await??;
    assert_eq!(obtained, Some(answer));
    assert!(discord.seen().is_empty());
    Ok(())
}

#[tokio::test]
async fn an_answers_summary_that_may_be_in_discord_keeps_its_job_from_another_card()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    // Discord takes the answer carrying channel 300's summary, but its reply
    // is lost; it refuses the one in channel 301.
    let discord = Substitute::start(Arc::new(|seen, _| {
        let summarized = seen.method == Method::POST
            && seen.body.get("content").is_some()
            && seen.body["embeds"].get(0).is_some();
        match seen.path.as_str() {
            "/channels/300/messages" if summarized => Reply::Lost,
            "/channels/301/messages" if summarized => Reply::Status(400),
            _ => Reply::Success,
        }
    }))
    .await?;
    let long_ago = now() - 60;
    let _provider = discord.provider(discord.http(false), config(false), &records);
    // Two parents answer while the work they delegated goes on.
    let answer = |turn: &str, origin: &str, activity, continued_by: Option<&str>| {
        let mut answer = delivered(turn, origin, TurnState::Completed);
        answer.target_id = origin.to_owned();
        answer.activity = Some(activity);
        answer.continued_by = continued_by.map(str::to_owned);
        answer
    };
    for channel in ["300", "301"] {
        let made = delegating(
            tools(None, &[], &[("bash", "make", true)]),
            WorkState::Running,
        );
        discord.deliver(answer(channel, channel, made, Some("report")));
    }
    discord
        .wait_for(&Method::POST, "/channels/301/messages", 1)
        .await?;
    // The summary that may be there is remembered, without a card to name.
    recorded_soon(&records, "300", &json!({"channel_id": "300"})).await?;
    // The observations run long enough for a card.
    let report = |channel: &str| {
        let reporting = delegating(tools(Some("Reporting."), &[], &[]), WorkState::Succeeded);
        continuing(
            &format!("{channel}-report"),
            channel,
            channel,
            long_ago,
            Some(reporting),
        )
    };
    discord.list(vec![report("300"), report("301")]);
    // A refused answer proves nothing is there: its job gets a card.
    discord
        .wait_for_text(&Method::POST, "/channels/301/messages", "> Reporting.")
        .await?;
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    // One that may be there stands for its job's card: none is made.
    assert_eq!(
        discord
            .matching(&Method::POST, "/channels/300/messages")
            .len(),
        1
    );
    for channel in ["300", "301"] {
        let read = tools(None, &[], &[("read", "notes.md", true)]);
        discord.deliver(answer(&format!("{channel}-report"), channel, read, None));
    }
    discord.list(Vec::new());
    // The job's last answer still goes out, plainly; nothing names a guessed card.
    let answers = discord
        .wait_for(&Method::POST, "/channels/300/messages", 2)
        .await?;
    assert_eq!(answers[1].body["content"], "answer");
    assert!(answers[1].body.get("embeds").is_none());
    discord
        .wait_for_text(&Method::PATCH, "/channels/301/messages/", "✓ Done")
        .await?;
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    assert!(
        discord
            .matching(&Method::PATCH, "/channels/300/")
            .is_empty()
    );
    assert_eq!(
        discord
            .matching(&Method::POST, "/channels/300/messages")
            .len(),
        2
    );
    assert_eq!(recorded(&records)?, json!({}));
    Ok(())
}

#[tokio::test]
async fn a_finishing_card_that_failed_to_update_is_retried_until_it_stops_showing_work()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    let discord = Substitute::start(Arc::new(|seen, earlier| {
        if seen.method == Method::PATCH && earlier == 0 {
            Reply::Status(500)
        } else {
            Reply::Success
        }
    }))
    .await?;
    let activity = tools(None, &[("bash", "cargo test")], &[("read", "a.rs", true)]);
    discord.list(vec![running("turn", "300", now() - 60, Some(activity))]);
    let (outcomes, monitor) = discord.monitor(discord.http(false), config(false), &records);
    discord
        .wait_for(&Method::POST, "/channels/300/messages", 1)
        .await?;
    // Core reads keep succeeding with the turn absent; its outcome is withheld.
    discord.list(Vec::new());
    let edits = discord
        .wait_for(&Method::PATCH, "/channels/300/messages/901", 2)
        .await?;
    assert!(edits[1].at - edits[0].at >= Duration::from_millis(2_900));
    for edit in &edits {
        assert!(edit.content().contains("Finishing up"));
        assert!(edit.content().contains("read `a.rs`"));
        assert!(!edit.content().contains("cargo test"));
    }
    // Once shown, the same text is not sent again.
    tokio::time::sleep(Duration::from_millis(4_000)).await;
    assert_eq!(discord.matching(&Method::PATCH, "/channels").len(), 2);
    settle(
        &outcomes,
        "turn",
        TurnState::Completed,
        DeliveryOutcome::Delivered,
    )?;
    let kept = discord
        .wait_for(&Method::PATCH, "/channels/300/messages/901", 3)
        .await?;
    assert!(kept[2].content().starts_with("✓ Done"));
    assert!(discord.matching(&Method::DELETE, "/channels").is_empty());
    monitor.abort();
    Ok(())
}

#[tokio::test]
async fn an_uncertain_card_is_not_created_again_after_an_orderly_restart() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    // The card is created, but the provider never learns its identity.
    let discord = Substitute::start(Arc::new(|seen, _| {
        if seen.method == Method::POST && seen.path.ends_with("/messages") {
            Reply::Lost
        } else {
            Reply::Success
        }
    }))
    .await?;
    let turn = || {
        let activity = tools(None, &[("bash", "sleep 600")], &[]);
        vec![running("turn", "300", now() - 60, Some(activity))]
    };
    discord.list(turn());
    let (_outcomes, monitor) = discord.monitor(discord.http(false), config(false), &records);
    discord
        .wait_for(&Method::POST, "/channels/300/messages", 1)
        .await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    monitor.abort();
    let _ = monitor.await;

    // The same turn is still running when the provider starts again.
    let (outcomes, monitor) = discord.monitor(discord.http(false), config(false), &records);
    tokio::time::sleep(Duration::from_millis(4_000)).await;
    assert_eq!(
        discord
            .matching(&Method::POST, "/channels/300/messages")
            .len(),
        1
    );
    assert!(discord.matching(&Method::DELETE, "/channels").is_empty());
    // A turn that ended still holds its job until the job settles.
    discord.list(Vec::new());
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    assert_eq!(recorded(&records)?, json!({"turn": {"channel_id": "300"}}));
    // Once it has, the unnamed card no longer holds anything back.
    settle(
        &outcomes,
        "turn",
        TurnState::Completed,
        DeliveryOutcome::Delivered,
    )?;
    tokio::time::timeout(Duration::from_secs(10), async {
        while recorded(&records).is_ok_and(|records| records != json!({})) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await?;
    monitor.abort();
    Ok(())
}

#[tokio::test]
async fn unreadable_card_records_turn_cards_off_but_keep_typing_and_the_evidence()
-> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let directory = tempfile::tempdir()?;
    let occupied = directory.path().join("occupied.json");
    std::fs::create_dir(&occupied)?;
    let malformed = directory.path().join("malformed.json");
    std::fs::write(&malformed, "{not json")?;
    let unreadable = directory.path().join("unreadable.json");
    std::fs::write(
        &unreadable,
        r#"{"turn": {"channel_id": "300", "message_id": "555"}}"#,
    )?;
    std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000))?;
    let mut faults = vec![occupied.clone(), malformed.clone()];
    // The owner of the file can read it regardless when running as root.
    if std::fs::read(&unreadable).is_err() {
        faults.push(unreadable.clone());
    }
    for records in faults {
        assert!(
            CardRecords::open(records.clone()).is_none(),
            "{}",
            records.display()
        );
        let discord = Substitute::start(succeed()).await?;
        let activity = tools(None, &[("bash", "sleep 600")], &[]);
        discord.list(vec![running("turn", "300", now() - 60, Some(activity))]);
        let (_outcomes, monitor) = discord.monitor(discord.http(false), config(true), &records);
        discord
            .wait_for(&Method::POST, "/channels/300/typing", 1)
            .await?;
        tokio::time::sleep(Duration::from_millis(3_500)).await;
        // No card, no attempt to remove one nobody can name, and typing continues.
        let seen = discord.seen();
        assert!(
            seen.iter().all(|seen| seen.path == "/channels/300/typing"),
            "{seen:#?}"
        );
        monitor.abort();
    }
    assert!(occupied.is_dir());
    assert_eq!(std::fs::read(&malformed)?, b"{not json");
    std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o600))?;
    assert_eq!(
        std::fs::read_to_string(&unreadable)?,
        r#"{"turn": {"channel_id": "300", "message_id": "555"}}"#
    );
    Ok(())
}

#[tokio::test]
async fn a_job_keeps_one_card_while_its_answer_arrives_and_its_follow_up_reports()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    let discord = Substitute::start(succeed()).await?;
    let long_ago = now() - 60;
    let working = delegating(tools(None, &[("bash", "make")], &[]), WorkState::Running);
    discord.list(vec![running("parent", "300", long_ago, Some(working))]);
    let (outcomes, monitor) = discord.monitor(discord.http(false), config(false), &records);
    discord
        .wait_for(&Method::POST, "/channels/300/messages", 1)
        .await?;
    // The parent answers while its child still runs; core queued the report.
    let mut answered = delivered("parent", "parent", TurnState::Completed);
    let made = tools(None, &[], &[("bash", "make", true)]);
    answered.activity = Some(delegating(made, WorkState::Running));
    answered.continued_by = Some("report".to_owned());
    report(&outcomes, answered, DeliveryOutcome::Delivered)?;
    discord.list(Vec::new());
    // The answer is not the job's end: the card goes on, showing the child.
    let going = discord
        .wait_for_text(
            &Method::PATCH,
            "/channels/300/messages/901",
            "Answered · delegated work goes on",
        )
        .await?;
    assert!(
        going
            .content()
            .contains("↳ delegation `research` · running")
    );
    assert!(going.content().contains("✓ bash `make`"));
    // The follow-up reports the child's end on the same card.
    let reporting = delegating(tools(Some("Reporting."), &[], &[]), WorkState::Succeeded);
    discord.list(vec![continuing(
        "report",
        "parent",
        "300",
        now(),
        Some(reporting),
    )]);
    let child = discord
        .wait_for_text(
            &Method::PATCH,
            "/channels/300/messages/901",
            "✓ delegation `research` · done · 1m 1s",
        )
        .await?;
    assert!(
        child
            .content()
            .starts_with("⏳ Working · Waiting for the model · started ")
    );
    let mut reported = delivered("report", "parent", TurnState::Completed);
    reported.activity = Some(delegating(
        tools(None, &[], &[("read", "findings.md", true)]),
        WorkState::Succeeded,
    ));
    report(&outcomes, reported, DeliveryOutcome::Delivered)?;
    discord.list(Vec::new());
    let done = discord
        .wait_for_text(
            &Method::PATCH,
            "/channels/300/messages/901",
            "✓ Done · answered",
        )
        .await?;
    // The summary keeps the whole job: the parent's call, the follow-up's, the child.
    for part in [
        "✓ bash `make`",
        "✓ read `findings.md`",
        "✓ delegation `research` · done",
    ] {
        assert!(done.content().contains(part), "{}", done.content());
    }
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    assert_eq!(discord.matching(&Method::POST, "/channels").len(), 1);
    assert!(discord.matching(&Method::DELETE, "/channels").is_empty());
    assert_eq!(recorded(&records)?, json!({}));
    monitor.abort();
    Ok(())
}

#[tokio::test]
async fn a_late_answer_or_stale_read_never_takes_a_job_back() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    let discord = Substitute::start(succeed()).await?;
    let long_ago = now() - 60;
    discord.list(vec![running(
        "parent",
        "300",
        long_ago,
        Some(delegating(
            tools(None, &[("bash", "make")], &[]),
            WorkState::Running,
        )),
    )]);
    let (outcomes, monitor) = discord.monitor(discord.http(false), config(false), &records);
    discord
        .wait_for(&Method::POST, "/channels/300/messages", 1)
        .await?;
    // Core already runs the follow-up before the parent's answer is delivered.
    let follow_up = || {
        continuing(
            "report",
            "parent",
            "300",
            long_ago,
            Some(delegating(
                tools(Some("Reporting."), &[], &[]),
                WorkState::Succeeded,
            )),
        )
    };
    discord.list(vec![follow_up()]);
    discord
        .wait_for_text(&Method::PATCH, "/channels/300/messages/901", "> Reporting.")
        .await?;
    // Core cannot be read meanwhile: only the job itself decides what shows.
    *lock(&discord.active) = None;
    let mut late = delivered("parent", "parent", TurnState::Completed);
    late.activity = Some(delegating(tools(None, &[], &[]), WorkState::Running));
    late.continued_by = Some("report".to_owned());
    report(&outcomes, late, DeliveryOutcome::Delivered)?;
    tokio::time::sleep(Duration::from_millis(4_000)).await;
    // The late answer neither ends the job nor takes back what the follow-up shows.
    for edit in discord.matching(&Method::PATCH, "/channels/300/messages/901") {
        let text = edit.content();
        assert!(
            !text.contains("Answered") && !text.contains("Done"),
            "{text}"
        );
        assert!(!text.contains("delegation `research` · running"), "{text}");
    }
    report(
        &outcomes,
        delivered("report", "parent", TurnState::Completed),
        DeliveryOutcome::Delivered,
    )?;
    let done = discord
        .wait_for_text(&Method::PATCH, "/channels/300/messages/901", "✓ Done")
        .await?;
    // A read taken before the follow-up's delivery cannot revive it.
    discord.list(vec![follow_up()]);
    tokio::time::sleep(Duration::from_millis(4_000)).await;
    let edits = discord.matching(&Method::PATCH, "/channels/300/messages/901");
    assert_eq!(edits.last().map(Seen::content), Some(done.content()));
    assert_eq!(
        discord
            .matching(&Method::POST, "/channels/300/messages")
            .len(),
        1
    );
    monitor.abort();
    Ok(())
}
