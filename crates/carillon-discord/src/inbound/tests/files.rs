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
use tokio::{net::TcpListener, sync::mpsc};

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
    submissions: Vec<ExternalEvent>,
    /// The staged bytes core found for each submission.
    staged: Vec<Vec<Vec<u8>>>,
    /// Everything that happened, in order: submissions and Discord calls.
    trace: Vec<String>,
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
                    let known = core.lock().is_ok_and(|core| core.known.contains(&event_id));
                    Json(if known {
                        EventStatus::Accepted(receipt(&event_id, true))
                    } else {
                        EventStatus::Unknown
                    })
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
                    Json(
                        json!({"id": message, "type": 11, "parent_id": channel, "guild_id": "100"}),
                    )
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

fn submit(core: &Shared, staging: &std::path::Path, event: &ExternalEvent) -> Response {
    let Ok(mut core) = core.lock() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    core.trace.push(format!("submit {}", event.event_id));
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
    let ingress = Arc::new(crate::inbound::Ingress {
        downloads: crate::attachments::Downloads::open(world.staging.clone(), config.attachments)?
            .with_fixture_cdn(cdn),
        core: test_client(format!("http://{}", world.address)),
        participation: Arc::new(Participation::load(
            world.staging.with_file_name("participation.json"),
        )?),
        http: world.http.clone(),
        config,
    });
    let (send, receive) = mpsc::channel(lanes::MAX_PENDING);
    for message in messages {
        send.send(message).await?;
    }
    drop(send);
    tokio::time::timeout(Duration::from_secs(20), lanes::dispatch(ingress, receive)).await?;
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
        // Accepted, then shown as queued; staging is released.
        let submitted = position(&core.trace, "submit 10")?;
        let reacted = position(
            &core.trace,
            "PUT /api/v10/channels/300/messages/10/reactions/",
        )?;
        assert!(submitted < reacted);
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
