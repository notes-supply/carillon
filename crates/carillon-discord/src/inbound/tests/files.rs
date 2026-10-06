//! Messages with files through lanes, against a fake core, a Discord API
//! stand-in and a CDN stand-in.
use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path as AxumPath, State},
    http::{Method, StatusCode, Uri},
    response::{IntoResponse as _, Response},
    routing::{get, post},
};
use carillon_protocol::model::{EventReceipt, EventStatus, ExternalEvent, TurnState};
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::mpsc, time::Instant};

use crate::{
    attachments::tests::{Serve, attachment, serve_cdn},
    client::test_client,
    config::{DiscordActivation, DiscordConversation},
    conversations::{IncomingMessage, Participation, SourceAttachment, tests::discord_config},
    inbound::{QueuedMessage, lanes},
};

/// How the fake core answers submissions, in order; then it accepts.
#[derive(Clone, Copy)]
enum Answer {
    Accept,
    Status(StatusCode),
}

#[derive(Default)]
struct Core {
    answers: VecDeque<Answer>,
    /// Event IDs core already accepted, as a replay would find them.
    known: Vec<String>,
    /// Event IDs whose submissions core never answers usefully.
    unavailable: Vec<String>,
    /// Events core received although their submissions failed, with how
    /// many status lookups after the first submission still find them
    /// capturing before they are accepted.
    received: HashMap<String, usize>,
    /// Whether Discord fails every thread creation, as during an outage.
    threads_fail: bool,
    submissions: Vec<ExternalEvent>,
    /// The staged bytes core found for each submission.
    staged: Vec<Vec<Vec<u8>>>,
    /// Everything that happened, in order: submissions, status lookups and
    /// Discord calls.
    trace: Vec<String>,
    /// When each submission and status lookup happened.
    times: Vec<(String, Instant)>,
}

type Shared = Arc<Mutex<Core>>;

struct World {
    core: Shared,
    staging: PathBuf,
    http: Arc<twilight_http::Client>,
    address: std::net::SocketAddr,
    _directory: tempfile::TempDir,
}

async fn world(answers: Vec<Answer>, known: Vec<String>) -> Result<World> {
    let directory = tempfile::tempdir()?;
    let staging = directory.path().join("incoming");
    let core = Arc::new(Mutex::new(Core {
        answers: answers.into(),
        known,
        ..Core::default()
    }));
    let reading = staging.clone();
    let app = Router::new()
        .route(
            "/v1/events",
            post(
                move |State(core): State<Shared>, Json(event): Json<ExternalEvent>| {
                    let staging = reading.clone();
                    async move { submit(&core, &staging, &event) }
                },
            ),
        )
        .route(
            "/v1/events/{event_id}",
            get(
                |State(core): State<Shared>, AxumPath(event_id): AxumPath<String>| async move {
                    status(&core, &event_id)
                },
            ),
        )
        .route(
            "/api/v10/channels/{channel}/messages/{message}/threads",
            post(
                |State(core): State<Shared>,
                 AxumPath((channel, message)): AxumPath<(String, String)>,
                 body: Bytes| async move {
                    record(
                        &core,
                        format!("thread {message} {}", String::from_utf8_lossy(&body)),
                    );
                    if core.lock().is_ok_and(|core| core.threads_fail) {
                        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                    }
                    Json(
                        json!({"id": message, "type": 11, "parent_id": channel, "guild_id": "100"}),
                    )
                    .into_response()
                },
            ),
        )
        .fallback(
            |State(core): State<Shared>, method: Method, uri: Uri, body: Bytes| async move {
                record(
                    &core,
                    format!("{method} {uri} {}", String::from_utf8_lossy(&body)),
                );
                if method == Method::GET && uri.path() == "/api/v10/channels/777" {
                    // An ordinary channel that is not a route.
                    return Json(json!({"id": "777", "type": 0, "guild_id": "100"}))
                        .into_response();
                }
                if method == Method::GET && uri.path().starts_with("/api/v10/channels/") {
                    // No existing thread for a root message.
                    return (
                        StatusCode::NOT_FOUND,
                        Json(json!({"code": 10003, "message": "Unknown Channel"})),
                    )
                        .into_response();
                }
                StatusCode::NO_CONTENT.into_response()
            },
        )
        .with_state(core.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    tokio::spawn(async move { axum::serve(listener, app).await });
    let http = Arc::new(
        twilight_http::Client::builder()
            .token("test-only".to_owned())
            .proxy(address.to_string(), true)
            .ratelimiter(None)
            .build(),
    );
    Ok(World {
        core,
        staging,
        http,
        address,
        _directory: directory,
    })
}

fn record(core: &Shared, entry: String) {
    if let Ok(mut core) = core.lock() {
        core.trace.push(entry);
    }
}

fn receipt(event_id: &str, duplicate: bool) -> EventReceipt {
    EventReceipt {
        event_id: event_id.to_owned(),
        turn_id: format!("turn-{event_id}"),
        state: TurnState::Queued,
        duplicate,
    }
}

fn status(core: &Shared, event_id: &str) -> Response {
    let Ok(mut core) = core.lock() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let entry = format!("status {event_id}");
    core.trace.push(entry.clone());
    core.times.push((entry, Instant::now()));
    let submitted = core
        .submissions
        .iter()
        .any(|event| event.event_id == event_id);
    if submitted && let Some(capturing) = core.received.get_mut(event_id) {
        if *capturing > 0 {
            *capturing -= 1;
            return Json(EventStatus::Capturing).into_response();
        }
        core.known.push(event_id.to_owned());
    }
    Json(if core.known.iter().any(|known| known == event_id) {
        EventStatus::Accepted(receipt(event_id, true))
    } else {
        EventStatus::Unknown
    })
    .into_response()
}

fn submit(core: &Shared, staging: &std::path::Path, event: &ExternalEvent) -> Response {
    let Ok(mut core) = core.lock() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let entry = format!("submit {}", event.event_id);
    core.trace.push(entry.clone());
    core.times.push((entry, Instant::now()));
    let staged = event
        .attachments
        .as_ref()
        .map(|batch| {
            (0..batch.files.len())
                .map(|ordinal| {
                    std::fs::read(staging.join(&batch.batch).join(ordinal.to_string()))
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default();
    core.staged.push(staged);
    core.submissions.push(event.clone());
    if core.unavailable.contains(&event.event_id) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "fake core is unavailable"})),
        )
            .into_response();
    }
    match core.answers.pop_front().unwrap_or(Answer::Accept) {
        Answer::Accept => {
            let duplicate = core.known.contains(&event.event_id);
            core.known.push(event.event_id.clone());
            Json(receipt(&event.event_id, duplicate)).into_response()
        }
        Answer::Status(status) => {
            (status, Json(json!({"error": "fake core refusal"}))).into_response()
        }
    }
}

fn message(
    id: &str,
    channel: &str,
    user: &str,
    content: &str,
    files: Vec<SourceAttachment>,
) -> QueuedMessage {
    QueuedMessage {
        request: IncomingMessage {
            event_id: id.to_owned(),
            guild_id: Some("100".to_owned()),
            user_id: user.to_owned(),
            channel_id: channel.to_owned(),
            parent_channel_id: None,
            content: content.to_owned(),
            attachments: files,
        },
        native: Value::Null,
    }
}

/// Run lanes over `messages` until all settle.
async fn run(
    world: &World,
    config: crate::config::DiscordConfig,
    cdn: u16,
    messages: Vec<QueuedMessage>,
) -> Result<()> {
    run_for(world, config, cdn, messages, Duration::from_secs(20)).await
}

fn ingress(
    world: &World,
    config: crate::config::DiscordConfig,
    cdn: u16,
) -> Result<Arc<crate::inbound::Ingress>> {
    Ok(Arc::new(crate::inbound::Ingress {
        downloads: Arc::new(
            crate::attachments::Downloads::open(world.staging.clone(), config.attachments)?
                .with_fixture_cdn(cdn),
        ),
        core: test_client(format!("http://{}", world.address)),
        participation: Arc::new(Participation::load(
            world.staging.with_file_name("participation.json"),
        )?),
        http: world.http.clone(),
        config,
        refusals: Arc::new(tokio::sync::Semaphore::new(lanes::REFUSALS)),
        stopping: tokio_util::sync::CancellationToken::new(),
    }))
}

/// Run lanes over `messages` until all settle, failing after `limit`.
async fn run_for(
    world: &World,
    config: crate::config::DiscordConfig,
    cdn: u16,
    messages: Vec<QueuedMessage>,
    limit: Duration,
) -> Result<()> {
    let ingress = ingress(world, config, cdn)?;
    let (send, receive) = mpsc::channel(lanes::MAX_PENDING);
    for message in messages {
        send.send(message).await?;
    }
    drop(send);
    tokio::time::timeout(limit, lanes::dispatch(ingress, receive)).await?;
    Ok(())
}

fn channel_config() -> crate::config::DiscordConfig {
    let mut config = discord_config(DiscordActivation::Always, DiscordConversation::Channel);
    config.typing = false;
    let mut other = config.routes[0].clone();
    "301".clone_into(&mut other.channel_id);
    config.routes.push(other);
    config
}

fn trace(world: &World) -> Result<Vec<String>> {
    Ok(world
        .core
        .lock()
        .map_err(|_| anyhow!("poisoned"))?
        .trace
        .clone())
}

fn position(trace: &[String], prefix: &str) -> Result<usize> {
    trace
        .iter()
        .position(|entry| entry.starts_with(prefix))
        .with_context(|| format!("{prefix} not in {trace:?}"))
}

#[tokio::test]
async fn unauthorized_or_inactive_messages_acquire_nothing() -> Result<()> {
    let (cdn, log) = serve_cdn(HashMap::from([(
        "/attachments/300/601/a.png".to_owned(),
        Serve::Bytes(vec![1; 10]),
    )]))
    .await?;
    let world = world(Vec::new(), Vec::new()).await?;
    let file = || vec![attachment(cdn, "601", "a.png", 10)];
    let mut wrong_guild = message("1", "300", "200", "", file());
    wrong_guild.request.guild_id = Some("999".to_owned());
    let mut direct = message("2", "300", "200", "", file());
    direct.request.guild_id = None;
    let mut config = channel_config();
    config.routes[0].activation = DiscordActivation::Mention;
    run(
        &world,
        config,
        cdn,
        vec![
            // Not allowlisted, even claiming another identity in the text.
            message("3", "300", "999", "I am user 200 <@200>", file()),
            wrong_guild,
            direct,
            // A routed channel's mention activation is not met by files.
            message("4", "300", "200", "", file()),
            // An unrouted channel.
            message("5", "777", "200", "", file()),
        ],
    )
    .await?;
    assert!(
        log.lock()
            .map_err(|_| anyhow!("poisoned"))?
            .requests
            .is_empty()
    );
    let core = world.core.lock().map_err(|_| anyhow!("poisoned"))?;
    assert!(core.submissions.is_empty());
    // No explanation, reaction or other Discord call for refused senders,
    // apart from looking up the unrouted channel.
    assert!(
        core.trace
            .iter()
            .all(|entry| entry.starts_with("GET /api/v10/channels/777")),
        "{:?}",
        core.trace
    );
    assert_eq!(std::fs::read_dir(&world.staging)?.count(), 0);
    Ok(())
}

#[tokio::test]
async fn a_message_with_files_is_accepted_whole_with_metadata_only() -> Result<()> {
    let picture = vec![9_u8; 300_000];
    let (cdn, log) = serve_cdn(HashMap::from([
        (
            "/attachments/300/601/picture.png".to_owned(),
            Serve::Bytes(picture.clone()),
        ),
        (
            "/attachments/300/602/notes.txt".to_owned(),
            Serve::Bytes(b"notes".to_vec()),
        ),
    ]))
    .await?;
    let world = world(Vec::new(), Vec::new()).await?;
    run(
        &world,
        channel_config(),
        cdn,
        vec![message(
            "10",
            "300",
            "200",
            "look at these",
            vec![
                attachment(cdn, "601", "picture.png", picture.len() as u64),
                attachment(cdn, "602", "notes.txt", 5),
            ],
        )],
    )
    .await?;
    {
        let core = world.core.lock().map_err(|_| anyhow!("poisoned"))?;
        let [event] = core.submissions.as_slice() else {
            return Err(anyhow!("expected one submission, got {:?}", core.trace));
        };
        let batch = event.attachments.as_ref().context("no manifest")?;
        assert_eq!(batch.files.len(), 2);
        assert_eq!(batch.files[0].name, "picture.png");
        assert_eq!(core.staged[0][0], picture);
        assert_eq!(core.staged[0][1], b"notes");
        // The event is metadata: no bytes, no signed addresses, no paths.
        let wire = serde_json::to_string(event)?;
        assert!(wire.len() < 2_000, "{} bytes", wire.len());
        assert!(!wire.contains("hm=") && !wire.contains("127.0.0.1") && !wire.contains("incoming"));
        // Shown as processing once downloaded, before core can deliver an
        // answer that replaces the reaction; staging is then released.
        let submitted = position(&core.trace, "submit 10")?;
        let reacted = position(
            &core.trace,
            "PUT /api/v10/channels/300/messages/10/reactions/",
        )?;
        assert!(reacted < submitted);
        assert_eq!(
            log.lock().map_err(|_| anyhow!("poisoned"))?.requests.len(),
            2
        );
    }
    for _ in 0..200 {
        if std::fs::read_dir(&world.staging)?.count() == 0 {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    Err(anyhow!("staging was not removed after acceptance"))
}

#[tokio::test]
async fn a_failed_file_refuses_the_whole_message_with_a_quiet_explanation() -> Result<()> {
    let (cdn, _) = serve_cdn(HashMap::from([
        (
            "/attachments/300/601/ok.bin".to_owned(),
            Serve::Bytes(vec![1; 100]),
        ),
        (
            "/attachments/300/602/bad.bin".to_owned(),
            Serve::Status(400),
        ),
    ]))
    .await?;
    let world = world(Vec::new(), Vec::new()).await?;
    run(
        &world,
        channel_config(),
        cdn,
        vec![message(
            "20",
            "300",
            "200",
            "two files",
            vec![
                attachment(cdn, "601", "ok.bin", 100),
                attachment(cdn, "602", "bad.bin", 1),
            ],
        )],
    )
    .await?;
    let trace = trace(&world)?;
    assert!(
        !trace.iter().any(|entry| entry.starts_with("submit")),
        "{trace:?}"
    );
    let explanation = &trace[position(&trace, "POST /api/v10/channels/300/messages ")?];
    assert!(explanation.contains("not accepted"), "{explanation}");
    assert!(
        explanation.contains("\"allowed_mentions\":{\"parse\":[]"),
        "{explanation}"
    );
    assert!(
        explanation.contains("\"message_id\":\"20\""),
        "{explanation}"
    );
    // Failure is marked with the configured failure reaction.
    position(
        &trace,
        "PUT /api/v10/channels/300/messages/20/reactions/%E2%9D%8C",
    )?;
    Ok(())
}

#[tokio::test]
async fn retries_reuse_downloaded_files_and_storage_refusals_are_explained_once() -> Result<()> {
    let (cdn, log) = serve_cdn(HashMap::from([(
        "/attachments/300/601/a.bin".to_owned(),
        Serve::Bytes(vec![5; 64]),
    )]))
    .await?;
    let world = world(
        vec![
            Answer::Status(StatusCode::SERVICE_UNAVAILABLE),
            Answer::Status(StatusCode::INTERNAL_SERVER_ERROR),
            Answer::Accept,
            Answer::Status(StatusCode::INSUFFICIENT_STORAGE),
        ],
        Vec::new(),
    )
    .await?;
    run(
        &world,
        channel_config(),
        cdn,
        vec![
            message(
                "30",
                "300",
                "200",
                "",
                vec![attachment(cdn, "601", "a.bin", 64)],
            ),
            message(
                "31",
                "300",
                "200",
                "",
                vec![attachment(cdn, "601", "a.bin", 64)],
            ),
        ],
    )
    .await?;
    let core = world.core.lock().map_err(|_| anyhow!("poisoned"))?;
    let batches = core
        .submissions
        .iter()
        .map(|event| {
            (
                event.event_id.clone(),
                event.attachments.as_ref().map(|batch| batch.batch.clone()),
            )
        })
        .collect::<Vec<_>>();
    // Three attempts with one download and one batch, then one refusal.
    assert_eq!(batches.len(), 4, "{batches:?}");
    assert!(batches[..3].iter().all(|batch| *batch == batches[0]));
    assert!(core.staged[..3].iter().all(|files| files[0] == vec![5; 64]));
    assert_eq!(batches[3].0, "31");
    assert_eq!(
        log.lock().map_err(|_| anyhow!("poisoned"))?.requests.len(),
        2
    );
    let explanation = &core.trace[position(&core.trace, "POST /api/v10/channels/300/messages ")?];
    assert!(explanation.contains("storage is full"), "{explanation}");
    Ok(())
}

#[tokio::test]
async fn a_message_core_already_accepted_is_not_downloaded_again() -> Result<()> {
    let (cdn, log) = serve_cdn(HashMap::from([(
        "/attachments/300/601/a.bin".to_owned(),
        Serve::Bytes(vec![5; 64]),
    )]))
    .await?;
    let world = world(Vec::new(), vec!["40".to_owned()]).await?;
    run(
        &world,
        channel_config(),
        cdn,
        vec![message(
            "40",
            "300",
            "200",
            "",
            vec![attachment(cdn, "601", "a.bin", 64)],
        )],
    )
    .await?;
    assert!(
        log.lock()
            .map_err(|_| anyhow!("poisoned"))?
            .requests
            .is_empty()
    );
    assert!(
        world
            .core
            .lock()
            .map_err(|_| anyhow!("poisoned"))?
            .submissions
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn a_slow_transfer_keeps_its_conversation_in_order_without_stopping_others() -> Result<()> {
    let (cdn, _) = serve_cdn(HashMap::from([(
        "/attachments/300/601/slow.bin".to_owned(),
        Serve::Slow(Duration::from_millis(500), vec![1; 32]),
    )]))
    .await?;
    let world = world(Vec::new(), Vec::new()).await?;
    run(
        &world,
        channel_config(),
        cdn,
        vec![
            message(
                "50",
                "300",
                "200",
                "slow file",
                vec![attachment(cdn, "601", "slow.bin", 32)],
            ),
            message("51", "300", "200", "after the file", Vec::new()),
            message("52", "301", "200", "another conversation", Vec::new()),
        ],
    )
    .await?;
    let trace = trace(&world)?;
    let (slow, after, other) = (
        position(&trace, "submit 50")?,
        position(&trace, "submit 51")?,
        position(&trace, "submit 52")?,
    );
    assert!(slow < after, "{trace:?}");
    assert!(other < slow, "{trace:?}");
    Ok(())
}

#[tokio::test]
async fn a_full_conversation_lane_refuses_with_an_explanation() -> Result<()> {
    let (cdn, _) = serve_cdn(HashMap::from([(
        "/attachments/300/601/slow.bin".to_owned(),
        Serve::Slow(Duration::from_millis(300), vec![1; 8]),
    )]))
    .await?;
    let world = world(Vec::new(), Vec::new()).await?;
    let mut messages = vec![message(
        "100",
        "300",
        "200",
        "",
        vec![attachment(cdn, "601", "slow.bin", 8)],
    )];
    messages.extend((1..=lanes::MAX_PER_LANE).map(|index| {
        message(
            &format!("{}", 100 + index),
            "300",
            "200",
            "text",
            Vec::new(),
        )
    }));
    run(&world, channel_config(), cdn, messages).await?;
    let trace = trace(&world)?;
    let overflow = 100 + lanes::MAX_PER_LANE;
    assert!(!trace.contains(&format!("submit {overflow}")), "{trace:?}");
    let explanation = trace
        .iter()
        .find(|entry| entry.contains("too busy"))
        .context("no overload explanation")?;
    assert!(
        explanation.contains(&format!("\"message_id\":\"{overflow}\"")),
        "{explanation}"
    );
    assert_eq!(
        trace
            .iter()
            .filter(|entry| entry.starts_with("submit"))
            .count(),
        lanes::MAX_PER_LANE
    );
    Ok(())
}

#[tokio::test]
async fn a_file_only_root_message_starts_a_thread_named_after_its_file() -> Result<()> {
    let (cdn, _) = serve_cdn(HashMap::from([(
        "/attachments/300/601/quarterly%20report.pdf".to_owned(),
        Serve::Bytes(b"%PDF".to_vec()),
    )]))
    .await?;
    let world = world(Vec::new(), Vec::new()).await?;
    let mut config = discord_config(DiscordActivation::Always, DiscordConversation::Thread);
    config.typing = false;
    run(
        &world,
        config,
        cdn,
        vec![message(
            "60",
            "300",
            "200",
            "",
            vec![attachment(cdn, "601", "quarterly%20report.pdf", 4)],
        )],
    )
    .await?;
    let trace = trace(&world)?;
    let thread = &trace[position(&trace, "thread 60 ")?];
    assert!(
        thread.contains("\"name\":\"quarterly%20report.pdf\""),
        "{thread}"
    );
    let core = world.core.lock().map_err(|_| anyhow!("poisoned"))?;
    assert_eq!(core.submissions[0].delivery_target_id, "60");
    assert_eq!(core.submissions[0].content, "");
    Ok(())
}

#[tokio::test]
async fn a_new_thread_holds_its_lane_until_its_first_message_settles() -> Result<()> {
    let (cdn, log) = serve_cdn(HashMap::from([(
        "/attachments/300/601/slow.bin".to_owned(),
        Serve::Slow(Duration::from_millis(300), vec![1; 16]),
    )]))
    .await?;
    let world = world(Vec::new(), Vec::new()).await?;
    let mut config = discord_config(DiscordActivation::Always, DiscordConversation::Thread);
    config.typing = false;
    let root = || {
        message(
            "70",
            "300",
            "200",
            "",
            vec![attachment(cdn, "601", "slow.bin", 16)],
        )
    };
    run(
        &world,
        config,
        cdn,
        vec![
            root(),
            // Sent in the new thread while its first message is still downloading.
            message("71", "70", "200", "in the thread", Vec::new()),
            // The gateway delivering the first message again.
            root(),
        ],
    )
    .await?;
    let trace = trace(&world)?;
    let (thread, first, child) = (
        position(&trace, "thread 70 ")?,
        position(&trace, "submit 70")?,
        position(&trace, "submit 71")?,
    );
    assert!(thread < first && first < child, "{trace:?}");
    assert_eq!(
        trace.iter().filter(|entry| *entry == "submit 70").count(),
        1,
        "{trace:?}"
    );
    assert_eq!(
        log.lock().map_err(|_| anyhow!("poisoned"))?.requests.len(),
        1
    );
    let core = world.core.lock().map_err(|_| anyhow!("poisoned"))?;
    assert_eq!(core.submissions[1].audience_id, "discord-100-70");
    Ok(())
}

#[tokio::test]
async fn overload_replies_only_to_messages_admission_would_activate() -> Result<()> {
    let world = world(Vec::new(), Vec::new()).await?;
    let mut config = channel_config();
    config.routes[0].activation = DiscordActivation::Mention;
    let ingress = Arc::new(crate::inbound::Ingress {
        downloads: Arc::new(crate::attachments::Downloads::open(
            world.staging.clone(),
            config.attachments,
        )?),
        core: test_client(format!("http://{}", world.address)),
        participation: Arc::new(Participation::load(
            world.staging.with_file_name("participation.json"),
        )?),
        http: world.http.clone(),
        config,
        refusals: Arc::new(tokio::sync::Semaphore::new(lanes::REFUSALS)),
        stopping: tokio_util::sync::CancellationToken::new(),
    });
    for ignored in [
        // A mention route, not mentioned.
        message("80", "300", "200", "just chatting", Vec::new()),
        // A thread the provider has not seen.
        message("81", "999", "200", "hello", Vec::new()),
        // Not allowlisted.
        message("82", "301", "999", "hello", Vec::new()),
    ] {
        lanes::refuse_overloaded(&ingress, ignored.request);
    }
    lanes::refuse_overloaded(
        &ingress,
        message("83", "301", "200", "hello", Vec::new()).request,
    );
    for _ in 0..200 {
        let trace = trace(&world)?;
        if trace
            .iter()
            .any(|entry| entry.contains("/messages/83/reactions/"))
        {
            assert!(trace.iter().all(|entry| entry.contains("83")), "{trace:?}");
            assert!(
                trace.iter().any(|entry| entry.contains("too busy")),
                "{trace:?}"
            );
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Err(anyhow!("no overload refusal for an activated message"))
}

#[tokio::test]
async fn a_full_gateway_queue_refuses_only_messages_admission_would_activate() -> Result<()> {
    let world = world(Vec::new(), Vec::new()).await?;
    let mut config = channel_config();
    config.routes[0].activation = DiscordActivation::Mention;
    let ingress = ingress(&world, config, 0)?;
    // The lanes' queue is full: one message waits in it.
    let (send, _receive) = mpsc::channel(1);
    crate::application::queue_or_refuse(
        &send,
        &ingress,
        message("90", "301", "200", "first", Vec::new()),
    )?;
    for queued in [
        // A mention route, not mentioned: ordinary chatter.
        message("91", "300", "200", "just chatting", Vec::new()),
        // A channel that is no route.
        message("92", "999", "200", "hello", Vec::new()),
        // A message the agent would answer.
        message("93", "301", "200", "hello", Vec::new()),
    ] {
        crate::application::queue_or_refuse(&send, &ingress, queued)?;
    }
    for _ in 0..200 {
        let trace = trace(&world)?;
        if trace
            .iter()
            .any(|entry| entry.contains("/messages/93/reactions/"))
        {
            assert!(trace.iter().all(|entry| entry.contains("93")), "{trace:?}");
            assert!(explanation(&trace, "93")?.contains("too busy"), "{trace:?}");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Err(anyhow!("no overload refusal for an activated message"))
}

/// Past the preparation deadline, in a paused clock's virtual time.
const PAST_DEADLINE: Duration = Duration::from_mins(45);
const DEADLINE: Duration = Duration::from_mins(30);

fn script(world: &World, change: impl FnOnce(&mut Core)) -> Result<()> {
    change(&mut *world.core.lock().map_err(|_| anyhow!("poisoned"))?);
    Ok(())
}

/// When each `entry` happened.
fn times(world: &World, entry: &str) -> Result<Vec<Instant>> {
    Ok(world
        .core
        .lock()
        .map_err(|_| anyhow!("poisoned"))?
        .times
        .iter()
        .filter(|(seen, _)| seen == entry)
        .map(|(_, at)| *at)
        .collect())
}

/// The explanation sent in reply to `message`.
fn explanation(trace: &[String], message: &str) -> Result<String> {
    trace
        .iter()
        .find(|entry| {
            entry.starts_with("POST /api/v10/channels/")
                && entry.contains(&format!("\"message_id\":\"{message}\""))
        })
        .cloned()
        .with_context(|| format!("no explanation for {message} in {trace:?}"))
}

async fn released(world: &World) -> Result<()> {
    for _ in 0..200 {
        if std::fs::read_dir(&world.staging)?.count() == 0 {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    Err(anyhow!("staging was not released"))
}

#[tokio::test(start_paused = true)]
async fn preparation_that_outlasts_its_deadline_ends_without_submitting() -> Result<()> {
    let (cdn, _) = serve_cdn(HashMap::from([(
        "/attachments/300/601/a.bin".to_owned(),
        Serve::Bytes(vec![1; 8]),
    )]))
    .await?;
    let world = world(Vec::new(), Vec::new()).await?;
    // The files are staged, but Discord keeps failing to create the thread.
    script(&world, |core| core.threads_fail = true)?;
    let mut config = discord_config(DiscordActivation::Always, DiscordConversation::Thread);
    config.typing = false;
    let started = Instant::now();
    run_for(
        &world,
        config,
        cdn,
        vec![message(
            "85",
            "300",
            "200",
            "",
            vec![attachment(cdn, "601", "a.bin", 8)],
        )],
        PAST_DEADLINE,
    )
    .await?;
    let elapsed = started.elapsed();
    assert!(
        elapsed > Duration::from_mins(29) && elapsed < Duration::from_mins(31),
        "{elapsed:?}"
    );
    let trace = trace(&world)?;
    assert!(position(&trace, "thread 85").is_ok());
    assert!(
        !trace.iter().any(|entry| entry.starts_with("submit")),
        "{trace:?}"
    );
    assert!(explanation(&trace, "85")?.contains("in time"));
    position(
        &trace,
        "PUT /api/v10/channels/300/messages/85/reactions/%E2%9D%8C",
    )?;
    released(&world).await
}

#[tokio::test(start_paused = true)]
async fn expired_submissions_are_settled_by_core_before_their_lane_moves_on() -> Result<()> {
    let (cdn, _) = serve_cdn(HashMap::from([(
        "/attachments/300/601/a.bin".to_owned(),
        Serve::Bytes(vec![1; 8]),
    )]))
    .await?;
    let world = world(Vec::new(), Vec::new()).await?;
    // Core never answers either message's submissions in time. It turns out
    // to have received the first, still capturing it for a while; the second
    // it never received.
    script(&world, |core| {
        core.unavailable = vec!["90".to_owned(), "95".to_owned()];
        core.received.insert("90".to_owned(), 2);
    })?;
    let started = Instant::now();
    run_for(
        &world,
        channel_config(),
        cdn,
        vec![
            message(
                "90",
                "300",
                "200",
                "",
                vec![attachment(cdn, "601", "a.bin", 8)],
            ),
            message("91", "300", "200", "after the file", Vec::new()),
            message(
                "95",
                "301",
                "200",
                "",
                vec![attachment(cdn, "601", "a.bin", 8)],
            ),
        ],
        PAST_DEADLINE,
    )
    .await?;
    let deadline = started + DEADLINE;
    for event in ["90", "95"] {
        let submitted = times(&world, &format!("submit {event}"))?;
        // Retried with backoff, but no attempt started after the deadline;
        // after it, only core is asked.
        assert!(submitted.len() > 1, "{event}: {submitted:?}");
        assert!(submitted.iter().all(|at| *at < deadline), "{event}");
        let last = submitted.iter().max().copied();
        let asked = times(&world, &format!("status {event}"))?;
        assert!(asked.iter().any(|at| Some(*at) > last), "{event}");
    }
    // The first message's lane waited while core was still capturing it,
    // then moved on once core had accepted it.
    let capturing = world
        .core
        .lock()
        .map_err(|_| anyhow!("poisoned"))?
        .received
        .get("90")
        .copied();
    assert_eq!(capturing, Some(0));
    let settled = times(&world, "status 90")?.into_iter().max();
    let next = times(&world, "submit 91")?.into_iter().min();
    assert!(settled.is_some() && settled < next, "{settled:?} {next:?}");
    let trace = trace(&world)?;
    assert!(explanation(&trace, "90").is_err(), "{trace:?}");
    // The second was never accepted, and its sender is told.
    assert!(explanation(&trace, "95")?.contains("in time"));
    released(&world).await
}

#[tokio::test(start_paused = true)]
async fn a_submission_still_waiting_for_a_slot_at_the_deadline_is_never_made() -> Result<()> {
    let world = world(Vec::new(), Vec::new()).await?;
    let config = channel_config();
    let ingress = ingress(&world, config.clone(), 1)?;
    // Every slot for active work is busy, as with long transfers elsewhere.
    let mut busy = Vec::new();
    for _ in 0..config.attachments.concurrent_messages {
        busy.push(ingress.downloads.active().await);
    }
    let event = ExternalEvent {
        event_id: "99".to_owned(),
        agent_id: "default".to_owned(),
        user_id: "200".to_owned(),
        audience_id: "discord-100-300".to_owned(),
        delivery_target_id: "300".to_owned(),
        feedback_target_id: "300:99".to_owned(),
        content: String::new(),
        native: Value::Null,
        attachments: None,
    };
    let started = Instant::now();
    let refusal = crate::inbound::submit_files(&ingress, &event, started + DEADLINE)
        .await
        .err()
        .context("submitted after the deadline")?;
    assert!(refusal.0.contains("in time"), "{}", refusal.0);
    assert!(started.elapsed() <= DEADLINE + Duration::from_secs(1));
    // Nothing was sent, so nothing is ambiguous: core is not even asked.
    assert!(trace(&world)?.is_empty());
    drop(busy);
    Ok(())
}

#[tokio::test]
async fn stopping_settles_what_core_may_hold_and_drops_the_rest() -> Result<()> {
    let (cdn, log) = serve_cdn(HashMap::from([
        (
            "/attachments/300/601/a.bin".to_owned(),
            Serve::Bytes(vec![1; 8]),
        ),
        ("/attachments/300/602/stalled".to_owned(), Serve::Stall),
    ]))
    .await?;
    let world = world(Vec::new(), Vec::new()).await?;
    // Core receives the first message's files, but its answer is lost and
    // it is still capturing them for two more lookups.
    script(&world, |core| {
        core.unavailable = vec!["110".to_owned()];
        core.received.insert("110".to_owned(), 2);
    })?;
    let ingress = ingress(&world, channel_config(), cdn)?;
    let stopping = ingress.stopping.clone();
    let (send, receive) = mpsc::channel(lanes::MAX_PENDING);
    for queued in [
        message(
            "110",
            "300",
            "200",
            "",
            vec![attachment(cdn, "601", "a.bin", 8)],
        ),
        message("111", "300", "200", "after the file", Vec::new()),
        message(
            "112",
            "301",
            "200",
            "",
            vec![attachment(cdn, "602", "stalled", 100)],
        ),
    ] {
        send.send(queued).await?;
    }
    let dispatcher = tokio::spawn(lanes::dispatch(ingress, receive));
    // Stop once the first message is with core and the other is downloading.
    for _ in 0..2000 {
        let downloading = log
            .lock()
            .map_err(|_| anyhow!("poisoned"))?
            .requests
            .iter()
            .any(|request| request.contains("stalled"));
        if downloading && trace(&world)?.iter().any(|entry| entry == "submit 110") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    stopping.cancel();
    // The stop, not the gateway closing, ends dispatch.
    tokio::time::timeout(Duration::from_secs(20), dispatcher).await??;
    let trace = trace(&world)?;
    // What core may have received was settled with core first.
    let settled = world
        .core
        .lock()
        .map_err(|_| anyhow!("poisoned"))?
        .known
        .contains(&"110".to_owned());
    assert!(settled, "{trace:?}");
    assert!(explanation(&trace, "110").is_err(), "{trace:?}");
    // The waiting message is dropped, and the interrupted download is not
    // submitted; its sender is told.
    assert!(
        !trace
            .iter()
            .any(|entry| entry == "submit 111" || entry == "submit 112"),
        "{trace:?}"
    );
    let stopped = explanation(&trace, "112")?;
    assert!(stopped.contains("restarting"), "{stopped}");
    released(&world).await?;
    drop(send);
    Ok(())
}

#[tokio::test]
async fn stopping_settles_text_submissions_and_tells_senders_what_core_lacks() -> Result<()> {
    let (cdn, _) = serve_cdn(HashMap::new()).await?;
    let world = world(Vec::new(), Vec::new()).await?;
    // Core answers no submission of either text: it accepted the first all
    // the same, while the second never reached it.
    script(&world, |core| {
        core.unavailable = vec!["130".to_owned(), "131".to_owned()];
        core.received.insert("130".to_owned(), 0);
    })?;
    let ingress = ingress(&world, channel_config(), cdn)?;
    let stopping = ingress.stopping.clone();
    let (send, receive) = mpsc::channel(lanes::MAX_PENDING);
    for queued in [
        message("130", "300", "200", "accepted", Vec::new()),
        message("131", "301", "200", "never received", Vec::new()),
        message("132", "300", "200", "waiting", Vec::new()),
    ] {
        send.send(queued).await?;
    }
    let dispatcher = tokio::spawn(lanes::dispatch(ingress, receive));
    for _ in 0..2000 {
        let trace = trace(&world)?;
        if ["submit 130", "submit 131"]
            .iter()
            .all(|submitted| trace.iter().any(|entry| entry == submitted))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    stopping.cancel();
    tokio::time::timeout(Duration::from_secs(20), dispatcher).await??;
    let trace = trace(&world)?;
    // Each text handed to core is settled by asking core: the accepted one
    // is left to its answer, the other's sender is told.
    position(&trace, "status 130")?;
    position(&trace, "status 131")?;
    assert!(explanation(&trace, "130").is_err(), "{trace:?}");
    let stopped = explanation(&trace, "131")?;
    assert!(stopped.contains("restarting"), "{stopped}");
    // The waiting message is dropped, as before.
    assert!(
        !trace.iter().any(|entry| entry == "submit 132"),
        "{trace:?}"
    );
    drop(send);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_conversation_attempt_stalled_in_discords_answer_is_bounded() -> Result<()> {
    // Looking up the message's thread, Discord answers but never finishes
    // the body, which Twilight's own request timeout does not cover.
    let (port, log) = serve_cdn(HashMap::from([
        (
            "/attachments/300/601/a.bin".to_owned(),
            Serve::Bytes(vec![1; 8]),
        ),
        ("/api/v10/channels/86".to_owned(), Serve::Stall),
    ]))
    .await?;
    let world = world(Vec::new(), Vec::new()).await?;
    let mut config = discord_config(DiscordActivation::Always, DiscordConversation::Thread);
    config.typing = false;
    let mut ingress = ingress(&world, config, port)?;
    Arc::get_mut(&mut ingress).context("shared ingress")?.http = Arc::new(
        twilight_http::Client::builder()
            .token("test-only".to_owned())
            .proxy(format!("127.0.0.1:{port}"), true)
            .ratelimiter(None)
            .build(),
    );
    let (send, receive) = mpsc::channel(1);
    send.send(message(
        "86",
        "300",
        "200",
        "",
        vec![attachment(port, "601", "a.bin", 8)],
    ))
    .await?;
    drop(send);
    let started = Instant::now();
    tokio::time::timeout(PAST_DEADLINE, lanes::dispatch(ingress, receive)).await?;
    let elapsed = started.elapsed();
    assert!(
        elapsed > Duration::from_mins(29) && elapsed < Duration::from_mins(31),
        "{elapsed:?}"
    );
    let requests = log
        .lock()
        .map_err(|_| anyhow!("poisoned"))?
        .requests
        .clone();
    // Each attempt was cut off and tried again; the sender was told.
    let attempts = requests
        .iter()
        .filter(|request| *request == "/api/v10/channels/86")
        .count();
    assert!(attempts > 1, "{requests:?}");
    assert!(
        requests
            .iter()
            .any(|request| request == "/api/v10/channels/300/messages")
    );
    assert!(
        !trace(&world)?
            .iter()
            .any(|entry| entry.starts_with("submit"))
    );
    released(&world).await
}
