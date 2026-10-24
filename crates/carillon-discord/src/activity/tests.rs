//! The monitor against a local substitute serving what core recorded of
//! jobs, as core's job reads do (pages of jobs at work and jobs read by
//! name), core's deliveries, and Discord's API through a real Twilight
//! client, beside the delivery loop where a case needs it. Cases marked as
//! using the rate limiter run Twilight's own limiter, permit queue and 429
//! retries.
use super::{CardRecords, Delivering, activity_monitor};
use crate::{
    client::{CoreClient, test_client as core_client},
    config::{DiscordActivation, DiscordConfig, DiscordConversation},
    conversations::tests::discord_config,
    delivery::{Reactions, delivery_loop},
    module::{Module, tests::release_module},
};
use anyhow::Context as _;
use axum::{
    Json, Router,
    body::Bytes,
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri},
    response::{IntoResponse as _, Response},
};
use carillon_protocol::model::{
    ActivityPhase, AgentInfo, Delivery, DeliveryAck, DeliveryOutcome, DeliveryState, FinishedTool,
    Job, JobPage, JobTurn, Origin, RunningTool, TurnActivity, TurnState, WorkItem, WorkKind,
    WorkState, WorkSummary,
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{OwnedMutexGuard, mpsc},
    task::JoinHandle,
    time::Instant,
};
use tokio_util::sync::CancellationToken;
use twilight_model::id::Id;

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

    fn is_card(&self) -> bool {
        self.body["embeds"].get(0).is_some()
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
    /// Success, answered this much later.
    Delayed(Duration),
}

type Script = Arc<dyn Fn(&Seen, usize) -> Reply + Send + Sync>;

/// What core recorded, as the substitute serves it.
#[derive(Default)]
struct Recorded {
    /// The jobs, oldest first; `None` answers as an unavailable core.
    jobs: Option<Vec<Job>>,
    /// What core hands the delivery loop next, in order.
    deliveries: VecDeque<Delivery>,
    /// The turn each handed delivery belongs to.
    claimed: HashMap<i64, String>,
    /// The names of each read by name.
    named: Vec<Vec<String>>,
}

struct Substitute {
    address: std::net::SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
    core: Arc<Mutex<Recorded>>,
    /// Core records no delivery while a case holds this.
    recording: Arc<tokio::sync::Mutex<()>>,
    /// Reads of core begun so far.
    reads: Arc<AtomicUsize>,
    /// Core answers no read, with the jobs it took when asked, while a case
    /// holds this.
    reading: Arc<tokio::sync::Mutex<()>>,
    /// The provider's client for this core.
    client: CoreClient,
    /// The release module, which draws the cards.
    module: Arc<Module>,
    server: JoinHandle<()>,
}

/// Jobs a page of jobs at work lists: fewer than core's, so a few jobs
/// already take several pages.
const PAGE: usize = 2;

impl Substitute {
    async fn start(script: Script) -> anyhow::Result<Self> {
        let seen = Arc::new(Mutex::new(Vec::<Seen>::new()));
        let core = Arc::new(Mutex::new(Recorded {
            jobs: Some(Vec::new()),
            ..Recorded::default()
        }));
        let recording = Arc::new(tokio::sync::Mutex::new(()));
        let reads = Arc::new(AtomicUsize::new(0));
        let reading = Arc::new(tokio::sync::Mutex::new(()));
        let (recorded, held) = (core.clone(), recording.clone());
        let (begun, answering, observed) = (reads.clone(), reading.clone(), seen.clone());
        let app = Router::new().fallback(move |method: Method, uri: Uri, body: Bytes| {
            let (core, recording, script) = (recorded.clone(), held.clone(), script.clone());
            let (reads, reading, seen) = (begun.clone(), answering.clone(), observed.clone());
            async move {
                if uri.path() == "/v1/jobs" {
                    // Core takes its record when asked, then answers.
                    let page = read_jobs(&core, uri.query().unwrap_or_default());
                    reads.fetch_add(1, Ordering::SeqCst);
                    let _answered = reading.lock().await;
                    return match page {
                        Some(page) => Json(page).into_response(),
                        None => StatusCode::SERVICE_UNAVAILABLE.into_response(),
                    };
                }
                if uri.path() == "/v1/deliveries" {
                    let mut core = lock(&core);
                    let delivery = core.deliveries.pop_front();
                    if let Some(delivery) = &delivery {
                        core.claimed.insert(delivery.id, delivery.turn_id.clone());
                    }
                    return Json(delivery).into_response();
                }
                if let Some(id) = uri
                    .path()
                    .strip_prefix("/v1/deliveries/")
                    .and_then(|path| path.strip_suffix("/ack"))
                {
                    let _recorded = recording.lock().await;
                    let ack: DeliveryAck = serde_json::from_slice(&body).unwrap_or(DeliveryAck {
                        native_ref: None,
                        outcome: DeliveryOutcome::Uncertain,
                    });
                    acknowledge(&core, id.parse().unwrap_or_default(), ack.outcome);
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
                respond(&request, reply, creates)
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
            core,
            recording,
            reads,
            reading,
            client: core_client(format!("http://{address}"))?,
            module: release_module().await?,
            server,
        })
    }

    /// Core's record of every job from now on.
    fn record(&self, jobs: Vec<Job>) {
        lock(&self.core).jobs = Some(jobs);
    }

    /// Core answers no read until it records jobs again.
    fn unavailable(&self) {
        lock(&self.core).jobs = None;
    }

    /// Core hands `delivery` to the delivery loop's next claim.
    fn deliver(&self, delivery: Delivery) {
        lock(&self.core).deliveries.push_back(delivery);
    }

    /// The names each read by name was given.
    fn named(&self) -> Vec<Vec<String>> {
        lock(&self.core).named.clone()
    }

    /// Holds every acknowledgement until the guard is dropped, as a core
    /// slow to record deliveries would.
    async fn hold_recording(&self) -> OwnedMutexGuard<()> {
        self.recording.clone().lock_owned().await
    }

    /// Holds every read's answer until the guard is dropped, as a core slow
    /// to describe the jobs it read would.
    async fn hold_reads(&self) -> OwnedMutexGuard<()> {
        self.reading.clone().lock_owned().await
    }

    /// Waits until a read begun after the first `count` has taken core's record.
    async fn read_taken(&self, count: usize) -> anyhow::Result<()> {
        tokio::time::timeout(Duration::from_secs(10), async {
            while self.reads.load(Ordering::SeqCst) <= count {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .context("no read of core began")
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
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

    /// The cards made in `channel`.
    fn cards_in(&self, channel: &str) -> Vec<Seen> {
        self.matching(&Method::POST, &format!("/channels/{channel}/messages"))
            .into_iter()
            .filter(Seen::is_card)
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

    /// The monitor alone; the case says what the delivery loop would.
    fn monitor(
        &self,
        http: Arc<twilight_http::Client>,
        config: DiscordConfig,
        records: &Path,
    ) -> (mpsc::UnboundedSender<Delivering>, JoinHandle<()>) {
        self.serving(http, crate::config::Settings::new(config), records)
    }

    /// The monitor alone, under settings the case may change as it runs.
    fn serving(
        &self,
        http: Arc<twilight_http::Client>,
        settings: crate::config::Settings,
        records: &Path,
    ) -> (mpsc::UnboundedSender<Delivering>, JoinHandle<()>) {
        let (delivering, heard) = mpsc::unbounded_channel();
        let monitor = activity_monitor(
            self.client.clone(),
            self.module.clone(),
            Reactions::new(http, settings),
            CardRecords::open(records.to_path_buf()),
            heard,
            CancellationToken::new(),
        );
        (delivering, tokio::spawn(monitor))
    }

    /// The monitor beside the delivery loop, sharing the stop as the
    /// provider does. Deliveries add no reactions.
    fn provider(
        &self,
        http: Arc<twilight_http::Client>,
        mut config: DiscordConfig,
        records: &Path,
    ) -> Provider {
        config.reactions = false;
        let core = self.client.clone();
        let (delivering, heard) = mpsc::unbounded_channel();
        let stopping = CancellationToken::new();
        let reactions = Reactions::new(http.clone(), crate::config::Settings::new(config));
        let delivered = delivery_loop(
            core.clone(),
            reactions.clone(),
            http,
            delivering,
            stopping.clone(),
        );
        let records = CardRecords::open(records.to_path_buf());
        let monitor = activity_monitor(
            core,
            self.module.clone(),
            reactions,
            records,
            heard,
            stopping.clone(),
        );
        Provider {
            monitor: tokio::spawn(monitor),
            delivering: tokio::spawn(delivered),
            stopping,
        }
    }
}

/// Serves a read of jobs as core does: those named in `tracked`, or a page
/// of those at work after the one `after` names.
fn read_jobs(core: &Mutex<Recorded>, query: &str) -> Option<JobPage> {
    let mut core = lock(core);
    let jobs = core.jobs.clone()?;
    let parameter = |name: &str| {
        query
            .split('&')
            .find_map(|pair| pair.strip_prefix(&format!("{name}=")))
            .map(|value| value.replace("%2C", ","))
    };
    if let Some(tracked) = parameter("tracked") {
        let named = tracked.split(',').map(str::to_owned).collect::<Vec<_>>();
        core.named.push(named.clone());
        let jobs = jobs
            .into_iter()
            .filter(|job| named.contains(&job.origin.turn_id))
            .collect();
        return Some(JobPage { jobs, next: None });
    }
    let at_work = jobs
        .into_iter()
        .filter(|job| {
            job.turns
                .last()
                .is_some_and(|turn| matches!(turn.state, TurnState::Queued | TurnState::Running))
        })
        .collect::<Vec<_>>();
    let start = match parameter("after") {
        Some(after) => at_work
            .iter()
            .position(|job| job.origin.turn_id == after)
            .map_or(at_work.len(), |index| index + 1),
        None => 0,
    };
    let page = at_work.into_iter().skip(start).collect::<Vec<_>>();
    let next = (page.len() > PAGE).then(|| page[PAGE - 1].origin.turn_id.clone());
    Some(JobPage {
        jobs: page.into_iter().take(PAGE).collect(),
        next,
    })
}

/// Records how a handed delivery went on its turn, as core does.
fn acknowledge(core: &Mutex<Recorded>, id: i64, outcome: DeliveryOutcome) {
    let mut core = lock(core);
    let Some(turn_id) = core.claimed.get(&id).cloned() else {
        return;
    };
    for job in core.jobs.iter_mut().flatten() {
        for turn in &mut job.turns {
            if turn.turn_id == turn_id {
                turn.delivery = Some(outcome.into());
            }
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
    /// then the monitor that hears of them.
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

fn respond(request: &Seen, reply: Reply, creates: usize) -> Response {
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

/// A turn of a job accepted at `accepted_at`: an ended one finished a minute
/// later with its answer not yet sent.
fn turn(
    turn_id: &str,
    state: TurnState,
    accepted_at: i64,
    activity: Option<TurnActivity>,
) -> JobTurn {
    let ended = matches!(state, TurnState::Completed | TurnState::Interrupted);
    JobTurn {
        turn_id: turn_id.to_owned(),
        state,
        accepted_at,
        finished_at: ended.then_some(accepted_at + 60),
        activity,
        delivery: ended.then_some(DeliveryState::Pending),
    }
}

/// The job `origin` began in `channel`, with its turns so far.
fn job(origin: &str, channel: &str, accepted_at: i64, turns: Vec<JobTurn>) -> Job {
    Job {
        origin: Origin {
            turn_id: origin.to_owned(),
            accepted_at,
        },
        target_id: channel.to_owned(),
        feedback_target_id: None,
        turns,
        earlier: false,
    }
}

fn running(origin: &str, channel: &str, accepted_at: i64, activity: Option<TurnActivity>) -> Job {
    job(
        origin,
        channel,
        accepted_at,
        vec![turn(origin, TurnState::Running, accepted_at, activity)],
    )
}

/// A job whose message's turn ended as `state`, reporting `report`, and
/// whose answer went as `delivery` says, when it was sent.
fn ended(
    origin: &str,
    channel: &str,
    accepted_at: i64,
    state: TurnState,
    report: Option<TurnActivity>,
    delivery: Option<DeliveryState>,
) -> Job {
    let mut ended = turn(origin, state, accepted_at, report);
    ended.delivery = delivery.or(ended.delivery);
    job(origin, channel, accepted_at, vec![ended])
}

/// `job` with its last answer recorded as sent.
fn sent(mut job: Job) -> Job {
    if let Some(last) = job.turns.last_mut() {
        last.delivery = Some(DeliveryState::Delivered);
    }
    job
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
        controls: Vec::new(),
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

/// The answer to a turn of the job `origin`, in channel 300.
fn answer(id: i64, turn_id: &str, origin: &str, turn_state: TurnState) -> Delivery {
    Delivery {
        turn_state,
        id,
        turn_id: turn_id.to_owned(),
        target_id: "300".to_owned(),
        feedback_target_id: "300:400".to_owned(),
        content: "answer".to_owned(),
        silence: None,
        origin: Origin {
            turn_id: origin.to_owned(),
            accepted_at: now() - 60,
        },
    }
}

fn recorded(path: &PathBuf) -> anyhow::Result<Value> {
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

/// Waits until the records are `expected`. A card is recorded once Discord
/// has answered its creation, after the request a test saw arrive.
async fn recorded_soon(path: &PathBuf, expected: &Value) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !recorded(path).is_ok_and(|records| records == *expected) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .with_context(|| format!("records stayed {:?}", recorded(path).ok()))
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
async fn a_slow_job_gets_one_delayed_card_that_follows_it_and_keeps_its_summary()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    let discord = Substitute::start(succeed()).await?;
    let accepted = now();
    let testing = || {
        let command = ("bash", "cargo test -p carillon-discord …");
        tools(Some("@everyone: checking <@200>'s tests"), &[command], &[])
    };
    discord.record(vec![
        running("slow", "300", accepted, Some(testing())),
        running("fast", "301", accepted, None),
    ]);
    let started = Instant::now();
    // Exercise configured inline presentation through the real outbound requests.
    let (delivering, monitor) = discord.monitor(discord.http(false), with_markers()?, &records);
    // A job that ends before the appearance delay never shows a card: its
    // answer is taken up and recorded at once.
    tokio::time::sleep(Duration::from_millis(500)).await;
    delivering.send(Delivering::Claimed("fast".to_owned()))?;
    let fast = ended("fast", "301", accepted, TurnState::Completed, None, None);
    discord.record(vec![
        running("slow", "300", accepted, Some(testing())),
        sent(fast),
    ]);
    delivering.send(Delivering::Recorded)?;

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
    recorded_soon(
        &records,
        &json!({"slow": {"channel_id": "300", "message_id": "901"}}),
    )
    .await?;

    let history = [
        ("read", "src/lib.rs", true),
        ("bash", "cargo test -p carillon-discord …", false),
    ];
    let later = tools(None, &[("bash", "cargo build")], &history);
    discord.record(vec![running("slow", "300", accepted, Some(later))]);
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
    // An unchanged job is not edited again.
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    assert_eq!(discord.matching(&Method::PATCH, "/channels").len(), 1);

    // The turn ended without a report, its answer not yet sent: nothing is
    // shown as still running, and what the card saw stays.
    let finishing = ended("slow", "300", accepted, TurnState::Completed, None, None);
    discord.record(vec![finishing.clone()]);
    let edits = discord
        .wait_for(&Method::PATCH, "/channels/300/messages/901", 2)
        .await?;
    let text = edits[1].content();
    assert!(text.starts_with("◇ Finishing up") && text.contains("read `src/lib.rs`"));
    assert!(!text.contains("cargo build"));

    // Its answer sent, the card stays as the job's summary.
    discord.record(vec![sent(finishing)]);
    delivering.send(Delivering::Recorded)?;
    let kept = discord
        .wait_for(&Method::PATCH, "/channels/300/messages/901", 3)
        .await?;
    let text = kept[2].content();
    assert!(
        text.starts_with("✔ Done · answered 1m 0s after the request"),
        "{text}"
    );
    assert!(text.contains("✔ read `src/lib.rs`") && !text.contains("cargo build"));
    recorded_soon(&records, &json!({})).await?;
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    let seen = discord.seen();
    assert!(
        seen.iter()
            .all(|request| request.at <= kept[2].at && request.method != Method::DELETE),
        "{seen:#?}"
    );
    assert_eq!(discord.matching(&Method::POST, "/channels").len(), 1);
    monitor.abort();
    Ok(())
}

#[tokio::test]
async fn a_fast_job_gets_one_standalone_summary_after_its_plain_answer_and_a_plain_one_none()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    let discord = Substitute::start(succeed()).await?;
    let accepted = now() - 60;
    let made = tools(None, &[], &[("bash", "make", true)]);
    let mut plain = tools(None, &[], &[]);
    plain.agent = Some(AgentInfo {
        model: Some("claude-fable-5-1".to_owned()),
        effort: None,
        context: None,
    });
    // Three jobs ended between two reads, before any read listed them.
    discord.record(vec![
        ended(
            "tools",
            "300",
            accepted,
            TurnState::Completed,
            Some(made),
            None,
        ),
        ended(
            "plain",
            "300",
            accepted,
            TurnState::Completed,
            Some(plain),
            None,
        ),
        ended(
            "stopped",
            "300",
            accepted,
            TurnState::Interrupted,
            None,
            None,
        ),
    ]);
    for (id, origin, state) in [
        (1, "tools", TurnState::Completed),
        (2, "plain", TurnState::Completed),
        (3, "stopped", TurnState::Interrupted),
    ] {
        discord.deliver(answer(id, origin, origin, state));
    }
    let mut provider = discord.provider(discord.http(false), config(false), &records);
    discord
        .wait_for(&Method::POST, "/channels/300/messages", 5)
        .await?;
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    provider.stop().await?;
    let messages = discord.matching(&Method::POST, "/channels/300/messages");
    // Answers are plain text; each job that earned a summary then gets one
    // card of its own showing it, after its answer.
    let answers = messages.iter().filter(|seen| !seen.is_card()).count();
    let cards = discord.cards_in("300");
    assert_eq!((answers, cards.len()), (3, 2), "{messages:#?}");
    assert!(!messages[0].is_card(), "{messages:#?}");
    let texts = cards.iter().map(Seen::content).collect::<Vec<_>>();
    assert!(
        texts
            .iter()
            .any(|text| text.starts_with("✓ Done · answered 1m 0s")
                && text.contains("✓ bash `make`")),
        "{texts:#?}"
    );
    assert!(
        texts
            .iter()
            .any(|text| text.starts_with("⏹️ Stopped before finishing.")),
        "{texts:#?}"
    );
    assert!(
        texts.iter().all(|text| !text.contains("claude")),
        "{texts:#?}"
    );
    assert!(discord.matching(&Method::PATCH, "/channels").is_empty());
    assert!(!records.exists() || recorded(&records)? == json!({}));
    Ok(())
}

#[tokio::test]
async fn a_slow_plain_answers_temporary_card_is_removed_even_with_agent_metadata()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    let discord = Substitute::start(succeed()).await?;
    let mut plain = tools(None, &[], &[]);
    plain.agent = Some(AgentInfo {
        model: Some("claude-fable-5-1".to_owned()),
        effort: Some("high".to_owned()),
        context: None,
    });
    let accepted = now() - 60;
    discord.record(vec![running("plain", "300", accepted, Some(plain.clone()))]);
    let (delivering, monitor) = discord.monitor(discord.http(false), config(false), &records);
    discord
        .wait_for(&Method::POST, "/channels/300/messages", 1)
        .await?;
    recorded_soon(
        &records,
        &json!({"plain": {"channel_id": "300", "message_id": "901"}}),
    )
    .await?;
    let done = ended(
        "plain",
        "300",
        accepted,
        TurnState::Completed,
        Some(plain),
        None,
    );
    discord.record(vec![sent(done)]);
    delivering.send(Delivering::Recorded)?;
    discord
        .wait_for(&Method::DELETE, "/channels/300/messages/901", 1)
        .await?;
    assert!(discord.matching(&Method::PATCH, "/channels").is_empty());
    recorded_soon(&records, &json!({})).await?;
    monitor.abort();
    Ok(())
}

#[tokio::test]
async fn an_early_answer_and_a_later_report_keep_one_card_ending_on_the_works_last_state()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    let discord = Substitute::start(succeed()).await?;
    let accepted = now() - 60;
    // The message answered early; the child it delegated still runs, and
    // the observation core queued to report it waits.
    let parent_report = delegating(
        tools(None, &[], &[("bash", "make", true)]),
        WorkState::Running,
    );
    let parent = |delivery| {
        let mut parent = turn(
            "parent",
            TurnState::Completed,
            accepted,
            Some(parent_report.clone()),
        );
        parent.delivery = Some(delivery);
        parent
    };
    let observation = |state, activity| turn("report", state, accepted + 5, activity);
    discord.record(vec![job(
        "parent",
        "300",
        accepted,
        vec![
            parent(DeliveryState::Pending),
            observation(TurnState::Queued, None),
        ],
    )]);
    let (delivering, monitor) = discord.monitor(discord.http(false), config(false), &records);
    let created = discord
        .wait_for(&Method::POST, "/channels/300/messages", 1)
        .await?;
    let text = created[0].content();
    assert!(
        text.starts_with("⏳ Answered · delegated work goes on"),
        "{text}"
    );
    assert!(text.contains("✓ bash `make`"), "{text}");
    assert!(text.contains("delegation `research` · running"), "{text}");
    // The observation runs, showing the child's progress.
    let waiting = delegating(
        tools(Some("Waiting for research."), &[], &[]),
        WorkState::Running,
    );
    discord.record(vec![job(
        "parent",
        "300",
        accepted,
        vec![
            parent(DeliveryState::Delivered),
            observation(TurnState::Running, Some(waiting)),
        ],
    )]);
    let working = discord
        .wait_for_text(&Method::PATCH, "/channels/300/messages/901", "Working")
        .await?;
    assert!(working.content().contains("> Waiting for research."));
    // Its report ends the job with the child's final state.
    let reported = delegating(tools(None, &[], &[]), WorkState::Succeeded);
    let mut report = observation(TurnState::Completed, Some(reported));
    report.delivery = Some(DeliveryState::Delivered);
    discord.record(vec![job(
        "parent",
        "300",
        accepted,
        vec![parent(DeliveryState::Delivered), report],
    )]);
    delivering.send(Delivering::Recorded)?;
    let summary = discord
        .wait_for_text(&Method::PATCH, "/channels/300/messages/901", "Done")
        .await?
        .content();
    assert!(summary.starts_with("✓ Done · answered"), "{summary}");
    assert!(summary.contains("✓ bash `make`"), "{summary}");
    // One row per piece of work, as its latest report says.
    assert_eq!(summary.matches("research").count(), 1, "{summary}");
    assert!(
        summary.contains("delegation `research` · done · 1m 1s"),
        "{summary}"
    );
    assert!(!summary.contains("may not be shown"), "{summary}");
    recorded_soon(&records, &json!({})).await?;
    assert_eq!(discord.matching(&Method::POST, "/channels").len(), 1);
    assert!(discord.matching(&Method::DELETE, "/channels").is_empty());
    monitor.abort();
    Ok(())
}

#[tokio::test]
async fn jobs_of_one_conversation_get_cards_of_their_own_across_pages_of_jobs_at_work()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    let discord = Substitute::start(succeed()).await?;
    let accepted = now() - 60;
    let make = || tools(None, &[("bash", "make")], &[]);
    let second = |state| {
        job(
            "second",
            "300",
            accepted,
            vec![turn("second", state, accepted, None)],
        )
    };
    // Two messages of one conversation, one waiting behind the other, and
    // more jobs at work than a page of them lists.
    discord.record(vec![
        running("first", "300", accepted, Some(make())),
        second(TurnState::Queued),
        running("other", "301", accepted, Some(make())),
    ]);
    let (delivering, monitor) = discord.monitor(discord.http(false), config(false), &records);
    let created = discord.wait_for(&Method::POST, "/channels/30", 3).await?;
    // The cards are made at once, in no set order; each gets the next ID.
    let card = |channel: &str, waits: bool| {
        created
            .iter()
            .position(|seen| {
                seen.path == format!("/channels/{channel}/messages")
                    && seen.content().contains("Waiting for earlier work") == waits
            })
            .map(|index| (901 + index).to_string())
            .with_context(|| format!("no card in {channel}"))
    };
    let (first_card, second_card, other_card) =
        (card("300", false)?, card("300", true)?, card("301", false)?);
    // The first job ends; the second starts on its own card.
    let first = ended(
        "first",
        "300",
        accepted,
        TurnState::Completed,
        Some(tools(None, &[], &[("bash", "make", true)])),
        None,
    );
    discord.record(vec![
        sent(first),
        second(TurnState::Running),
        running("other", "301", accepted, Some(make())),
    ]);
    delivering.send(Delivering::Recorded)?;
    discord
        .wait_for_text(
            &Method::PATCH,
            &format!("/channels/300/messages/{first_card}"),
            "✓ Done",
        )
        .await?;
    discord
        .wait_for_text(
            &Method::PATCH,
            &format!("/channels/300/messages/{second_card}"),
            "Working",
        )
        .await?;
    recorded_soon(
        &records,
        &json!({
            "other": {"channel_id": "301", "message_id": other_card},
            "second": {"channel_id": "300", "message_id": second_card},
        }),
    )
    .await?;
    assert_eq!(discord.cards_in("300").len(), 2);
    monitor.abort();
    Ok(())
}

#[tokio::test]
async fn a_restart_goes_on_with_the_cards_of_jobs_at_work_from_what_core_recorded()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    let before = json!({
        "running": {"channel_id": "300", "message_id": "555"},
        "continued": {"channel_id": "301", "message_id": "556"},
        "unnamed": {"channel_id": "305"},
    });
    std::fs::write(&records, serde_json::to_vec(&before)?)?;
    let discord = Substitute::start(succeed()).await?;
    let accepted = now() - 60;
    let mut parent = turn(
        "continued",
        TurnState::Completed,
        accepted,
        Some(tools(None, &[], &[("bash", "make", true)])),
    );
    parent.delivery = Some(DeliveryState::Delivered);
    let read = tools(None, &[("read", "notes.md")], &[]);
    let sleeping = tools(None, &[("bash", "sleep 60")], &[]);
    discord.record(vec![
        running("running", "300", accepted, Some(sleeping)),
        job(
            "continued",
            "301",
            accepted,
            vec![
                parent,
                turn("report", TurnState::Running, accepted, Some(read)),
            ],
        ),
        running("unnamed", "305", accepted, None),
    ]);
    let (_delivering, monitor) = discord.monitor(discord.http(false), config(false), &records);
    // Jobs that go on are shown on their cards from what core recorded,
    // including what an earlier turn reported.
    let live = discord
        .wait_for(&Method::PATCH, "/channels/300/messages/555", 1)
        .await?;
    assert!(live[0].content().contains("↳ bash `sleep 60`"));
    let continued = discord
        .wait_for(&Method::PATCH, "/channels/301/messages/556", 1)
        .await?[0]
        .content();
    assert!(continued.starts_with("⏳ Working"), "{continued}");
    assert!(
        continued.contains("✓ bash `make`") && continued.contains("↳ read `notes.md`"),
        "{continued}"
    );
    assert!(!continued.contains("may not be shown"), "{continued}");
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    // A card nobody can name holds its job from another until it settles.
    let seen = discord.seen();
    assert!(
        seen.iter().all(|request| request.method == Method::PATCH),
        "{seen:#?}"
    );
    assert_eq!(recorded(&records)?, before);
    monitor.abort();
    Ok(())
}

#[tokio::test]
async fn a_restart_finishes_the_cards_of_jobs_that_ended_meanwhile_from_what_core_recorded()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    std::fs::write(
        &records,
        serde_json::to_vec(&json!({
            "settled": {"channel_id": "302", "message_id": "557"},
            "plain": {"channel_id": "303", "message_id": "558"},
            "lost": {"channel_id": "304", "message_id": "559"},
            "unnamed": {"channel_id": "306"},
        }))?,
    )?;
    let discord = Substitute::start(succeed()).await?;
    let accepted = now() - 60;
    let reported = tools(None, &[], &[("read", "notes.md", true)]);
    let completed = TurnState::Completed;
    discord.record(vec![
        sent(ended(
            "settled",
            "302",
            accepted,
            completed,
            Some(reported),
            None,
        )),
        sent(ended("plain", "303", accepted, completed, None, None)),
        sent(ended("unnamed", "306", accepted, completed, None, None)),
    ]);
    let (_delivering, monitor) = discord.monitor(discord.http(false), config(false), &records);
    // A job that settled shows its summary; one that earned none, or that
    // core holds nothing of, loses its card; a card nobody can name is
    // forgotten.
    let settled = discord
        .wait_for(&Method::PATCH, "/channels/302/messages/557", 1)
        .await?[0]
        .content();
    assert!(
        settled.starts_with("✓ Done · answered 1m 0s after the request"),
        "{settled}"
    );
    assert!(settled.contains("✓ read `notes.md`"), "{settled}");
    discord
        .wait_for(&Method::DELETE, "/channels/303/messages/558", 1)
        .await?;
    discord
        .wait_for(&Method::DELETE, "/channels/304/messages/559", 1)
        .await?;
    recorded_soon(&records, &json!({})).await?;
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    assert!(discord.matching(&Method::POST, "/channels").is_empty());
    assert_eq!(discord.seen().len(), 3, "{:#?}", discord.seen());
    monitor.abort();
    Ok(())
}

#[tokio::test]
async fn a_read_begun_before_a_job_ended_is_shown_and_the_next_read_ends_the_card_promptly()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    let discord = Substitute::start(succeed()).await?;
    let accepted = now() - 60;
    let make = |detail| tools(None, &[("bash", detail)], &[]);
    discord.record(vec![running("job", "300", accepted, Some(make("make")))]);
    let (delivering, monitor) = discord.monitor(discord.http(false), config(false), &records);
    discord
        .wait_for(&Method::POST, "/channels/300/messages", 1)
        .await?;
    // A read takes core's record while the job still runs, and answers late.
    let held = discord.hold_reads().await;
    let begun = discord.reads();
    discord.record(vec![running(
        "job",
        "300",
        accepted,
        Some(make("make check")),
    )]);
    discord.read_taken(begun).await?;
    let report = tools(None, &[], &[("bash", "make check", true)]);
    let done = ended(
        "job",
        "300",
        accepted,
        TurnState::Completed,
        Some(report),
        None,
    );
    discord.record(vec![sent(done)]);
    delivering.send(Delivering::Recorded)?;
    let released = Instant::now();
    drop(held);
    // The late read shows what it took; the next follows at once and ends
    // the card, which nothing takes back.
    let summary = discord
        .wait_for_text(&Method::PATCH, "/channels/300/messages/901", "✓ Done")
        .await?;
    assert!(
        summary.at - released < Duration::from_millis(1_500),
        "the next read waited for the poll interval"
    );
    tokio::time::sleep(Duration::from_millis(4_000)).await;
    let edits = discord.matching(&Method::PATCH, "/channels/300/messages/901");
    assert!(
        edits
            .last()
            .is_some_and(|last| last.content().starts_with("✓ Done")),
        "{edits:#?}"
    );
    assert!(edits.iter().all(|edit| edit.at <= summary.at));
    assert_eq!(discord.matching(&Method::POST, "/channels").len(), 1);
    monitor.abort();
    Ok(())
}

#[tokio::test]
async fn unknown_refused_or_deleted_cards_are_never_created_again_or_replaced() -> anyhow::Result<()>
{
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    let discord = Substitute::start(Arc::new(|seen, _| {
        match (&seen.method, seen.path.as_str()) {
            (&Method::POST, "/channels/300/messages") => Reply::Status(500),
            (&Method::PATCH, path) if path.starts_with("/channels/302/") => Reply::Status(404),
            (&Method::POST, "/channels/301/messages") | (&Method::PATCH, _) => Reply::Status(403),
            _ => Reply::Success,
        }
    }))
    .await?;
    let accepted = now() - 60;
    let channels = ["300", "301", "302", "303"];
    let jobs = |detail: &str| {
        channels
            .iter()
            .map(|channel| {
                let activity = tools(None, &[("bash", detail)], &[]);
                running(channel, channel, accepted, Some(activity))
            })
            .collect()
    };
    discord.record(jobs("first"));
    let (delivering, monitor) = discord.monitor(discord.http(false), config(true), &records);
    for channel in channels {
        let created = format!("/channels/{channel}/messages");
        discord.wait_for(&Method::POST, &created, 1).await?;
    }
    discord.record(jobs("second"));
    discord
        .wait_for(&Method::PATCH, "/channels/302/", 1)
        .await?;
    discord
        .wait_for(&Method::PATCH, "/channels/303/", 1)
        .await?;
    discord.record(jobs("third"));
    tokio::time::sleep(Duration::from_millis(7_000)).await;
    // Each job made one creation, whatever became of it; a refused edit
    // and a card Discord no longer has are not edited again.
    assert_eq!(
        discord
            .matching(&Method::POST, "/channels/30")
            .iter()
            .filter(|seen| seen.path.ends_with("/messages"))
            .count(),
        4
    );
    assert_eq!(discord.matching(&Method::PATCH, "/channels").len(), 2);
    // Typing does not depend on cards.
    discord
        .wait_for(&Method::POST, "/channels/300/typing", 2)
        .await?;
    // Unknown, refused and deleted cards retain their suppression checkpoint,
    // so their jobs get no other card even after a restart.
    recorded_soon(
        &records,
        &json!({
            "300": {"channel_id": "300"},
            "301": {"channel_id": "301"},
            "302": {"channel_id": "302"},
            "303": {"channel_id": "303", "message_id": "904"},
        }),
    )
    .await?;
    // Every job settles, having earned a summary.
    let done = tools(None, &[], &[("bash", "third", true)]);
    discord.record(
        channels
            .iter()
            .map(|channel| {
                let report = Some(done.clone());
                sent(ended(
                    channel,
                    channel,
                    accepted,
                    TurnState::Completed,
                    report,
                    None,
                ))
            })
            .collect(),
    );
    delivering.send(Delivering::Recorded)?;
    // Refusing edits does not revoke the earned summary. Its card stays
    // recorded for retry; unknown and deleted cards get no replacement.
    discord
        .wait_for(&Method::PATCH, "/channels/303/messages/904", 2)
        .await?;
    recorded_soon(
        &records,
        &json!({"303": {"channel_id": "303", "message_id": "904"}}),
    )
    .await?;
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    assert!(discord.matching(&Method::DELETE, "/channels").is_empty());
    assert_eq!(
        discord
            .matching(&Method::POST, "/channels/30")
            .iter()
            .filter(|seen| seen.path.ends_with("/messages"))
            .count(),
        4
    );
    monitor.abort();
    Ok(())
}

#[tokio::test]
async fn a_card_left_unfinished_is_finished_by_the_next_start_without_another() -> anyhow::Result<()>
{
    for status in [403, 500] {
        let directory = tempfile::tempdir()?;
        let records = directory.path().join("activity-cards.json");
        let failing = Arc::new(AtomicBool::new(true));
        let discord = Substitute::start(Arc::new({
            let failing = failing.clone();
            move |seen, _| {
                if seen.method == Method::DELETE && failing.load(Ordering::SeqCst) {
                    Reply::Status(status)
                } else {
                    Reply::Success
                }
            }
        }))
        .await?;
        let accepted = now() - 60;
        discord.record(vec![running("plain", "300", accepted, None)]);
        let (delivering, monitor) = discord.monitor(discord.http(false), config(false), &records);
        discord
            .wait_for(&Method::POST, "/channels/300/messages", 1)
            .await?;
        discord.record(vec![sent(ended(
            "plain",
            "300",
            accepted,
            TurnState::Completed,
            None,
            None,
        ))]);
        delivering.send(Delivering::Recorded)?;
        // Every removal fails; the card stays recorded.
        let attempts = if status == 403 { 1 } else { 3 };
        discord
            .wait_for(&Method::DELETE, "/channels/300/messages/901", attempts)
            .await?;
        tokio::time::sleep(Duration::from_millis(500)).await;
        monitor.abort();
        let _ = monitor.await;
        assert_eq!(
            recorded(&records)?,
            json!({"plain": {"channel_id": "300", "message_id": "901"}})
        );
        // The next start finishes it from what core recorded of its job.
        failing.store(false, Ordering::SeqCst);
        let (_delivering, monitor) = discord.monitor(discord.http(false), config(false), &records);
        discord
            .wait_for(&Method::DELETE, "/channels/300/messages/901", attempts + 1)
            .await?;
        recorded_soon(&records, &json!({})).await?;
        assert!(discord.matching(&Method::PATCH, "/channels").is_empty());
        assert_eq!(
            discord
                .matching(&Method::POST, "/channels/300/messages")
                .len(),
            1
        );
        monitor.abort();
    }
    Ok(())
}

#[tokio::test]
async fn a_job_moving_between_pages_has_one_card_owner_with_its_latest_read() -> anyhow::Result<()>
{
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    let discord = Substitute::start(succeed()).await?;
    let accepted = now() - 60;
    let job = |origin, channel, detail| {
        running(
            origin,
            channel,
            accepted,
            Some(tools(None, &[("bash", detail)], &[])),
        )
    };
    discord.record(vec![
        job("a", "300", "first"),
        job("b", "301", "b"),
        job("c", "302", "c"),
    ]);
    let held = discord.hold_reads().await;
    let (_delivering, monitor) = discord.monitor(discord.http(false), config(false), &records);
    discord.read_taken(0).await?;
    // Completing a turn queues its observation at a later acceptance sequence:
    // the same origin can appear again after the first page's cursor.
    let mut continued = job("a", "300", "observation");
    continued.turns[0].turn_id = "a-observation".to_owned();
    discord.record(vec![job("b", "301", "b"), job("c", "302", "c"), continued]);
    drop(held);
    for channel in ["300", "301", "302"] {
        discord
            .wait_for(&Method::POST, &format!("/channels/{channel}/messages"), 1)
            .await?;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let cards = discord.cards_in("300");
    assert_eq!(cards.len(), 1, "{cards:#?}");
    assert!(cards[0].content().contains("observation"));
    monitor.abort();
    Ok(())
}

#[tokio::test]
async fn a_refused_final_edit_keeps_the_card_for_the_next_start() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    let refusing = Arc::new(AtomicBool::new(true));
    let discord = Substitute::start(Arc::new({
        let refusing = refusing.clone();
        move |seen, _| {
            if seen.method == Method::PATCH
                && seen.content().starts_with("✓ Done")
                && refusing.load(Ordering::SeqCst)
            {
                Reply::Status(403)
            } else {
                Reply::Success
            }
        }
    }))
    .await?;
    let accepted = now() - 60;
    discord.record(vec![running(
        "work",
        "300",
        accepted,
        Some(tools(None, &[("bash", "make")], &[])),
    )]);
    let (delivering, monitor) = discord.monitor(discord.http(false), config(false), &records);
    discord
        .wait_for(&Method::POST, "/channels/300/messages", 1)
        .await?;
    discord.record(vec![sent(ended(
        "work",
        "300",
        accepted,
        TurnState::Completed,
        Some(tools(None, &[], &[("bash", "make", true)])),
        None,
    ))]);
    delivering.send(Delivering::Recorded)?;
    discord
        .wait_for_text(&Method::PATCH, "/channels/300/messages/901", "✓ Done")
        .await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    monitor.abort();
    let _ = monitor.await;
    assert!(discord.matching(&Method::DELETE, "/channels").is_empty());
    assert_eq!(
        recorded(&records)?,
        json!({"work": {"channel_id": "300", "message_id": "901"}})
    );
    refusing.store(false, Ordering::SeqCst);
    let (_delivering, monitor) = discord.monitor(discord.http(false), config(false), &records);
    discord
        .wait_for(&Method::PATCH, "/channels/300/messages/901", 2)
        .await?;
    recorded_soon(&records, &json!({})).await?;
    assert_eq!(discord.cards_in("300").len(), 1);
    monitor.abort();
    Ok(())
}

/// A retry must not be sent unless its suppression is durably recorded.
#[tokio::test]
async fn a_failed_progress_identity_save_does_not_create_a_second_card_after_restart()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("cards.json");
    let saved = directory.path().join("saved.json");
    std::fs::write(&records, b"{}")?;
    let discord = Substitute::start(Arc::new({
        let records = records.clone();
        let saved = saved.clone();
        move |seen, earlier| {
            if seen.method == Method::POST && earlier == 0 {
                let saved = std::fs::rename(&records, &saved).map_err(|error| error.kind());
                assert_eq!(saved, Ok(()), "save the pre-creation checkpoint");
                let obstructed = std::fs::create_dir(&records).map_err(|error| error.kind());
                assert_eq!(obstructed, Ok(()), "obstruct the identity publication");
            }
            Reply::Success
        }
    }))
    .await?;
    let cards = cards(
        discord.http(false),
        directory.path(),
        CancellationToken::new(),
    )
    .await?;
    let job = running(
        "work",
        "300",
        now() - 60,
        Some(tools(None, &[("bash", "sleep 15")], &[])),
    );
    let embed = embed("work");
    assert!(matches!(
        cards
            .create_progress("work", Some(Id::new(300)), &embed)
            .await,
        super::Holding::Card { .. }
    ));
    drop(cards);
    std::fs::remove_dir(&records)?;
    std::fs::rename(&saved, &records)?;
    discord.record(vec![job]);
    let (_delivering, monitor) = discord.monitor(discord.http(false), config(false), &records);
    // The identity was not saved, so a restart cannot update the first card.
    // The durable checkpoint nevertheless suppresses a duplicate creation.
    tokio::time::sleep(Duration::from_millis(3_200)).await;
    assert_eq!(discord.cards_in("300").len(), 1);
    assert_eq!(recorded(&records)?, json!({"work": {"channel_id": "300"}}));
    assert!(discord.matching(&Method::PATCH, "/channels").is_empty());
    monitor.abort();
    Ok(())
}

#[tokio::test]
async fn an_uncertain_progress_edit_keeps_the_identity_to_repair_a_late_overwrite()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    let remote = Arc::new(Mutex::new(String::new()));
    let discord = Substitute::start(Arc::new({
        let remote = remote.clone();
        move |seen, _| {
            if seen.method != Method::PATCH {
                return Reply::Success;
            }
            let content = seen.content();
            if content.contains("Finishing up") {
                let remote = remote.clone();
                // The request reached Discord, but its application and reply
                // arrive after the client's timeout and the final edit.
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(18)).await;
                    *lock(&remote) = content;
                });
                Reply::Delayed(Duration::from_secs(18))
            } else {
                *lock(&remote) = content;
                Reply::Success
            }
        }
    }))
    .await?;
    let accepted = now() - 60;
    let report = tools(None, &[], &[("bash", "make", true)]);
    discord.record(vec![running("work", "300", accepted, Some(report.clone()))]);
    let (delivering, monitor) = discord.monitor(discord.http(false), config(false), &records);
    discord
        .wait_for(&Method::POST, "/channels/300/messages", 1)
        .await?;
    let finishing = ended(
        "work",
        "300",
        accepted,
        TurnState::Completed,
        Some(report),
        None,
    );
    discord.record(vec![finishing.clone()]);
    discord
        .wait_for_text(&Method::PATCH, "/channels/300/messages/901", "Finishing up")
        .await?;
    discord.record(vec![sent(finishing)]);
    delivering.send(Delivering::Recorded)?;
    discord
        .wait_for_text(&Method::PATCH, "/channels/300/messages/901", "✓ Done")
        .await?;
    tokio::time::timeout(Duration::from_secs(20), async {
        while !lock(&remote).contains("Finishing up") {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    // A successful final edit did not fence the earlier uncertain write.
    // Keep its identity so the next start can repair it from Core's facts.
    assert_eq!(
        recorded(&records)?,
        json!({"work": {"channel_id": "300", "message_id": "901"}})
    );
    monitor.abort();
    let _ = monitor.await;
    let (_delivering, monitor) = discord.monitor(discord.http(false), config(false), &records);
    let edits = discord
        .wait_for(&Method::PATCH, "/channels/300/messages/901", 3)
        .await?;
    assert!(
        edits
            .last()
            .is_some_and(|edit| edit.content().starts_with("✓ Done"))
    );
    recorded_soon(&records, &json!({})).await?;
    assert!(lock(&remote).starts_with("✓ Done"));
    assert_eq!(discord.cards_in("300").len(), 1);
    monitor.abort();
    Ok(())
}

#[tokio::test]
async fn failed_creation_suppression_sends_no_retryable_summary() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("cards.json");
    let retry = json!({"fast": {"channel_id": "300", "pending_create": true}});
    std::fs::write(&records, serde_json::to_vec(&retry)?)?;
    let discord = Substitute::start(succeed()).await?;
    let cards = cards(
        discord.http(false),
        directory.path(),
        CancellationToken::new(),
    )
    .await?;
    // A real publication refusal, independent of the test user's authority.
    // Preserve the durable retry record, obstruct its replacement, then repair.
    let saved = directory.path().join("saved.json");
    std::fs::rename(&records, &saved)?;
    std::fs::create_dir(&records)?;
    let job = sent(ended(
        "fast",
        "300",
        now(),
        TurnState::Completed,
        Some(tools(None, &[], &[("bash", "make", true)])),
        None,
    ));
    let summary = embed("fast");
    cards
        .finish(
            "fast",
            super::Holding::Nothing,
            Some(summary),
            Some(Id::new(300)),
        )
        .await;
    assert!(discord.cards_in("300").is_empty());
    std::fs::remove_dir(&records)?;
    std::fs::rename(&saved, &records)?;
    assert_eq!(recorded(&records)?, retry);
    discord.record(vec![job]);
    let (_delivering, monitor) = discord.monitor(discord.http(false), config(false), &records);
    discord
        .wait_for_text(&Method::POST, "/channels/300/messages", "✓ Done")
        .await?;
    recorded_soon(&records, &json!({})).await?;
    assert_eq!(discord.cards_in("300").len(), 1);
    monitor.abort();
    Ok(())
}

/// Uses the rate limiter: an exhausted bucket proves no summary was sent.
#[tokio::test]
async fn an_unsent_fast_summary_is_retried_after_restart() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    let discord = Substitute::start(Arc::new(|_, earlier| {
        if earlier == 0 {
            Reply::Bucket("messages", 0, 120)
        } else {
            Reply::Success
        }
    }))
    .await?;
    let http = discord.http(true);
    crate::delivery::send_discord_message(&http, Id::new(300), "prime the bucket").await?;
    discord.record(vec![sent(ended(
        "fast",
        "300",
        now(),
        TurnState::Completed,
        Some(tools(None, &[], &[("bash", "make", true)])),
        None,
    ))]);
    let (delivering, monitor) = discord.monitor(http, config(false), &records);
    delivering.send(Delivering::Claimed("fast".to_owned()))?;
    recorded_soon(&records, &json!({"fast": {"channel_id": "300"}})).await?;
    tokio::time::timeout(Duration::from_secs(30), async {
        while !recorded(&records).is_ok_and(|value| value["fast"]["pending_create"] == true) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    // Restart between attempts. The record says
    // only that creation has not been sent, never mirrors the job's outcome.
    monitor.abort();
    let _ = monitor.await;
    assert_eq!(
        recorded(&records)?,
        json!({"fast": {"channel_id": "300", "pending_create": true}})
    );
    assert_eq!(discord.cards_in("300").len(), 0);
    let (_delivering, monitor) = discord.monitor(discord.http(false), config(false), &records);
    let summary = discord
        .wait_for_text(&Method::POST, "/channels/300/messages", "✓ Done")
        .await?;
    assert!(summary.content().contains("make"));
    recorded_soon(&records, &json!({})).await?;
    assert_eq!(discord.cards_in("300").len(), 1);
    monitor.abort();
    Ok(())
}

#[tokio::test]
async fn followed_jobs_are_read_by_name_a_bounded_number_at_a_time() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    // Forty jobs' cards nobody can name, all settled since.
    let unnamed = (0..40)
        .map(|index| (format!("job-{index:02}"), json!({"channel_id": "300"})))
        .collect::<serde_json::Map<_, _>>();
    std::fs::write(&records, serde_json::to_vec(&unnamed)?)?;
    let discord = Substitute::start(succeed()).await?;
    let accepted = now() - 60;
    discord.record(
        unnamed
            .keys()
            .map(|origin| {
                sent(ended(
                    origin,
                    "300",
                    accepted,
                    TurnState::Completed,
                    None,
                    None,
                ))
            })
            .collect(),
    );
    let (_delivering, monitor) = discord.monitor(discord.http(false), config(false), &records);
    recorded_soon(&records, &json!({})).await?;
    let mut sizes = discord.named().iter().map(Vec::len).collect::<Vec<_>>();
    sizes.sort_unstable();
    assert_eq!(sizes, [8, 32]);
    assert!(discord.seen().is_empty(), "{:#?}", discord.seen());
    monitor.abort();
    Ok(())
}

#[tokio::test]
async fn new_jobs_type_and_cards_draw_as_the_settings_serving_then_say() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    let discord = Substitute::start(succeed()).await?;
    let accepted = now();
    let work = || tools(None, &[("bash", "make")], &[]);
    let first = running("first", "300", accepted, Some(work()));
    discord.record(vec![first.clone()]);
    // The provider starts without typing, drawing the default markers.
    let settings = crate::config::Settings::new(config(false));
    let (_delivering, monitor) = discord.serving(discord.http(false), settings.clone(), &records);
    let card = discord
        .wait_for(&Method::POST, "/channels/300/messages", 1)
        .await?;
    assert!(card[0].content().starts_with("⏳ Working"), "{card:#?}");
    // The operator then serves typing and other markers, and another job
    // starts.
    let mut next = with_markers()?;
    next.typing = true;
    let changed = settings
        .apply(
            || Ok(next),
            |_| std::future::ready(Ok(crate::module::tests::unchanged())),
        )
        .await
        .map_err(|refusal| anyhow::anyhow!("{refusal:?}"))?
        .changed;
    assert_eq!(changed, ["typing", "activity_markers"]);
    discord.record(vec![
        first,
        running("second", "301", accepted, Some(work())),
    ]);
    discord
        .wait_for(&Method::POST, "/channels/301/typing", 1)
        .await?;
    let card = discord
        .wait_for(&Method::POST, "/channels/301/messages", 1)
        .await?;
    assert!(
        card[0].content().starts_with("<a:working:123> Working"),
        "{card:#?}"
    );
    // The job followed before keeps the typing decision it started with.
    assert!(
        discord
            .matching(&Method::POST, "/channels/300/typing")
            .is_empty()
    );
    monitor.abort();
    Ok(())
}

#[tokio::test]
async fn an_unreachable_core_neither_finishes_nor_removes_a_card() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    let discord = Substitute::start(succeed()).await?;
    let accepted = now() - 60;
    let make = tools(None, &[("bash", "x")], &[]);
    discord.record(vec![running("turn", "300", accepted, Some(make))]);
    let (_delivering, monitor) = discord.monitor(discord.http(false), config(false), &records);
    discord
        .wait_for(&Method::POST, "/channels/300/messages", 1)
        .await?;
    discord.unavailable();
    tokio::time::sleep(Duration::from_millis(6_500)).await;
    assert_eq!(discord.seen().len(), 1, "{:#?}", discord.seen());
    // Ended, its answer not yet sent: the card stops showing running calls.
    discord.record(vec![ended(
        "turn",
        "300",
        accepted,
        TurnState::Completed,
        None,
        None,
    )]);
    let finishing = discord
        .wait_for(&Method::PATCH, "/channels/300/messages/901", 1)
        .await?;
    assert!(finishing[0].content().contains("Finishing up"));
    assert!(!finishing[0].content().contains("bash `x`"));
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
    // A record of an older format is not guessed at.
    let unsupported = directory.path().join("unsupported.json");
    let old = r#"{"turn": {"channel_id": "300", "message_id": "555", "hosted": true}}"#;
    std::fs::write(&unsupported, old)?;
    let unreadable = directory.path().join("unreadable.json");
    std::fs::write(
        &unreadable,
        r#"{"turn": {"channel_id": "300", "message_id": "555"}}"#,
    )?;
    std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000))?;
    let mut faults = vec![occupied.clone(), malformed.clone(), unsupported.clone()];
    // The owner of the file can read it regardless when running as root.
    if std::fs::read(&unreadable).is_err() {
        faults.push(unreadable.clone());
    }
    for records in faults {
        let discord = Substitute::start(succeed()).await?;
        let activity = tools(None, &[("bash", "sleep 600")], &[]);
        discord.record(vec![running("turn", "300", now() - 60, Some(activity))]);
        let (_delivering, monitor) = discord.monitor(discord.http(false), config(true), &records);
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
    assert_eq!(std::fs::read_to_string(&unsupported)?, old);
    std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o600))?;
    assert_eq!(
        std::fs::read_to_string(&unreadable)?,
        r#"{"turn": {"channel_id": "300", "message_id": "555"}}"#
    );
    Ok(())
}

/// Uses the rate limiter.
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
    let make = tools(None, &[("bash", "x")], &[]);
    discord.record(vec![running("turn", "300", now() - 60, Some(make))]);
    let (_delivering, monitor) = discord.monitor(discord.http(true), config(false), &records);
    let attempts = discord
        .wait_for(&Method::POST, "/channels/300/messages", 3)
        .await?;
    // Twilight waited out each limit before its own retry.
    assert!(attempts[2].at - attempts[0].at >= Duration::from_millis(1_900));
    tokio::time::sleep(Duration::from_millis(4_000)).await;
    assert_eq!(discord.cards_in("300").len(), 3);
    recorded_soon(
        &records,
        &json!({"turn": {"channel_id": "300", "message_id": "903"}}),
    )
    .await?;
    monitor.abort();
    Ok(())
}

/// Uses the rate limiter.
#[tokio::test]
async fn progress_stuck_behind_the_limiter_yields_to_the_jobs_end_and_answers() -> anyhow::Result<()>
{
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
    let accepted = now() - 60;
    let jobs = |detail: &str| {
        ["300", "301"]
            .iter()
            .map(|channel| {
                let activity = tools(None, &[("bash", detail)], &[]);
                running(channel, channel, accepted, Some(activity))
            })
            .collect()
    };
    discord.record(jobs("one"));
    let (delivering, monitor) = discord.monitor(http.clone(), config(false), &records);
    discord
        .wait_for(&Method::POST, "/channels/300/messages", 1)
        .await?;
    discord
        .wait_for(&Method::POST, "/channels/301/messages", 1)
        .await?;
    discord.record(jobs("two"));
    discord
        .wait_for(&Method::PATCH, "/channels/300/", 1)
        .await?;
    discord
        .wait_for(&Method::PATCH, "/channels/301/", 1)
        .await?;
    // The next edit of the first card now waits for a permit.
    discord.record(jobs("three"));
    tokio::time::sleep(Duration::from_millis(4_000)).await;
    assert_eq!(discord.matching(&Method::PATCH, "/channels/300/").len(), 1);

    // An answer to the same channel, on its own route, is not held by that edit.
    let started = Instant::now();
    crate::delivery::send_discord_message(&http, Id::new(300), "the answer").await?;
    assert!(started.elapsed() < Duration::from_secs(1));

    // Both jobs end: the waiting edit is dropped unsent, and the other card
    // shows its summary at once.
    let report = tools(None, &[], &[("bash", "three", true)]);
    discord.record(
        ["300", "301"]
            .iter()
            .map(|channel| {
                let report = Some(report.clone());
                sent(ended(
                    channel,
                    channel,
                    accepted,
                    TurnState::Completed,
                    report,
                    None,
                ))
            })
            .collect(),
    );
    delivering.send(Delivering::Recorded)?;
    let settled = Instant::now();
    let other = discord
        .wait_for_text(&Method::PATCH, "/channels/301/", "✓ Done")
        .await?;
    assert!(other.at - settled < Duration::from_secs(2));
    // The blocked card's summary waits for its own bucket; the progress
    // edit that waited there was never sent.
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    assert_eq!(discord.matching(&Method::PATCH, "/channels/300/").len(), 1);
    monitor.abort();
    Ok(())
}

/// A card that says `text`, for cases about the card's lifecycle alone.
fn embed(text: &str) -> twilight_model::channel::message::Embed {
    twilight_model::channel::message::Embed {
        author: None,
        color: None,
        description: Some(text.to_owned()),
        fields: Vec::new(),
        footer: None,
        image: None,
        kind: "rich".to_owned(),
        provider: None,
        thumbnail: None,
        timestamp: None,
        title: None,
        url: None,
        video: None,
    }
}

/// What a job's card tasks share, over records in `directory`.
async fn cards(
    http: Arc<twilight_http::Client>,
    directory: &Path,
    stopping: CancellationToken,
) -> anyhow::Result<super::Cards> {
    Ok(super::Cards {
        http,
        module: release_module().await?,
        settings: crate::config::Settings::new(config(false)),
        records: Arc::new(CardRecords::open(directory.join("cards.json")).context("records")?),
        stopping,
    })
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
    let accepted = now() - 60;
    let make = |channel: &str, detail: &str| {
        running(
            channel,
            channel,
            accepted,
            Some(tools(None, &[("bash", detail)], &[])),
        )
    };
    // Two jobs have cards: one is answered as the provider stops, one goes on.
    discord.record(vec![make("300", "make"), make("301", "make")]);
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
    // Core is slow to record the answer, which goes out plainly.
    let recording = discord.hold_recording().await;
    let report = tools(
        None,
        &[],
        &[("bash", "make", true), ("read", "notes.md", true)],
    );
    discord.record(vec![
        ended(
            "300",
            "300",
            accepted,
            TurnState::Completed,
            Some(report),
            None,
        ),
        make("301", "make"),
    ]);
    discord.deliver(answer(1, "300", "300", TurnState::Completed));
    let sent = discord
        .wait_for(&Method::POST, "/channels/300/messages", 2)
        .await?;
    assert!(!sent[1].is_card());
    // The provider stops; jobs core lists from now on are not shown.
    provider.stopping.cancel();
    let mut later = lock(&discord.core).jobs.clone().unwrap_or_default();
    later.push(make("302", "make"));
    discord.record(later);
    let started = Instant::now();
    let (stopped, ()) = tokio::join!(
        tokio::time::timeout(Duration::from_secs(25), provider.stop()),
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
async fn a_stopping_provider_makes_no_new_card() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let discord = Substitute::start(succeed()).await?;
    let stopping = CancellationToken::new();
    stopping.cancel();
    let cards = cards(discord.http(false), directory.path(), stopping).await?;
    // A job read before the stop, long past its card's appearance.
    let accepted = now() - 60;
    let make = tools(None, &[("bash", "make")], &[]);
    let (view, watched) = tokio::sync::watch::channel(super::Known::Read(running(
        "job",
        "300",
        accepted,
        Some(make),
    )));
    let showing = tokio::spawn(super::show_card(cards, "job".to_owned(), watched, None));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(discord.seen().is_empty(), "{:#?}", discord.seen());
    // It ends having earned a summary, which gets no card either.
    let report = tools(None, &[], &[("bash", "make", true)]);
    view.send_replace(super::Known::Read(sent(ended(
        "job",
        "300",
        accepted,
        TurnState::Completed,
        Some(report),
        None,
    ))));
    tokio::time::timeout(Duration::from_secs(5), showing).await??;
    assert!(discord.seen().is_empty());
    Ok(())
}

/// Uses the rate limiter.
#[tokio::test]
async fn a_stopping_provider_sends_no_waiting_progress_but_finishes_and_awaits_what_left()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    // Each route has one request to spend, then its permit returns two
    // seconds later; a card made in channel 301 is answered after a second.
    let discord = Substitute::start(Arc::new(|seen, earlier| {
        match (&seen.method, seen.path.as_str()) {
            (&Method::POST, "/channels/301/messages") => Reply::Delayed(Duration::from_secs(1)),
            (&Method::POST, _) if earlier == 0 => Reply::Bucket("creates", 0, 2),
            (&Method::PATCH, _) if earlier == 0 => Reply::Bucket("edits", 0, 2),
            _ => Reply::Success,
        }
    }))
    .await?;
    let stopping = CancellationToken::new();
    let cards = cards(discord.http(true), directory.path(), stopping.clone()).await?;
    // A running job, which nothing will settle while the provider stops.
    let job = running(
        "job",
        "300",
        now() - 60,
        Some(tools(None, &[("bash", "make")], &[])),
    );
    let embed = embed("job");
    let (_view, watched) = tokio::sync::watch::channel(super::Known::Read(job));
    let (channel, other) = (Id::new(300), Id::new(301));
    let card = super::Card {
        channel_id: channel,
        message_id: Id::new(777),
    };
    assert!(matches!(
        cards.create("initial", channel, &embed).await,
        super::Call::Done(_)
    ));
    assert_eq!(cards.edit(card, &embed, None).await, super::Call::Done(()));
    // A new card and progress now wait for permits; a card in channel 301 is under way.
    let spawn_create = |job_id, channel| {
        let (cards, embed) = (cards.clone(), embed.clone());
        tokio::spawn(async move { cards.create(job_id, channel, &embed).await })
    };
    let (waiting, under_way) = (
        spawn_create("waiting", channel),
        spawn_create("under-way", other),
    );
    let progress = {
        let (cards, embed) = (cards.clone(), embed.clone());
        let wanted = super::job_wanted(&watched, &stopping, |known| !known.is_over());
        tokio::spawn(async move { cards.edit(card, &embed, Some(wanted)).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    stopping.cancel();
    // What waited is not sent, at once; what left is still answered.
    let stopped = Instant::now();
    assert_eq!(waiting.await?, super::Call::NotSent);
    assert_eq!(progress.await?, super::Call::NotSent);
    assert!(stopped.elapsed() < Duration::from_millis(500));
    assert!(matches!(under_way.await?, super::Call::Done(_)));
    // A card's final edit still waits for its permit and is sent.
    assert_eq!(cards.edit(card, &embed, None).await, super::Call::Done(()));
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    let creates = discord.matching(&Method::POST, "/channels/300/messages");
    let edits = discord.matching(&Method::PATCH, "/channels/300/messages/777");
    assert_eq!(
        (creates.len(), edits.len()),
        (1, 2),
        "{:#?}",
        discord.seen()
    );
    assert_eq!(
        discord
            .matching(&Method::POST, "/channels/301/messages")
            .len(),
        1
    );
    Ok(())
}

/// A module that draws the cards a module is checked with, and traps on
/// every real job's card.
async fn trapping_module(directory: &Path) -> anyhow::Result<Arc<Module>> {
    std::fs::write(
        directory.join("release.wasm"),
        crate::module::tests::other_cards("(unreachable)", &[])?,
    )?;
    Ok(Arc::new(
        Module::load(
            crate::module::tests::files(directory),
            crate::module::Source::Bundled,
        )
        .await?,
    ))
}

#[tokio::test]
async fn a_module_that_draws_no_card_costs_no_answer_and_leaves_cards_for_the_next_start()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    // A card an earlier start made, of a job that has ended since.
    let earlier = json!({"earlier": {"channel_id": "300", "message_id": "555"}});
    std::fs::write(&records, serde_json::to_vec(&earlier)?)?;
    let mut discord = Substitute::start(succeed()).await?;
    discord.module = trapping_module(directory.path()).await?;
    let accepted = now() - 60;
    let made = || tools(None, &[], &[("bash", "make", true)]);
    discord.record(vec![
        running(
            "working",
            "300",
            accepted,
            Some(tools(None, &[("bash", "make")], &[])),
        ),
        ended(
            "earlier",
            "300",
            accepted,
            TurnState::Completed,
            Some(made()),
            None,
        ),
        ended(
            "fresh",
            "300",
            accepted,
            TurnState::Completed,
            Some(made()),
            None,
        ),
    ]);
    discord.deliver(answer(1, "earlier", "earlier", TurnState::Completed));
    discord.deliver(answer(2, "fresh", "fresh", TurnState::Completed));
    let mut provider = discord.provider(discord.http(false), config(false), &records);
    discord
        .wait_for(&Method::POST, "/channels/300/messages", 2)
        .await?;
    // Reads after both answers were recorded settle both jobs.
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    provider.stop().await?;
    // The answers went; no card was made, edited or removed.
    let messages = discord.matching(&Method::POST, "/channels/300/messages");
    assert_eq!(messages.len(), 2, "{messages:#?}");
    assert!(messages.iter().all(|seen| !seen.is_card()), "{messages:#?}");
    assert!(discord.matching(&Method::PATCH, "/channels").is_empty());
    assert!(discord.matching(&Method::DELETE, "/channels").is_empty());
    let settled = lock(&discord.core)
        .jobs
        .iter()
        .flatten()
        .filter(|job| job.settled().is_some())
        .count();
    assert_eq!(settled, 2);
    // The earlier card's record stays for the next start to finish.
    assert_eq!(recorded(&records)?, earlier);
    Ok(())
}

mod controls;
mod typing;
