use std::{fs, path::Path};

use anyhow::Context as _;
use axum::response::IntoResponse as _;
use tempfile::tempdir;
use tokio::sync::mpsc::error::TryRecvError;

use super::*;
use crate::{Settled, WorkerStep};
use carillon_protocol::{
    model::{DeliveryOutcome, JOB_READ_LIMIT, RuntimeTurnRequest, TurnState},
    turns::TurnOutcome,
};

/// A test installation: its registry, which cases change in memory as an
/// operator would edit and restart it, and core over its storage.
#[derive(Clone)]
struct Fixture {
    config: LoadedConfig,
    core: Core,
    client: Client,
}

impl Fixture {
    fn snapshot_store(&self) -> PathBuf {
        self.core.snapshot_store()
    }
}

fn installation(root: &Path) -> Result<Fixture> {
    use carillon_installation::config::{AgentConfig, Config, CoreConfig, hash_token};

    // Core tests use opaque provider IDs, without a Discord or Pi installation.
    let resources = root.join("resources");
    fs::create_dir(&resources)?;
    fs::write(resources.join("SYSTEM.md"), "test instructions")?;
    fs::create_dir(root.join("credentials"))?;
    let providers = [
        ("test-interface", ProviderRole::Interface),
        ("test-runtime", ProviderRole::Runtime),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (id, role))| {
        fs::write(
            root.join("credentials").join(format!("{id}.token")),
            "test-only",
        )?;
        Ok(Provider {
            id: id.to_owned(),
            role,
            version: "test".to_owned(),
            url: format!("http://127.0.0.1:{}", index + 2),
            token_hash: hash_token("test-only"),
            enabled: true,
        })
    })
    .collect::<Result<Vec<_>>>()?;
    let value = Config {
        data_dir: root.join("state"),
        core: CoreConfig {
            listen: "127.0.0.1:1".parse()?,
            operator_token_hash: hash_token("test-operator"),
            // Captures in these tests must not depend on the host's free space.
            attachments: carillon_installation::config::AttachmentLimits {
                free_space_mib: 0,
                ..carillon_installation::config::AttachmentLimits::default()
            },
        },
        providers,
        agents: vec![AgentConfig {
            id: "test-agent".to_owned(),
            name: "Test agent".to_owned(),
            runtime: "test-runtime".to_owned(),
            workspace: root.to_path_buf(),
            resources: Some(resources),
        }],
    };
    value.validate()?;
    let config = LoadedConfig {
        root: root.to_path_buf(),
        path: root.join("carillon.toml"),
        value,
    };
    Ok(Fixture {
        core: Core::create(core_options(&config))?,
        config,
        client: loopback::client().build()?,
    })
}

// These tests mutate configuration in memory to exercise ownership changes.
// Serve it as a restart would, without reopening storage: fault-injection
// tests deliberately alter its schema.
fn configured(fixture: &Fixture) -> AppState {
    let config = fixture.config.clone();
    AppState {
        core: fixture.core.reconfigured(core_options(&config)),
        registry: Arc::new(Registry::new(config.root.join("carillon.toml"), config)),
        client: fixture.client.clone(),
        workers: Arc::new(watch::Sender::new(Census::default())),
        pause: Arc::new(watch::Sender::new(None)),
    }
}

/// Hands `runtime_id` its oldest queued turn: the request it was handed, if any.
async fn hand_next(core: &Core, runtime_id: &str) -> Result<Option<RuntimeTurnRequest>> {
    let mut handed = None;
    core.dispatch_next(runtime_id, &CancellationToken::new(), async |request| {
        handed = Some(request);
        Ok(())
    })
    .await?;
    Ok(handed)
}

/// Settles a turn `runtime_id` holds as answered, with a reply to its input
/// and `activity` as its report.
fn answer(
    core: &Core,
    runtime_id: &str,
    request: &RuntimeTurnRequest,
    activity: Option<TurnActivity>,
) -> Result<()> {
    let answer = TurnOutcome::Answered {
        output: format!("reply to {}", request.input),
        activity,
    };
    let settled = core.settle(runtime_id, &request.correlation_id, answer)?;
    anyhow::ensure!(settled == Settled::Recorded, "the answer was {settled:?}");
    Ok(())
}

/// Hands `runtime_id` its oldest queued turn and answers it.
async fn answer_next(core: &Core, runtime_id: &str) -> Result<RuntimeTurnRequest> {
    let request = hand_next(core, runtime_id)
        .await?
        .context("nothing was queued")?;
    answer(core, runtime_id, &request, None)?;
    Ok(request)
}

async fn accept(state: &Fixture, id: &str, audience: &str) -> Result<EventReceipt> {
    accept_for(state, &state.config.value.agents[0].id, id, audience).await
}

async fn accept_for(
    state: &Fixture,
    agent: &str,
    id: &str,
    audience: &str,
) -> Result<EventReceipt> {
    let mut headers = HeaderMap::new();
    headers.insert(PROVIDER_HEADER, "test-interface".parse()?);
    headers.insert(
        http::header::AUTHORIZATION,
        format!("Bearer {}", state.config.credential("test-interface")?).parse()?,
    );
    let receipt = accept_event(
        State(configured(state)),
        headers,
        Json(ExternalEvent {
            event_id: id.to_owned(),
            agent_id: agent.to_owned(),
            user_id: "test-user".to_owned(),
            audience_id: audience.to_owned(),
            channel_id: "test-channel".to_owned(),
            delivery_target_id: "test-destination".to_owned(),
            feedback_target_id: "test-feedback".to_owned(),
            content: id.to_owned(),
            native: Value::Null,
            attachments: None,
        }),
    )
    .await
    .map_err(|error| anyhow!(error.message))?;
    Ok(receipt.0)
}

/// A runtime's final report that counted `calls` calls, as it reads on the wire.
fn report_value(calls: u32) -> Value {
    json!({
        "phase": "model",
        "running": [],
        "recent": [{"tool": "bash", "detail": "make", "ok": false, "duration_ms": 900}],
        "tool_calls": calls,
        "notes": ["Building first."],
    })
}

fn report(calls: u32) -> Result<TurnActivity> {
    Ok(serde_json::from_value(report_value(calls))?)
}

#[tokio::test]
async fn startup_opens_the_database_without_creating_one() -> Result<()> {
    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    state.config.value.core.listen = listener.local_addr()?;
    drop(listener);
    let path = state.config.value.data_dir.join("carillon.sqlite3");
    fs::remove_file(&path)?;
    crate::assert_refused(
        serve(
            state.config.root.join("carillon.toml"),
            state.config.clone(),
        )
        .await,
        "unable to open database file",
    )?;
    assert!(!path.exists());
    Ok(())
}

#[tokio::test]
async fn occupied_listener_leaves_accepted_and_running_work_untouched() -> Result<()> {
    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    let running = accept(&state, "running", "busy").await?;
    hand_next(&state.core, "test-runtime")
        .await?
        .context("missing running turn")?;
    let queued = accept(&state, "queued", "other").await?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    state.config.value.core.listen = listener.local_addr()?;

    let error = tokio::time::timeout(
        Duration::from_secs(5),
        serve(
            state.config.root.join("carillon.toml"),
            state.config.clone(),
        ),
    )
    .await?
    .err()
    .context("occupied port was accepted")?;
    assert_eq!(
        error
            .downcast_ref::<std::io::Error>()
            .context("not a bind error")?
            .kind(),
        std::io::ErrorKind::AddrInUse
    );
    assert_eq!(
        state
            .core
            .database
            .inspect_turn(&running.turn_id)?
            .context("missing running turn")?
            .state,
        TurnState::Running
    );
    assert_eq!(
        state
            .core
            .database
            .inspect_turn(&queued.turn_id)?
            .context("missing queued turn")?
            .state,
        TurnState::Queued
    );
    Ok(())
}

#[tokio::test]
async fn live_interface_claim_survives_core_startup_and_acknowledges_once() -> Result<()> {
    use carillon_protocol::model::DeliveryState;

    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    let receipt = accept(&state, "one", "conversation").await?;
    answer_next(&state.core, "test-runtime").await?;
    let claim = state
        .core
        .database
        .claim_delivery("test-interface", "live-instance")?
        .context("missing claim")?;
    // Startup uses the real service path, not merely reopening SQLite.
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    state.config.value.core.listen = listener.local_addr()?;
    drop(listener);
    let base = format!("http://{}", state.config.value.core.listen);
    let server = tokio::spawn(serve(
        state.config.root.join("carillon.toml"),
        state.config.clone(),
    ));
    let outcome = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if state
                .client
                .get(format!("{base}/health"))
                .send()
                .await
                .is_ok()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        let request = |method, path: &str| {
            state
                .client
                .request(method, format!("{base}{path}"))
                .bearer_auth("test-only")
                .header(PROVIDER_HEADER, "test-interface")
                .header(INSTANCE_HEADER, "live-instance")
        };
        let reconciliation = request(reqwest::Method::POST, "/v1/deliveries/reconcile")
            .send()
            .await?
            .error_for_status()?
            .json::<DeliveryReconciliation>()
            .await?;
        assert!(reconciliation.feedback_target_ids.is_empty());
        let recovered = request(reqwest::Method::GET, "/v1/deliveries")
            .send()
            .await?
            .error_for_status()?
            .json::<Option<Delivery>>()
            .await?
            .context("live claim was lost")?;
        assert_eq!(recovered.id, claim.id);
        // The interface already sent the reply; it retries only the known acknowledgement.
        for _ in 0..2 {
            assert_eq!(
                request(
                    reqwest::Method::POST,
                    &format!("/v1/deliveries/{}/ack", claim.id)
                )
                .json(&DeliveryAck {
                    outcome: DeliveryOutcome::Delivered,
                    native_ref: Some("native-reply".to_owned())
                })
                .send()
                .await?
                .status(),
                StatusCode::NO_CONTENT
            );
        }
        assert!(
            request(reqwest::Method::GET, "/v1/deliveries")
                .send()
                .await?
                .error_for_status()?
                .json::<Option<Delivery>>()
                .await?
                .is_none()
        );
        let turn = state
            .core
            .database
            .inspect_turn(&receipt.turn_id)?
            .context("missing turn")?;
        assert_eq!(turn.delivery_state, Some(DeliveryState::Delivered));
        assert_eq!(turn.delivery_native_ref.as_deref(), Some("native-reply"));
        Ok::<_, anyhow::Error>(())
    })
    .await;
    server.abort();
    let _ = server.await;
    outcome??;
    Ok(())
}

#[tokio::test]
async fn credential_drift_leaves_work_queued_until_repaired() -> Result<()> {
    let directory = tempdir()?;
    let runtime = durable::DurableRuntime::start().await?;
    let (_fixture, state) = durable::durable(directory.path(), &runtime)?;
    let credential = directory.path().join("credentials/test-runtime.token");
    let first = live::ok(live::submit(&state, "one", "conversation").await)?;
    for replacement in [Some("changed-token"), Some("\n"), None] {
        if let Some(value) = replacement {
            fs::write(&credential, value)?;
        } else {
            fs::remove_file(&credential)?;
        }
        let stalled = durable::dispatch(&state)
            .await
            .err()
            .context("a turn was handed over with a drifted credential")?;
        assert!(matches!(stalled, crate::Stalled::Core(_)), "{stalled}");
        assert!(stalled.to_string().contains("credential"), "{stalled}");
        assert!(
            runtime.handed().is_empty(),
            "runtime was contacted before credential validation"
        );
        assert_eq!(
            durable::state_of(&state, &first.turn_id)?,
            TurnState::Queued
        );
    }
    fs::write(&credential, "test-only\n")?;
    assert_eq!(durable::dispatch(&state).await?, WorkerStep::Advanced);
    assert_eq!(runtime.handed(), ["one"]);
    assert_eq!(runtime.request(0)?.correlation_id, first.turn_id);
    assert_eq!(
        durable::state_of(&state, &first.turn_id)?,
        TurnState::Running
    );
    Ok(())
}

#[tokio::test]
async fn duplicate_events_share_one_accepted_turn_without_reexecution() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let first = accept(&state, "one", "conversation").await?;
    let duplicate = accept(&state, "one", "conversation").await?;
    assert_eq!(first.state, TurnState::Queued);
    assert!(!first.duplicate);
    assert!(duplicate.duplicate);
    assert_eq!(duplicate.turn_id, first.turn_id);
    assert_eq!(state.core.database.recent_turns(None, 10)?.len(), 1);
    assert!(
        state
            .core
            .database
            .claim_delivery("test-interface", "instance")?
            .is_none()
    );
    let snapshot = state.snapshot_store();
    fs::write(
        state.config.value.resource_roots()[0].join("SYSTEM.md"),
        "changed after acceptance",
    )?;
    let request = answer_next(&state.core, "test-runtime").await?;
    assert_eq!(request.correlation_id, first.turn_id);
    assert!(Path::new(&request.resource_root).starts_with(&snapshot));
    assert_ne!(
        fs::read_to_string(Path::new(&request.resource_root).join("SYSTEM.md"))?,
        "changed after acceptance"
    );
    assert_eq!(
        accept(&state, "one", "conversation").await?.state,
        TurnState::Completed
    );
    assert!(hand_next(&state.core, "test-runtime").await?.is_none());
    let delivery = state
        .core
        .database
        .claim_delivery("test-interface", "instance")?
        .context("missing reply")?;
    assert_eq!(delivery.turn_id, first.turn_id);
    assert_eq!(delivery.content, "reply to one");
    Ok(())
}

#[tokio::test]
async fn dispatchers_hear_of_queued_turns_and_hand_over_nothing_once_stopping() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let mut work = state.core.work();
    let mut changed = || {
        let seen = work.has_changed().ok();
        work.mark_unchanged();
        seen
    };
    let queued = accept(&state, "one", "conversation").await?;
    assert_eq!(changed(), Some(true));
    // Stopping, it hands nothing over: the turn stays queued.
    let stopping = CancellationToken::new();
    stopping.cancel();
    let step = state
        .core
        .dispatch_next("test-runtime", &stopping, async |_| {
            unreachable!("a turn was handed over once stopping")
        })
        .await?;
    assert_eq!(step, WorkerStep::Idle);
    assert_eq!(
        state
            .core
            .inspect_turn(&queued.turn_id)?
            .context("missing turn")?
            .state,
        TurnState::Queued
    );
    // A hand-over queues nothing, so it wakes no dispatcher.
    hand_next(&state.core, "test-runtime").await?;
    assert_eq!(changed(), Some(false));
    // Runtimes changing do, so a runtime enabled again is handed its turns.
    state.core.set_runtimes(runtime_facts(&state.config))?;
    assert_eq!(changed(), Some(true));
    Ok(())
}

#[tokio::test]
async fn missing_resource_root_refuses_new_acceptance_without_losing_receipts() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let first = accept(&state, "one", "conversation").await?;
    let root = &state.config.value.resource_roots()[0].to_path_buf();
    let saved = directory.path().join("saved-resources");
    fs::rename(root, &saved)?;
    for missing in [true, false] {
        if !missing {
            fs::write(root, "not a directory")?;
        }
        crate::assert_refused(
            accept(&state, "two", "conversation").await,
            "agent resource root",
        )?;
        assert_eq!(state.core.database.recent_turns(None, 10)?.len(), 1);
        assert!(
            state
                .core
                .database
                .event_receipt("test-interface", "two")?
                .is_none()
        );
        let duplicate = accept(&state, "one", "conversation").await?;
        assert!(duplicate.duplicate);
        assert_eq!(duplicate.turn_id, first.turn_id);
        assert_eq!(duplicate.state, TurnState::Queued);
    }
    fs::remove_file(root)?;
    fs::rename(saved, root)?;
    // An existing empty root is valid: individual resources remain optional.
    fs::remove_dir_all(root)?;
    fs::create_dir(root)?;
    let second = accept(&state, "two", "conversation").await?;
    assert!(!second.duplicate);
    assert_eq!(state.core.database.recent_turns(None, 10)?.len(), 2);
    let request = answer_next(&state.core, "test-runtime").await?;
    assert_eq!(request.correlation_id, first.turn_id);
    assert!(
        Path::new(&request.resource_root)
            .join("SYSTEM.md")
            .is_file()
    );
    Ok(())
}

#[tokio::test]
async fn duplicate_receipts_do_not_read_mutable_resources() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let first = accept(&state, "one", "conversation").await?;
    // A resource change after acceptance must affect new work, not receipt recovery.
    std::os::unix::fs::symlink(
        "SYSTEM.md",
        state.config.value.resource_roots()[0].join("AGENTS.md"),
    )?;
    let duplicate = accept(&state, "one", "conversation").await?;
    assert!(duplicate.duplicate);
    assert_eq!(duplicate.turn_id, first.turn_id);
    assert_eq!(duplicate.state, TurnState::Queued);
    crate::assert_refused(
        accept(&state, "two", "conversation").await,
        "resource symlinks are not supported",
    )?;
    assert_eq!(state.core.database.recent_turns(None, 10)?.len(), 1);
    assert!(
        state
            .core
            .database
            .event_receipt("test-interface", "two")?
            .is_none()
    );
    Ok(())
}

#[test]
fn check_opens_storage_like_the_service_without_writing() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    check(&state.config)?;
    let database = state.config.value.data_dir.join("carillon.sqlite3");
    rusqlite::Connection::open(&database)?.execute_batch("DROP TABLE conversations")?;
    let before = fs::read(&database)?;
    crate::assert_refused(check(&state.config), "not the current Carillon format")?;
    assert_eq!(fs::read(&database)?, before);
    Ok(())
}

/// How the runtime substitute answers an activity request.
#[derive(Clone, Copy)]
enum Snapshot {
    Current,
    Missing,
    Malformed,
    Hanging,
}

async fn jobs_call(
    state: &Fixture,
    provider: &str,
    token: Option<&str>,
    query: &str,
) -> Result<(StatusCode, Value)> {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt as _;

    let router = Router::new()
        .route("/v1/jobs", get(jobs))
        .with_state(configured(state));
    let mut request = Request::builder()
        .uri(format!("/v1/jobs{query}"))
        .header(PROVIDER_HEADER, provider);
    if let Some(token) = token {
        request = request.header(http::header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let response = tokio::time::timeout(
        Duration::from_secs(10),
        router.oneshot(request.body(Body::empty())?),
    )
    .await??;
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1 << 20).await?;
    Ok((status, serde_json::from_slice(&body)?))
}

/// A runtime that records each activity request and answers as `mode` says.
fn activity_runtime(
    mode: Arc<std::sync::Mutex<Snapshot>>,
    sent: tokio::sync::mpsc::UnboundedSender<Value>,
) -> Router {
    Router::new().route(
        "/v1/turns/{id}/activity",
        get(
            move |AxumPath(id): AxumPath<String>,
                  Query(query): Query<std::collections::HashMap<String, String>>,
                  headers: HeaderMap| {
                let mode = *mode
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let _ = sent.send(json!({
                    "id": id,
                    "query": query,
                    "auth": headers.get(http::header::AUTHORIZATION).and_then(|v| v.to_str().ok()),
                }));
                async move {
                    match mode {
                        Snapshot::Current => Json(json!({
                            "phase": "tools",
                            "running": [{"tool": "bash", "detail": "cargo test", "started_at": 1}],
                            "recent": [],
                            "tool_calls": 1,
                        }))
                        .into_response(),
                        Snapshot::Missing => StatusCode::NOT_FOUND.into_response(),
                        Snapshot::Malformed => Json(json!({"phase": "thinking"})).into_response(),
                        Snapshot::Hanging => {
                            tokio::time::sleep(Duration::from_secs(30)).await;
                            StatusCode::NOT_FOUND.into_response()
                        }
                    }
                }
            },
        ),
    )
}

/// Registers `other-interface` and accepts one turn for it.
async fn foreign_turn(state: &mut Fixture, directory: &Path) -> Result<EventReceipt> {
    use carillon_installation::config::hash_token;

    let mut other = state
        .config
        .provider("test-interface", ProviderRole::Interface)?
        .clone();
    "other-interface".clone_into(&mut other.id);
    other.token_hash = hash_token("other-only");
    state.config.value.providers.push(other);
    fs::write(
        directory.join("credentials/other-interface.token"),
        "other-only",
    )?;
    Ok(configured(state)
        .core
        .accept(
            &selected_provider(
                state
                    .config
                    .provider("other-interface", ProviderRole::Interface)?,
            ),
            &ExternalEvent {
                event_id: "foreign".to_owned(),
                agent_id: "test-agent".to_owned(),
                user_id: "someone".to_owned(),
                audience_id: "elsewhere".to_owned(),
                channel_id: "test-channel".to_owned(),
                delivery_target_id: "foreign-destination".to_owned(),
                feedback_target_id: "foreign-feedback".to_owned(),
                content: "not yours".to_owned(),
                native: Value::Null,
                attachments: None,
            },
        )
        .await?)
}

#[tokio::test]
async fn job_reads_show_only_the_callers_jobs_with_runtime_snapshots() -> Result<()> {
    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    let mode = Arc::new(std::sync::Mutex::new(Snapshot::Current));
    let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    state.config.value.providers[1].url = format!("http://{}", listener.local_addr()?);
    let mut services = tokio::task::JoinSet::new();
    let runtime = activity_runtime(mode.clone(), sent);
    services.spawn(async move { axum::serve(listener, runtime).await });

    // A finished job is not at work.
    accept(&state, "finished", "earlier").await?;
    let finished = hand_next(&state.core, "test-runtime")
        .await?
        .context("nothing queued")?;
    answer(&state.core, "test-runtime", &finished, Some(report(2)?))?;
    let running = accept(&state, "running", "conversation").await?;
    let queued = accept(&state, "queued", "conversation").await?;
    foreign_turn(&mut state, directory.path()).await?;
    let held = hand_next(&state.core, "test-runtime")
        .await?
        .context("nothing queued")?;
    assert_eq!(held.correlation_id, running.turn_id);

    let (status, body) = jobs_call(&state, "test-interface", Some("test-only"), "").await?;
    assert_eq!(status, StatusCode::OK);
    let accepted = |index: usize| body["jobs"][index]["origin"]["accepted_at"].clone();
    assert_eq!(
        body,
        json!({"jobs": [
            {
                "origin": {"turn_id": running.turn_id, "accepted_at": accepted(0)},
                "target_id": "test-destination",
                "feedback_target_id": "test-feedback",
                "turns": [{
                    "turn_id": running.turn_id,
                    "state": "running",
                    "accepted_at": accepted(0),
                    "activity": {
                        "phase": "tools",
                        "running": [{"tool": "bash", "detail": "cargo test", "started_at": 1}],
                        "recent": [],
                        "tool_calls": 1,
                    },
                }],
            },
            {
                "origin": {"turn_id": queued.turn_id, "accepted_at": accepted(1)},
                "target_id": "test-destination",
                "feedback_target_id": "test-feedback",
                "turns": [{
                    "turn_id": queued.turn_id,
                    "state": "queued",
                    "accepted_at": accepted(1),
                }],
            },
        ]})
    );
    assert!(accepted(0).as_i64().is_some_and(|time| time > 0));
    // Only running turns are relayed, with the runtime credential, naming the caller.
    assert_eq!(
        received.try_recv()?,
        json!({
            "id": running.turn_id,
            "query": {"interface_id": "test-interface"},
            "auth": "Bearer test-only",
        })
    );
    assert_eq!(received.try_recv(), Err(TryRecvError::Empty));

    // A snapshot the runtime cannot supply is absent, never a shorter list.
    for snapshot in [Snapshot::Missing, Snapshot::Malformed, Snapshot::Hanging] {
        *mode
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = snapshot;
        let started = std::time::Instant::now();
        let (status, body) = jobs_call(&state, "test-interface", Some("test-only"), "").await?;
        assert_eq!(status, StatusCode::OK);
        let turn = &body["jobs"][0]["turns"][0];
        assert_eq!(turn["state"], "running");
        assert_eq!(turn.get("activity"), None);
        assert_eq!(body["jobs"].as_array().map(Vec::len), Some(2));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    for (provider, token) in [
        ("test-interface", None),
        ("test-interface", Some("wrong")),
        ("test-interface", Some("other-only")),
        ("test-interface", Some("test-operator")),
        ("test-runtime", Some("test-only")),
        ("unknown", Some("test-only")),
    ] {
        let (status, _) = jobs_call(&state, provider, token, "").await?;
        assert_eq!(status, StatusCode::FORBIDDEN, "{provider} {token:?}");
    }

    answer(&state.core, "test-runtime", &held, None)?;
    let (_, body) = jobs_call(&state, "test-interface", Some("test-only"), "").await?;
    assert_eq!(body["jobs"].as_array().map(Vec::len), Some(1));
    assert_eq!(body["jobs"][0]["origin"]["turn_id"], json!(queued.turn_id));
    Ok(())
}

#[tokio::test]
async fn job_reads_name_the_callers_finished_jobs_within_their_bound() -> Result<()> {
    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    let finished = accept(&state, "finished", "earlier").await?;
    let request = hand_next(&state.core, "test-runtime")
        .await?
        .context("nothing queued")?;
    answer(&state.core, "test-runtime", &request, Some(report(2)?))?;
    let running = accept(&state, "running", "conversation").await?;
    let foreign = foreign_turn(&mut state, directory.path()).await?;
    // A finished job is read by name, with its report and its answer's
    // state; another interface's job is not.
    let names = format!("?tracked={},{}", finished.turn_id, foreign.turn_id);
    let (status, body) = jobs_call(&state, "test-interface", Some("test-only"), &names).await?;
    assert_eq!(status, StatusCode::OK);
    let [read] = body["jobs"].as_array().context("jobs")?.as_slice() else {
        anyhow::bail!("expected the finished job only: {body}");
    };
    assert_eq!(read["origin"]["turn_id"], json!(finished.turn_id));
    let turn = &read["turns"][0];
    assert_eq!(
        (
            &turn["state"],
            &turn["delivery"],
            &turn["activity"]["tool_calls"]
        ),
        (&json!("completed"), &json!("pending"), &json!(2))
    );
    assert!(turn["finished_at"].as_i64().is_some());
    assert_eq!(body.get("next"), None);

    // Another interface sees its own work and nothing of this one's.
    let (_, body) = jobs_call(&state, "other-interface", Some("other-only"), "").await?;
    assert_eq!(body["jobs"].as_array().map(Vec::len), Some(1));
    assert_eq!(body["jobs"][0]["origin"]["turn_id"], json!(foreign.turn_id));
    let (status, _) = jobs_call(
        &state,
        "test-interface",
        Some("test-only"),
        &format!("?after={}", foreign.turn_id),
    )
    .await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // A read names a bounded list of jobs, or pages through those at work.
    let many = (0..=JOB_READ_LIMIT)
        .map(|index| format!("job-{index}"))
        .collect::<Vec<_>>()
        .join(",");
    for query in [
        format!("?tracked={many}"),
        format!("?tracked={},", finished.turn_id),
        format!("?tracked={}&after={}", finished.turn_id, running.turn_id),
    ] {
        let (status, _) = jobs_call(&state, "test-interface", Some("test-only"), &query).await?;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}");
    }

    Ok(())
}

#[tokio::test]
async fn job_reads_answer_promptly_when_several_runtimes_cannot_describe_their_turns() -> Result<()>
{
    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    let (sent, _received) = tokio::sync::mpsc::unbounded_channel();
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let mut services = tokio::task::JoinSet::new();
    let runtime = activity_runtime(Arc::new(std::sync::Mutex::new(Snapshot::Hanging)), sent);
    services.spawn(async move { axum::serve(listener, runtime).await });
    let mut accepted = Vec::new();
    for index in 0..6 {
        let id = format!("runtime-{index}");
        let mut provider = state
            .config
            .provider("test-runtime", ProviderRole::Runtime)?
            .clone();
        provider.id.clone_from(&id);
        provider.url.clone_from(&url);
        fs::write(
            directory.path().join(format!("credentials/{id}.token")),
            "test-only",
        )?;
        state.config.value.providers.push(provider);
        // New conversations belong to the runtime that is the default when they start.
        state.config.value.agents[0].runtime.clone_from(&id);
        let receipt = accept(
            &state,
            &format!("event-{index}"),
            &format!("conversation-{index}"),
        )
        .await?;
        let held = hand_next(&configured(&state).core, &id)
            .await?
            .context("nothing queued")?;
        assert_eq!(held.correlation_id, receipt.turn_id);
        accepted.push(receipt.turn_id);
    }
    let started = std::time::Instant::now();
    let (status, body) = jobs_call(&state, "test-interface", Some("test-only"), "").await?;
    let elapsed = started.elapsed();
    assert_eq!(status, StatusCode::OK);
    let listed = body["jobs"]
        .as_array()
        .context("list")?
        .iter()
        .map(|job| {
            let turn = &job["turns"][0];
            (
                turn["turn_id"].as_str().map(str::to_owned),
                turn.get("activity").cloned().unwrap_or_default(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        listed,
        accepted
            .into_iter()
            .map(|turn| (Some(turn), Value::Null))
            .collect::<Vec<_>>()
    );
    // Well within an interface's ten-second request deadline.
    assert!(elapsed < Duration::from_secs(4), "{elapsed:?}");
    Ok(())
}

mod attachments;
mod configuration;
mod cutover;
mod durable;
mod health;
mod held;
mod live;
mod maintenance;
mod reserved;
mod settings;

/// One async worker, as when every worker could be taken: another writer
/// holds SQLite's write lock while an interface claims a delivery and a
/// dispatcher hands a turn over. Their storage waits run beside the async
/// worker, so unrelated requests are answered meanwhile, and both complete
/// once the lock is released.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn storage_waits_never_hold_the_async_workers() -> Result<()> {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt as _;

    let directory = tempdir()?;
    let state = installation(directory.path())?;
    accept(&state, "queued", "conversation").await?;
    let writer =
        rusqlite::Connection::open(state.core.options.data_directory.join("carillon.sqlite3"))?;
    writer.execute_batch("BEGIN IMMEDIATE")?;
    let router = Router::new()
        .route("/health", get(health))
        .route("/v1/deliveries", get(claim_delivery))
        .with_state(configured(&state));
    let delivery = tokio::spawn(
        router.clone().oneshot(
            Request::builder()
                .uri("/v1/deliveries")
                .header(PROVIDER_HEADER, "test-interface")
                .header(INSTANCE_HEADER, "instance")
                .header(http::header::AUTHORIZATION, "Bearer test-only")
                .body(Body::empty())?,
        ),
    );
    let worker = {
        let state = state.clone();
        tokio::spawn(async move { hand_next(&state.core, "test-runtime").await })
    };
    // Both are now waiting for the lock.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let asked = std::time::Instant::now();
    let health = tokio::spawn(
        router
            .clone()
            .oneshot(Request::builder().uri("/health").body(Body::empty())?),
    );
    let health = tokio::time::timeout(Duration::from_secs(1), health).await???;
    assert_eq!(health.status(), StatusCode::OK);
    assert!(
        asked.elapsed() < Duration::from_millis(500),
        "health waited {:?} behind storage",
        asked.elapsed()
    );
    assert!(!delivery.is_finished() && !worker.is_finished());
    writer.execute_batch("ROLLBACK")?;
    let delivery = tokio::time::timeout(Duration::from_secs(5), delivery).await???;
    assert_eq!(delivery.status(), StatusCode::OK);
    let handed = tokio::time::timeout(Duration::from_secs(5), worker).await???;
    assert!(handed.is_some());
    Ok(())
}

/// Storage work started for an async caller finishes even when the caller
/// is stopped, as a dispatcher is at shutdown, and the service waits for it
/// before releasing its exclusive ownership.
#[tokio::test]
async fn shutdown_waits_for_storage_work_a_stopped_caller_started() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let receipt = accept(&state, "running", "conversation").await?;
    let core = state.core.clone();
    let (started, began) = tokio::sync::oneshot::channel();
    let caller = tokio::spawn(async move {
        core.blocking(move |core| {
            let _ = started.send(());
            std::thread::sleep(Duration::from_millis(300));
            let turn = core.database.next_dispatch("test-runtime")?;
            turn.map(|turn| core.database.dispatched(&turn)).transpose()
        })
        .await
    });
    began.await?;
    caller.abort();
    state.core.storage_settled().await;
    let turn = state
        .core
        .inspect_turn(&receipt.turn_id)?
        .context("turn missing")?;
    assert_eq!(turn.state, TurnState::Running);
    Ok(())
}

#[tokio::test]
async fn a_turns_final_report_is_kept_with_its_outcome_and_read_within_its_bound() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    // The report a read naming the turn's job shows beside its outcome.
    let reported = |turn_id: &str| -> Result<Option<TurnActivity>> {
        let jobs = state
            .core
            .named_jobs("test-interface", &[turn_id.to_owned()])
            .map_err(|failure| anyhow!("{failure}"))?;
        let [(_, job)] = jobs.as_slice() else {
            anyhow::bail!("expected one job: {jobs:?}");
        };
        Ok(job.turns[0].activity.clone())
    };
    let next_delivery = || -> Result<Delivery> {
        let delivery = state
            .core
            .database
            .claim_delivery("test-interface", "instance")?
            .context("delivery missing")?;
        state.core.database.acknowledge_delivery(
            "test-interface",
            delivery.id,
            "instance",
            DeliveryOutcome::Delivered,
            None,
        )?;
        Ok(delivery)
    };
    // An answer's report.
    accept(&state, "answered", "conversation").await?;
    let request = hand_next(&state.core, "test-runtime")
        .await?
        .context("nothing queued")?;
    answer(&state.core, "test-runtime", &request, Some(report(3)?))?;
    let answered = next_delivery()?;
    assert_eq!(answered.turn_state, TurnState::Completed);
    assert_eq!(reported(&answered.turn_id)?, Some(report(3)?));
    // A failed turn's report, though the runtime could show no explanation.
    accept(&state, "failed", "conversation").await?;
    let request = hand_next(&state.core, "test-runtime")
        .await?
        .context("nothing queued")?;
    let failure = TurnOutcome::Failed {
        explanation: None,
        activity: Some(report(1)?),
    };
    state
        .core
        .settle("test-runtime", &request.correlation_id, failure)?;
    let failed = next_delivery()?;
    assert_eq!(failed.turn_state, TurnState::Interrupted);
    assert!(failed.content.starts_with("That turn was interrupted."));
    assert_eq!(reported(&failed.turn_id)?, Some(report(1)?));
    let error = state
        .core
        .inspect_turn(&failed.turn_id)?
        .context("failed turn missing")?
        .error;
    assert_eq!(
        error.as_deref(),
        Some("runtime provider failed the turn with no explanation")
    );
    // A report over its bound is not kept, and costs the turn nothing.
    accept(&state, "oversized", "conversation").await?;
    let request = hand_next(&state.core, "test-runtime")
        .await?
        .context("nothing queued")?;
    let mut activity = report(1)?;
    activity.notes = vec!["語".repeat(500); 8];
    activity.recent[0].detail = Some("d".repeat(8_000));
    answer(&state.core, "test-runtime", &request, Some(activity))?;
    let oversized = next_delivery()?;
    assert_eq!(oversized.turn_state, TurnState::Completed);
    assert_eq!(oversized.content, "reply to oversized");
    assert_eq!(reported(&oversized.turn_id)?, None);
    Ok(())
}

#[tokio::test]
async fn each_agent_runs_on_its_own_runtime_with_its_own_resources() -> Result<()> {
    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    let mut replacement = state
        .config
        .provider("test-runtime", ProviderRole::Runtime)?
        .clone();
    replacement.id = "replacement".to_owned();
    replacement.url = "http://127.0.0.1:59".to_owned();
    fs::write(
        directory.path().join("credentials/replacement.token"),
        "test-only",
    )?;
    state.config.value.providers.push(replacement);
    let mut plain = state.config.value.agents[0].clone();
    "plain".clone_into(&mut plain.id);
    "replacement".clone_into(&mut plain.runtime);
    plain.resources = None;
    state.config.value.agents.push(plain);
    state.config.value.validate()?;
    fs::write(
        state.config.value.resource_roots()[0].join("SYSTEM.md"),
        "assistant",
    )?;

    // One audience, two agents: two conversations, each on its agent's runtime.
    let assistant = accept(&state, "assistant-turn", "conversation").await?;
    let plain = accept_for(&state, "plain", "plain-turn", "conversation").await?;
    let core = configured(&state).core;
    let request = answer_next(&core, "replacement").await?;
    assert_eq!(request.correlation_id, plain.turn_id);
    assert_eq!(request.agent_id, "plain");
    assert_eq!(fs::read_dir(&request.resource_root)?.count(), 0);
    let request = answer_next(&core, "test-runtime").await?;
    assert_eq!(request.correlation_id, assistant.turn_id);
    let system = fs::read_to_string(Path::new(&request.resource_root).join("SYSTEM.md"))?;
    assert_eq!(system, "assistant");
    crate::assert_refused(
        accept_for(&state, "unknown", "unknown-turn", "conversation").await,
        "unknown agent",
    )?;
    Ok(())
}
