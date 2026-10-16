use std::{
    fs,
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
};

use anyhow::Context as _;
use axum::response::IntoResponse as _;
use tempfile::tempdir;
use tokio::sync::mpsc::error::TryRecvError;

use super::*;
use crate::Reported;
use crate::db::Database;
use carillon_protocol::model::{DeliveryOutcome, JOB_READ_LIMIT, TurnState};

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
            parallel_turns: None,
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

async fn run_next_turn(
    state: &Fixture,
    runtime_id: &str,
    submit: impl AsyncFnOnce(
        &Provider,
        &str,
        RuntimeTurnRequest,
    ) -> Result<RuntimeTurnResponse, TurnFailure>,
) -> Result<WorkerStep> {
    run_next(
        &configured(state),
        runtime_id,
        Lane::Messages,
        &CancellationToken::new(),
        submit,
    )
    .await
}

async fn run_next_observation(
    state: &Fixture,
    runtime_id: &str,
    submit: impl AsyncFnOnce(
        &Provider,
        &str,
        RuntimeTurnRequest,
    ) -> Result<RuntimeTurnResponse, TurnFailure>,
) -> Result<WorkerStep> {
    run_next(
        &configured(state),
        runtime_id,
        Lane::Observations,
        &CancellationToken::new(),
        submit,
    )
    .await
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

fn completed(request: RuntimeTurnRequest) -> RuntimeTurnResponse {
    RuntimeTurnResponse {
        correlation_id: request.correlation_id,
        output: format!("reply to {}", request.input),
        follow_up: None,
        unanswered_left: false,
        activity: None,
        attachments_left: false,
    }
}

#[test]
fn configuration_rejects_malformed_provider_endpoints_before_startup() -> Result<()> {
    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    let path = directory.path().join("carillon.toml");
    for index in 0..state.config.value.providers.len() {
        let original = state.config.value.providers[index].url.clone();
        for url in [
            "not-a-url",
            "",
            "https://127.0.0.1:3000",
            "http://localhost:3000",
            "http://127.0.0.1",
            "http://127.0.0.1:99999",
            "http://127.0.0.1:3000/path",
            "http://user@127.0.0.1:3000",
            "http://127.0.0.1:3000?query",
            "http://0.0.0.0:3000",
            "http://192.168.1.2:3000",
            "http://[::]:3000",
            "http://[2001:db8::1]:3000",
            "http://127.0.0.1:0",
            "http://[::1]:0",
        ] {
            state.config.value.providers[index].url = url.to_owned();
            fs::write(&path, toml::to_string(&state.config.value)?)?;
            crate::assert_refused(LoadedConfig::load(&path), "invalid URL for provider")?;
        }
        for url in [
            "http://127.0.0.1:3000",
            "http://127.2.3.4:3000",
            "http://[::1]:3000",
        ] {
            state.config.value.providers[index].url = url.to_owned();
            fs::write(&path, toml::to_string(&state.config.value)?)?;
            LoadedConfig::load(&path)?;
        }
        state.config.value.providers[index].url = original;
    }
    // Concurrent conversations are a bounded runtime setting.
    for index in 0..state.config.value.providers.len() {
        let runtime = state.config.value.providers[index].role == ProviderRole::Runtime;
        for (parallel, valid) in [(0, false), (1, runtime), (16, runtime), (17, false)] {
            state.config.value.providers[index].parallel_turns = Some(parallel);
            fs::write(&path, toml::to_string(&state.config.value)?)?;
            let id = &state.config.value.providers[index].id;
            assert_eq!(
                LoadedConfig::load(&path)
                    .err()
                    .map(|error| error.to_string()),
                (!valid).then(|| format!(
                    "provider {id} parallel_turns must be between 1 and 16, on a runtime"
                )),
                "parallel_turns {parallel}"
            );
        }
        state.config.value.providers[index].parallel_turns = None;
    }
    state.config.value.core.listen.set_port(0);
    fs::write(&path, toml::to_string(&state.config.value)?)?;
    crate::assert_refused(
        LoadedConfig::load(&path),
        "core listen port must be nonzero",
    )?;
    Ok(())
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
    state
        .core
        .database
        .claim("test-runtime", Lane::Messages, &CancellationToken::new())?
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
    run_next_turn(&state, "test-runtime", async |_, _, request| {
        Ok(completed(request))
    })
    .await?;
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
    let mut state = installation(directory.path())?;
    let credential = directory.path().join("credentials/test-runtime.token");
    let first = accept(&state, "one", "conversation").await?;
    for replacement in [Some("changed-token"), Some("\n"), None] {
        if let Some(value) = replacement {
            fs::write(&credential, value)?;
        } else {
            fs::remove_file(&credential)?;
        }
        let mut calls = 0;
        let result = run_next_turn(&state, "test-runtime", async |_, _, request| {
            calls += 1;
            Ok(completed(request))
        })
        .await;
        crate::assert_refused(result, "credential")?;
        assert_eq!(
            calls, 0,
            "runtime was contacted before credential validation"
        );
        let turn = state
            .core
            .database
            .inspect_turn(&first.turn_id)?
            .context("missing turn")?;
        assert_eq!(turn.state, TurnState::Queued);
        assert!(turn.delivery_state.is_none());
    }
    state.core.database = Database::open(&state.config.value.data_dir.join("carillon.sqlite3"))?;
    assert_eq!(state.core.database.recover_running_turns()?, 0);
    fs::write(&credential, "test-only\n")?;
    let mut calls = 0;
    assert_eq!(
        run_next_turn(&state, "test-runtime", async |_, token, request| {
            calls += 1;
            assert_eq!(token, "test-only");
            assert_eq!(request.correlation_id, first.turn_id);
            Ok(completed(request))
        })
        .await?,
        WorkerStep::Advanced
    );
    assert_eq!(calls, 1);
    assert_eq!(
        state
            .core
            .database
            .inspect_turn(&first.turn_id)?
            .context("missing turn")?
            .state,
        TurnState::Completed
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
    let step = run_next_turn(&state, "test-runtime", async |runtime, _, request| {
        assert_eq!(runtime.id, "test-runtime");
        assert_eq!(request.correlation_id, first.turn_id);
        assert!(Path::new(&request.resource_root).starts_with(&snapshot));
        assert_ne!(
            fs::read_to_string(Path::new(&request.resource_root).join("SYSTEM.md"))
                .map_err(|error| TurnFailure::Retry(error.into()))?,
            "changed after acceptance"
        );
        Ok(completed(request))
    })
    .await?;
    assert_eq!(step, WorkerStep::Advanced);
    assert_eq!(
        accept(&state, "one", "conversation").await?.state,
        TurnState::Completed
    );
    assert_eq!(
        run_next_turn(&state, "test-runtime", async |_, _, _| {
            Err(TurnFailure::Uncertain(anyhow!("duplicate was executed")))
        })
        .await?,
        WorkerStep::Idle
    );
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
async fn follow_up_turns_carry_only_the_runtime_cursor_beside_later_messages() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let first = accept(&state, "one", "conversation").await?;
    let cursor = serde_json::json!({"message": "native-message", "after": 1});
    let answered = cursor.clone();
    run_next_turn(&state, "test-runtime", async move |_, _, request| {
        Ok(RuntimeTurnResponse {
            follow_up: Some(answered),
            unanswered_left: false,
            activity: None,
            attachments_left: false,
            ..completed(request)
        })
    })
    .await?;
    let delivery = state
        .core
        .database
        .claim_delivery("test-interface", "instance")?
        .context("missing reply")?;
    assert_eq!(delivery.turn_id, first.turn_id);
    state.core.database.acknowledge_delivery(
        "test-interface",
        delivery.id,
        "instance",
        DeliveryOutcome::Delivered,
        None,
    )?;
    // The report is observed in its own lane: a later message of the same
    // conversation runs meanwhile, while its executable lane ignores it.
    let later = accept(&state, "two", "conversation").await?;
    run_next_turn(&state, "test-runtime", async move |_, _, request| {
        assert!(request.follow_up.is_none());
        Ok(completed(request))
    })
    .await?;
    assert_eq!(
        state
            .core
            .database
            .inspect_turn(&later.turn_id)?
            .context("later turn missing")?
            .state,
        TurnState::Completed
    );
    // A lost answer only asks again: the observation reads, it does not run work.
    run_next_observation(&state, "test-runtime", async move |_, _, _| {
        Err(TurnFailure::Unanswered(anyhow!("connection reset")))
    })
    .await?;
    // The observation runs with the cursor, no input and no files of its own.
    let seen = Arc::new(std::sync::Mutex::new(None));
    let record = seen.clone();
    run_next_observation(&state, "test-runtime", async move |_, _, request| {
        *record
            .lock()
            .map_err(|_| TurnFailure::Uncertain(anyhow!("poisoned")))? = Some(request.clone());
        Ok(RuntimeTurnResponse {
            correlation_id: request.correlation_id,
            output: "the report".to_owned(),
            follow_up: None,
            unanswered_left: false,
            activity: None,
            attachments_left: false,
        })
    })
    .await?;
    let request = seen
        .lock()
        .map_err(|_| anyhow!("poisoned"))?
        .clone()
        .context("follow-up turn did not run")?;
    assert_ne!(request.correlation_id, first.turn_id);
    assert_eq!(request.input, "");
    assert_eq!(request.audience_id, "conversation");
    assert_eq!(request.follow_up, Some(cursor));
    assert_eq!(request.native, serde_json::json!({}));
    assert!(request.attachments.is_empty() && request.unanswered.is_empty());
    let mut contents = Vec::new();
    while let Some(delivery) = state
        .core
        .database
        .claim_delivery("test-interface", "instance")?
    {
        state.core.database.acknowledge_delivery(
            "test-interface",
            delivery.id,
            "instance",
            DeliveryOutcome::Delivered,
            None,
        )?;
        contents.push(delivery.content);
    }
    assert_eq!(
        contents
            .iter()
            .filter(|content| *content == "the report")
            .count(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn an_observation_the_runtime_fails_without_explanation_ends() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    // An answer the runtime fails without explanation ends an observation
    // like any turn, rather than being asked forever.
    let third = accept(&state, "three", "elsewhere").await?;
    let cursor = serde_json::json!({"message": "native-message", "after": 2});
    run_next_turn(&state, "test-runtime", async move |_, _, request| {
        Ok(RuntimeTurnResponse {
            follow_up: Some(cursor),
            unanswered_left: false,
            activity: None,
            attachments_left: false,
            ..completed(request)
        })
    })
    .await?;
    run_next_observation(&state, "test-runtime", async move |_, _, _| {
        Err(TurnFailure::Uncertain(anyhow!("runtime failed with 500")))
    })
    .await?;
    let recent = state
        .core
        .recent_turns(Some(&format!("follow-up:{}", third.turn_id)), 1)?;
    assert_eq!(
        recent.first().map(|turn| turn.state),
        Some(TurnState::Interrupted)
    );

    Ok(())
}

#[tokio::test]
async fn an_interface_cannot_pose_as_cores_follow_up() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let mut headers = HeaderMap::new();
    headers.insert(PROVIDER_HEADER, "test-interface".parse()?);
    headers.insert(
        http::header::AUTHORIZATION,
        format!("Bearer {}", state.config.credential("test-interface")?).parse()?,
    );
    let native = serde_json::json!({ "follow_up": {"message": "x", "after": 0} });
    let _receipt = accept_event(
        State(configured(&state)),
        headers,
        Json(ExternalEvent {
            event_id: "forged".to_owned(),
            agent_id: state.config.value.agents[0].id.clone(),
            user_id: "test-user".to_owned(),
            audience_id: "conversation".to_owned(),
            delivery_target_id: "test-destination".to_owned(),
            feedback_target_id: "test-feedback".to_owned(),
            content: "hello".to_owned(),
            native: native.clone(),
            attachments: None,
        }),
    )
    .await
    .map_err(|error| anyhow!(error.message))?;
    // Native data is the interface's own: the event is a message like any
    // other, never an observation, and carries no cursor.
    assert_eq!(
        run_next_observation(&state, "test-runtime", async |_, _, _| {
            Err(TurnFailure::Uncertain(anyhow!("ran as an observation")))
        })
        .await?,
        WorkerStep::Idle
    );
    run_next_turn(&state, "test-runtime", async move |_, _, request| {
        assert_eq!(
            (request.native.clone(), request.follow_up.clone()),
            (native, None)
        );
        Ok(completed(request))
    })
    .await?;
    Ok(())
}

#[tokio::test]
async fn workers_hear_of_claimable_turns_in_their_own_lane() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let mut messages = state.core.work(Lane::Messages);
    let mut observations = state.core.work(Lane::Observations);
    let mut changed = || {
        let seen = (messages.has_changed().ok(), observations.has_changed().ok());
        messages.mark_unchanged();
        observations.mark_unchanged();
        seen
    };
    accept(&state, "one", "conversation").await?;
    assert_eq!(changed(), (Some(true), Some(false)));
    // A turn the runtime never received waits for its own worker's backoff;
    // waking the others would ask a down runtime in a tight loop.
    run_next_turn(&state, "test-runtime", async |_, _, _| {
        Err(TurnFailure::Retry(anyhow!("connection refused")))
    })
    .await?;
    assert_eq!(changed(), (Some(false), Some(false)));
    // A finished message frees its conversation's next one, and its cursor
    // queues an observation.
    run_next_turn(&state, "test-runtime", async |_, _, request| {
        Ok(RuntimeTurnResponse {
            follow_up: Some(serde_json::json!({"message": "m", "after": 1})),
            unanswered_left: false,
            activity: None,
            attachments_left: false,
            ..completed(request)
        })
    })
    .await?;
    assert_eq!(changed(), (Some(true), Some(true)));
    // An observation holds back no message.
    run_next_observation(&state, "test-runtime", async |_, _, request| {
        Ok(completed(request))
    })
    .await?;
    assert_eq!(changed(), (Some(false), Some(false)));
    Ok(())
}

/// An interface may name its events anything but the identities core gives
/// its own follow-up observations; otherwise one sent after a parent's
/// receipt would make the parent's completion fail, losing its answer.
#[tokio::test]
async fn an_interface_cannot_take_a_follow_up_identity_from_a_parent_turn() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let parent = accept(&state, "parent", "conversation").await?;
    let internal = format!("follow-up:{}", parent.turn_id);
    let claimed = accept(&state, &internal, "conversation").await;
    let step = run_next_turn(&state, "test-runtime", async |_, _, request| {
        Ok(RuntimeTurnResponse {
            follow_up: Some(json!({"after": 1})),
            ..completed(request)
        })
    })
    .await;
    // The parent completed with its answer, and its observation is queued.
    step?;
    let inspection = state
        .core
        .inspect_turn(&parent.turn_id)?
        .context("missing parent")?;
    assert_eq!(inspection.state, TurnState::Completed);
    assert_eq!(inspection.output.as_deref(), Some("reply to parent"));
    let observation = state
        .core
        .database
        .event_receipt("test-interface", &internal)?
        .context("missing observation")?;
    assert_eq!(observation.state, TurnState::Queued);
    assert_ne!(observation.turn_id, parent.turn_id);
    // The interface's event was refused at admission, before anything was recorded.
    let refusal = claimed
        .err()
        .context("an interface took a follow-up identity")?;
    assert!(refusal.to_string().contains("reserved"), "{refusal}");
    Ok(())
}

#[tokio::test]
async fn concurrent_workers_progress_other_conversations_and_keep_each_in_order() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let first = accept(&state, "a1", "conversation-a").await?;
    let second = accept(&state, "a2", "conversation-a").await?;
    let other = accept(&state, "b1", "conversation-b").await?;
    // One worker holds conversation A's first turn running.
    let (release, held) = tokio::sync::oneshot::channel::<()>();
    let (started, running) = tokio::sync::oneshot::channel::<String>();
    let worker = configured(&state);
    let long = tokio::spawn(async move {
        run_next(
            &worker,
            "test-runtime",
            Lane::Messages,
            &CancellationToken::new(),
            async move |_, _, request| {
                let _ = started.send(request.correlation_id.clone());
                let _ = held.await;
                Ok(completed(request))
            },
        )
        .await
    });
    assert_eq!(running.await?, first.turn_id);
    // Another worker meanwhile runs the other conversation's turn, never A's next one.
    let mut ran = Vec::new();
    let step = run_next_turn(&state, "test-runtime", async |_, _, request| {
        ran.push(request.correlation_id.clone());
        Ok(completed(request))
    })
    .await?;
    assert_eq!(step, WorkerStep::Advanced);
    assert_eq!(ran, std::slice::from_ref(&other.turn_id));
    assert_eq!(
        run_next_turn(&state, "test-runtime", async |_, _, _| {
            Err(TurnFailure::Uncertain(anyhow!(
                "A's next turn ran beside its first"
            )))
        })
        .await?,
        WorkerStep::Idle
    );
    let _ = release.send(());
    assert_eq!(long.await??, WorkerStep::Advanced);
    run_next_turn(&state, "test-runtime", async |_, _, request| {
        ran.push(request.correlation_id.clone());
        Ok(completed(request))
    })
    .await?;
    assert_eq!(ran, [other.turn_id, second.turn_id]);
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
    assert_eq!(
        run_next_turn(&state, "test-runtime", async |_, _, request| {
            assert_eq!(request.correlation_id, first.turn_id);
            assert!(
                Path::new(&request.resource_root)
                    .join("SYSTEM.md")
                    .is_file()
            );
            Ok(completed(request))
        })
        .await?,
        WorkerStep::Advanced
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

#[tokio::test]
async fn unavailable_conversation_owner_rejects_input_as_configuration_conflict() -> Result<()> {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt as _;

    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    let original = accept(&state, "original", "pinned").await?;
    let mut replacement = state.config.value.providers[1].clone();
    replacement.id = "replacement".to_owned();
    state.config.value.providers.push(replacement);
    state.config.value.agents[0].runtime = "replacement".to_owned();
    state.config.value.providers[1].enabled = false;
    for id in ["disabled", "removed"] {
        let router = Router::new()
            .route("/v1/events", post(accept_event))
            .with_state(configured(&state));
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/events")
                    .header(http::header::CONTENT_TYPE, "application/json")
                    .header(http::header::AUTHORIZATION, "Bearer test-only")
                    .header(PROVIDER_HEADER, "test-interface")
                    .body(Body::from(serde_json::to_vec(&ExternalEvent {
                        event_id: id.to_owned(),
                        agent_id: "test-agent".to_owned(),
                        user_id: "test-user".to_owned(),
                        audience_id: "pinned".to_owned(),
                        delivery_target_id: "target".to_owned(),
                        feedback_target_id: "feedback".to_owned(),
                        content: "new input".to_owned(),
                        native: Value::Null,
                        attachments: None,
                    })?))?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert!(
            state
                .core
                .database
                .event_receipt("test-interface", id)?
                .is_none()
        );
        // Other conversations still accept input; recovery of known receipts still works.
        accept(&state, id, "unrelated").await?;
        assert!(accept(&state, "original", "pinned").await?.duplicate);
        assert_eq!(
            state
                .core
                .database
                .inspect_turn(&original.turn_id)?
                .context("missing pinned turn")?
                .state,
            TurnState::Queued
        );
        state
            .config
            .value
            .providers
            .retain(|provider| provider.id != "test-runtime");
    }
    Ok(())
}

#[tokio::test]
async fn startup_interrupts_old_runtime_work_without_replaying_it() -> Result<()> {
    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    let running = accept(&state, "interrupted", "blocked").await?;
    state
        .core
        .database
        .claim("test-runtime", Lane::Messages, &CancellationToken::new())?
        .context("missing claim")?;
    let blocked = accept(&state, "successor", "blocked").await?;
    let queued = accept(&state, "queued", "old-conversation").await?;
    let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
    let mut services = tokio::task::JoinSet::new();
    for id in ["test-runtime", "replacement"] {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let mut provider = state
            .config
            .provider("test-runtime", ProviderRole::Runtime)?
            .clone();
        provider.id = id.to_owned();
        provider.url = format!("http://{}", listener.local_addr()?);
        fs::write(
            directory.path().join(format!("credentials/{id}.token")),
            "test-only",
        )?;
        if id == "test-runtime" {
            state.config.value.providers[1] = provider;
        } else {
            state.config.value.providers.push(provider);
        }
        let sent = sent.clone();
        let app = Router::new().route(
            "/v1/turns",
            post(move |Json(request): Json<RuntimeTurnRequest>| {
                let sent = sent.clone();
                async move {
                    let _ = sent.send((id, request.correlation_id.clone()));
                    Json(completed(request))
                }
            }),
        );
        services.spawn(async move { axum::serve(listener, app).await });
    }
    state.config.value.agents[0].runtime = "replacement".to_owned();
    let fresh = accept(&state, "fresh", "new-conversation").await?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    state.config.value.core.listen = listener.local_addr()?;
    drop(listener);
    // Exercise the real startup worker selection and recovery against reopened SQLite.
    let config = state.config.clone();
    services.spawn(async move {
        serve(config.root.join("carillon.toml"), config)
            .await
            .map_err(std::io::Error::other)
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        assert_eq!(
            received.recv().await.context("runtime trace ended")?,
            ("replacement", fresh.turn_id.clone())
        );
        loop {
            let counts = state.core.database.turn_counts()?;
            if counts.completed == 1 && counts.interrupted == 3 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            state
                .core
                .database
                .inspect_turn(&running.turn_id)?
                .context("interrupted turn missing")?
                .state,
            TurnState::Interrupted
        );
        for receipt in [&blocked, &queued] {
            let turn = state
                .core
                .inspect_turn(&receipt.turn_id)?
                .context("missing old turn")?;
            assert_eq!(turn.state, TurnState::Interrupted);
            assert!(
                turn.error
                    .context("missing explanation")?
                    .contains("Start a new conversation.")
            );
        }
        assert_eq!(received.try_recv(), Err(TryRecvError::Empty));
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    services.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn definite_connection_failure_requeues_the_same_turn() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let receipt = accept(&state, "one", "conversation").await?;
    assert_eq!(
        run_next_turn(&state, "test-runtime", async |_, _, request| {
            assert_eq!(request.correlation_id, receipt.turn_id);
            Err(TurnFailure::Retry(anyhow!("connection refused")))
        })
        .await?,
        WorkerStep::Backoff
    );
    assert_eq!(
        state
            .core
            .database
            .inspect_turn(&receipt.turn_id)?
            .context("missing turn")?
            .state,
        TurnState::Queued
    );
    assert!(
        state
            .core
            .database
            .claim_delivery("test-interface", "instance")?
            .is_none()
    );
    run_next_turn(&state, "test-runtime", async |_, _, request| {
        assert_eq!(request.correlation_id, receipt.turn_id);
        Ok(completed(request))
    })
    .await?;
    assert_eq!(state.core.database.turn_counts()?.completed, 1);
    Ok(())
}

#[tokio::test]
async fn interruption_allows_the_next_message_without_replaying() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let uncertain = accept(&state, "one", "blocked").await?;
    let successor = accept(&state, "two", "blocked").await?;
    run_next_turn(&state, "test-runtime", async |_, _, _| {
        Err(TurnFailure::Uncertain(anyhow!("response lost")))
    })
    .await?;
    assert_eq!(
        state
            .core
            .database
            .inspect_turn(&uncertain.turn_id)?
            .context("missing turn")?
            .state,
        TurnState::Interrupted
    );
    run_next_turn(&state, "test-runtime", async |_, _, request| {
        assert_eq!(request.correlation_id, successor.turn_id);
        Ok(completed(request))
    })
    .await?;
    accept(&state, "three", "unrelated").await?;
    run_next_turn(&state, "test-runtime", async |_, _, request| {
        assert_eq!(request.audience_id, "unrelated");
        Ok(completed(request))
    })
    .await?;
    assert_eq!(state.core.database.turn_counts()?.completed, 2);
    assert_eq!(state.core.database.turn_counts()?.interrupted, 1);
    Ok(())
}

#[tokio::test]
async fn a_stopping_worker_settles_its_turn_and_claims_no_other() -> Result<()> {
    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    let held = accept(&state, "held", "first").await?;
    let queued = accept(&state, "queued", "second").await?;
    // The runtime answers a turn only once released, counting its calls.
    let calls = Arc::new(AtomicUsize::new(0));
    let (called, release) = (
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
    );
    let app = Router::new().route(
        "/v1/turns",
        post({
            let (calls, called, release) = (calls.clone(), called.clone(), release.clone());
            move |Json(request): Json<Value>| async move {
                calls.fetch_add(1, Ordering::SeqCst);
                called.notify_one();
                release.notified().await;
                Json(json!({"output": "answered", "correlation_id": request["correlation_id"]}))
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    state.config.value.providers[1].url = format!("http://{}", listener.local_addr()?);
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let stopping = CancellationToken::new();
    let worker = tokio::spawn(runtime_worker(
        configured(&state),
        "test-runtime".to_owned(),
        Lane::Messages,
        stopping.clone(),
        stopping.clone(),
    ));
    tokio::time::timeout(Duration::from_secs(5), called.notified()).await?;
    // Shutdown begins while the first turn runs.
    stopping.cancel();
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), worker).await??;
    server.abort();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let state_of = |turn: &str| -> Result<TurnState> {
        Ok(state
            .core
            .inspect_turn(turn)?
            .context("missing turn")?
            .state)
    };
    // The held turn settled; the queued one waits for the next start.
    assert_eq!(state_of(&held.turn_id)?, TurnState::Completed);
    assert_eq!(state_of(&queued.turn_id)?, TurnState::Queued);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_claim_waiting_for_the_database_writer_is_not_made_once_stopping() -> Result<()> {
    use std::io::Write as _;

    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    let queued = accept(&state, "queued", "first").await?;
    let calls = Arc::new(AtomicUsize::new(0));
    let app = Router::new().route(
        "/v1/turns",
        post({
            let calls = calls.clone();
            move |Json(request): Json<Value>| async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Json(json!({"output": "answered", "correlation_id": request["correlation_id"]}))
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    state.config.value.providers[1].url = format!("http://{}", listener.local_addr()?);
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    // Another connection holds the database writer.
    let writer = rusqlite::Connection::open(state.config.value.data_dir.join("carillon.sqlite3"))?;
    writer.execute_batch("BEGIN IMMEDIATE")?;
    // The worker reads its runtime credential after checking whether core is
    // stopping, and checks nothing more before its storage work claims; read
    // from a FIFO, it waits for the test. Opening the FIFO thus shows the
    // worker is past that check, and the held writer keeps the claim from
    // being made until the test releases the writer after shutdown has
    // begun. The test does not observe whether that storage work has already
    // reached its wait for the writer.
    let credential = directory.path().join("credentials/test-runtime.token");
    fs::remove_file(&credential)?;
    rustix::fs::mknodat(
        rustix::fs::CWD,
        &credential,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o600),
        0,
    )?;
    let stopping = CancellationToken::new();
    let worker = tokio::spawn(runtime_worker(
        configured(&state),
        "test-runtime".to_owned(),
        Lane::Messages,
        stopping.clone(),
        stopping.clone(),
    ));
    let mut fifo =
        tokio::task::spawn_blocking(move || fs::OpenOptions::new().write(true).open(credential))
            .await??;
    fifo.write_all(b"test-only")?;
    drop(fifo);
    stopping.cancel();
    writer.execute_batch("ROLLBACK")?;
    tokio::time::timeout(Duration::from_secs(10), worker).await??;
    server.abort();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "a turn was sent after stopping began"
    );
    let turn = state
        .core
        .inspect_turn(&queued.turn_id)?
        .context("missing turn")?;
    assert_eq!(turn.state, TurnState::Queued);
    Ok(())
}

async fn failing_runtime(
    state: &Fixture,
    status: StatusCode,
    body: Value,
) -> Result<(
    Provider,
    Arc<AtomicUsize>,
    tokio::task::JoinHandle<std::io::Result<()>>,
)> {
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    let app = Router::new().route(
        "/v1/turns",
        post(move || {
            counted.fetch_add(1, Ordering::SeqCst);
            let body = body.clone();
            async move { (status, Json(body)) }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let mut runtime = state
        .config
        .provider("test-runtime", ProviderRole::Runtime)?
        .clone();
    runtime.url = format!("http://{}", listener.local_addr()?);
    Ok((
        runtime,
        calls,
        tokio::spawn(async move { axum::serve(listener, app).await }),
    ))
}

#[tokio::test]
async fn runtime_explanation_reaches_the_conversation_once_and_it_continues() -> Result<()> {
    let explanation = "Codex error: The usage limit has been reached";
    for (status, body, notice) in [
        (
            StatusCode::BAD_GATEWAY,
            json!({"error": explanation}),
            format!(
                "That turn was interrupted: {explanation}\nSend another message to continue in this conversation. Actions already taken may have had effects."
            ),
        ),
        // An internal runtime failure carries no explanation, so the
        // conversation gets the generic notice rather than any diagnostic.
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({}),
            "That turn was interrupted. Send another message to continue in this conversation. Actions already taken may have had effects.".to_owned(),
        ),
    ] {
        let directory = tempdir()?;
        let state = installation(directory.path())?;
        let failed = accept(&state, "one", "conversation").await?;
        let successor = accept(&state, "two", "conversation").await?;
        let (runtime, calls, server) = failing_runtime(&state, status, body).await?;
        assert_eq!(
            run_next_turn(&state, "test-runtime", async |_, token, request| {
                call_runtime(&state.client, &runtime, token, request).await
            })
            .await?,
            WorkerStep::Advanced
        );
        server.abort();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "failed work must not be replayed");
        let inspection = state
            .core
            .inspect_turn(&failed.turn_id)?
            .context("missing turn")?;
        assert_eq!(inspection.state, TurnState::Interrupted);
        let delivery = state
            .core
            .claim_delivery("test-interface", "instance")?
            .context("missing notice")?;
        assert_eq!(delivery.turn_id, failed.turn_id);
        assert_eq!(delivery.turn_state, TurnState::Interrupted);
        assert_eq!(delivery.content, notice);
        if status == StatusCode::BAD_GATEWAY {
            assert_eq!(
                inspection.error.as_deref(),
                Some("runtime reported failure: Codex error: The usage limit has been reached")
            );
        }
        run_next_turn(&state, "test-runtime", async |_, _, request| {
            assert_eq!(request.correlation_id, successor.turn_id);
            Ok(completed(request))
        })
        .await?;
        assert_eq!(state.core.turn_counts()?.completed, 1);
    }
    Ok(())
}

#[tokio::test]
async fn failed_outcome_write_waits_for_restart_without_blocking_other_conversations() -> Result<()>
{
    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    let failed = accept(&state, "one", "blocked").await?;
    let successor = accept(&state, "two", "blocked").await?;
    let unrelated = accept(&state, "three", "unrelated").await?;
    let path = state.config.value.data_dir.join("carillon.sqlite3");
    let connection = rusqlite::Connection::open(&path)?;
    // Reject outcome writes, not the claim. Both completion and its fallback
    // uncertainty write encounter the same temporary database failure.
    connection.execute_batch(
        "CREATE TRIGGER fail_outcome BEFORE UPDATE ON turns
         WHEN OLD.state = 'running'
         BEGIN SELECT RAISE(FAIL, 'injected outcome write failure'); END;",
    )?;
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        run_next_turn(&state, "test-runtime", async |_, _, request| {
            assert_eq!(request.correlation_id, failed.turn_id);
            Ok(completed(request))
        }),
    )
    .await?;
    let error = result.err().context("outcome write must fail")?;
    assert!(format!("{error:#}").contains("injected outcome write failure"));
    connection.execute_batch("DROP TRIGGER fail_outcome")?;
    drop(connection);

    // Restoring database writes lets other conversations progress; it does
    // not make replaying the abandoned running turn safe.
    run_next_turn(&state, "test-runtime", async |_, _, request| {
        assert_eq!(request.correlation_id, unrelated.turn_id);
        Ok(completed(request))
    })
    .await?;
    assert_eq!(
        state
            .core
            .database
            .inspect_turn(&failed.turn_id)?
            .context("missing turn")?
            .state,
        TurnState::Running
    );
    assert_eq!(
        run_next_turn(&state, "test-runtime", async |_, _, _| {
            Err(TurnFailure::Uncertain(anyhow!("blocked work was replayed")))
        })
        .await?,
        WorkerStep::Idle
    );

    state.core.database = Database::open(&path)?;
    assert_eq!(state.core.database.recover_running_turns()?, 1);
    run_next_turn(&state, "test-runtime", async |_, _, request| {
        assert_eq!(request.correlation_id, successor.turn_id);
        Ok(completed(request))
    })
    .await?;
    assert_eq!(state.core.database.turn_counts()?.completed, 2);
    Ok(())
}

#[tokio::test]
async fn cancelled_execution_is_interrupted_after_reopening_while_queued_work_resumes() -> Result<()>
{
    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    let running = accept(&state, "one", "blocked").await?;
    let successor = accept(&state, "two", "blocked").await?;
    let queued = accept(&state, "three", "unrelated").await?;
    let (admitted, received) = tokio::sync::oneshot::channel();
    let worker = state.clone();
    let task = tokio::spawn(async move {
        run_next_turn(&worker, "test-runtime", async move |_, _, _| {
            let _ = admitted.send(());
            std::future::pending().await
        })
        .await
    });
    received.await?;
    task.abort();
    assert!(
        task.await
            .err()
            .context("worker unexpectedly finished")?
            .is_cancelled()
    );
    state.core.database = Database::open(&state.config.value.data_dir.join("carillon.sqlite3"))?;
    assert_eq!(state.core.database.recover_running_turns()?, 1);
    assert_eq!(
        state
            .core
            .database
            .inspect_turn(&running.turn_id)?
            .context("missing turn")?
            .state,
        TurnState::Interrupted
    );
    run_next_turn(&state, "test-runtime", async |_, _, request| {
        assert_eq!(request.correlation_id, successor.turn_id);
        Ok(completed(request))
    })
    .await?;
    run_next_turn(&state, "test-runtime", async |_, _, request| {
        assert_eq!(request.correlation_id, queued.turn_id);
        Ok(completed(request))
    })
    .await?;
    assert_eq!(
        run_next_turn(&state, "test-runtime", async |_, _, _| {
            Err(TurnFailure::Uncertain(anyhow!(
                "restart replayed ambiguous work"
            )))
        })
        .await?,
        WorkerStep::Idle
    );
    Ok(())
}

#[tokio::test]
async fn empty_runtime_output_interrupts_without_empty_delivery_or_replay() -> Result<()> {
    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    state.config.value.providers[1].url = format!("http://{}", listener.local_addr()?);
    let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
    let app = Router::new().route(
        "/v1/turns",
        post(move |Json(request): Json<RuntimeTurnRequest>| {
            let sent = sent.clone();
            async move {
                let _ = sent.send(request.correlation_id.clone());
                Json(RuntimeTurnResponse {
                    correlation_id: request.correlation_id,
                    output: if request.input == "empty" {
                        ""
                    } else {
                        " \t\n"
                    }
                    .to_owned(),
                    follow_up: None,
                    unanswered_left: false,
                    activity: None,
                    attachments_left: false,
                })
            }
        }),
    );
    let mut services = tokio::task::JoinSet::new();
    services.spawn(async move { axum::serve(listener, app).await });
    for input in ["empty", "whitespace"] {
        let receipt = accept(&state, input, input).await?;
        let successor = accept(&state, &format!("after-{input}"), input).await?;
        tokio::time::timeout(
            Duration::from_secs(5),
            run_next_turn(&state, "test-runtime", async |runtime, token, request| {
                call_runtime(&state.client, runtime, token, request).await
            }),
        )
        .await??;
        assert_eq!(received.try_recv()?, receipt.turn_id);
        let inspection = state
            .core
            .database
            .inspect_turn(&receipt.turn_id)?
            .context("missing turn")?;
        assert_eq!(
            inspection.state,
            TurnState::Interrupted,
            "empty output was completed"
        );
        let feedback = state
            .core
            .database
            .claim_delivery("test-interface", "instance")?
            .context("missing uncertainty feedback")?;
        assert_eq!(feedback.turn_id, receipt.turn_id);
        assert!(feedback.content.contains("interrupted"));
        state.core.database.acknowledge_delivery(
            "test-interface",
            feedback.id,
            "instance",
            DeliveryOutcome::Delivered,
            None,
        )?;
        run_next_turn(&state, "test-runtime", async |_, _, request| {
            assert_eq!(request.correlation_id, successor.turn_id);
            Ok(completed(request))
        })
        .await?;
        let delivery = state
            .core
            .database
            .claim_delivery("test-interface", "instance")?
            .context("missing successor delivery")?;
        assert_eq!(delivery.turn_id, successor.turn_id);
        state.core.database.acknowledge_delivery(
            "test-interface",
            delivery.id,
            "instance",
            DeliveryOutcome::Delivered,
            None,
        )?;
    }
    assert_eq!(received.try_recv(), Err(TryRecvError::Empty));
    Ok(())
}

#[tokio::test]
async fn a_closing_runtime_gives_no_answer_rather_than_a_failure() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    accept(&state, "one", "conversation").await?;
    let turn = state
        .core
        .database
        .claim("test-runtime", Lane::Messages, &CancellationToken::new())?
        .context("turn missing")?;
    let mut runtime = state
        .config
        .provider("test-runtime", ProviderRole::Runtime)?
        .clone();
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    runtime.url = format!("http://{}", listener.local_addr()?);
    let app = Router::new().route(
        "/v1/turns",
        post(|| async { (StatusCode::SERVICE_UNAVAILABLE, Json(json!({}))) }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let request = state.core.runtime_request(&turn, Vec::new());
    let outcome = call_runtime(&state.client, &runtime, "unused", request).await;
    // Work may have run, so it is not `Retry`; an observation is asked again.
    assert!(
        matches!(outcome, Err(TurnFailure::Unanswered(_))),
        "{outcome:?}"
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn runtime_http_boundary_classifies_and_validates_responses() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    accept(&state, "one", "conversation").await?;
    let turn = state
        .core
        .database
        .claim("test-runtime", Lane::Messages, &CancellationToken::new())?
        .context("missing claim")?;
    let mut runtime = state
        .config
        .provider("test-runtime", ProviderRole::Runtime)?
        .clone();
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    runtime.url = format!("http://{}", listener.local_addr()?);
    let app = Router::new().route(
        "/v1/turns",
        post(|Json(request): Json<RuntimeTurnRequest>| async move {
            match request.input.as_str() {
                "forbidden" => StatusCode::FORBIDDEN.into_response(),
                "server error" => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                "unexplained" => {
                    (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({}))).into_response()
                }
                "blank" => (StatusCode::BAD_GATEWAY, Json(json!({"error": " \n"}))).into_response(),
                "explained" => (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({"error": "The usage limit has been reached"})),
                )
                    .into_response(),
                "malformed" => "not json".into_response(),
                "mismatch" => {
                    Json(json!({"correlation_id": "other", "output": "reply"})).into_response()
                }
                _ => Json(completed(request)).into_response(),
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    // These responses carry no protocol proof of non-admission. This is not
    // a blanket rule for a future explicit rejected-before-execution response.
    for input in [
        "forbidden",
        "server error",
        "unexplained",
        "blank",
        "malformed",
        "mismatch",
    ] {
        let mut request = state.core.runtime_request(&turn, Vec::new());
        request.input = input.to_owned();
        let outcome = call_runtime(&state.client, &runtime, "unused", request).await;
        assert!(
            matches!(outcome, Err(TurnFailure::Uncertain(_))),
            "{input}: {outcome:?}"
        );
    }
    // An explanation is not proof that no model work ran: it is not `Retry`.
    let mut request = state.core.runtime_request(&turn, Vec::new());
    request.input = "explained".to_owned();
    let outcome = call_runtime(&state.client, &runtime, "unused", request).await;
    let Err(TurnFailure::Reported(reported)) = &outcome else {
        anyhow::bail!("{outcome:?}");
    };
    let explained = (reported.status, reported.explanation.as_deref());
    assert_eq!(explained, (502, Some("The usage limit has been reached")));
    let response = call_runtime(
        &state.client,
        &runtime,
        "unused",
        state.core.runtime_request(&turn, Vec::new()),
    )
    .await
    .map_err(|failure| match failure {
        TurnFailure::Retry(error)
        | TurnFailure::Unanswered(error)
        | TurnFailure::Uncertain(error) => error,
        TurnFailure::Reported(reported) => anyhow!("{reported:?}"),
    })?;
    assert_eq!(response.correlation_id, turn.turn_id);
    server.abort();
    // Reserve a local TCP port without listening, so refusal cannot hit another service.
    let socket = tokio::net::TcpSocket::new_v4()?;
    socket.bind("127.0.0.1:0".parse()?)?;
    runtime.url = format!("http://{}", socket.local_addr()?);
    assert!(matches!(
        call_runtime(
            &state.client,
            &runtime,
            "unused",
            state.core.runtime_request(&turn, Vec::new())
        )
        .await,
        Err(TurnFailure::Retry(_))
    ));
    Ok(())
}

#[test]
fn check_opens_storage_like_the_service_without_writing() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    check(&state.config)?;
    // State from a release with another schema is refused, and left as it was.
    let database = state.config.value.data_dir.join("carillon.sqlite3");
    rusqlite::Connection::open(&database)?
        .execute_batch("CREATE TABLE later_release (id INTEGER)")?;
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
                delivery_target_id: "foreign-destination".to_owned(),
                feedback_target_id: "foreign-feedback".to_owned(),
                content: "not yours".to_owned(),
                native: Value::Null,
                attachments: None,
            },
        )
        .await?)
}

/// Claims the next turn, which must be `turn_id`, and keeps it running until released.
async fn hold_running(
    state: &Fixture,
    runtime_id: &str,
    turn_id: &str,
) -> Result<(
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<Result<WorkerStep>>,
)> {
    let (release, held) = tokio::sync::oneshot::channel::<()>();
    let (worker_state, runtime_id) = (state.clone(), runtime_id.to_owned());
    let worker = tokio::spawn(async move {
        run_next_turn(&worker_state, &runtime_id, async |_, _, request| {
            let _ = held.await;
            Ok(completed(request))
        })
        .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while state
            .core
            .database
            .inspect_turn(turn_id)?
            .map(|turn| turn.state)
            != Some(TurnState::Running)
        {
            tokio::task::yield_now().await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok((release, worker))
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
    run_next_turn(&state, "test-runtime", async |_, _, request| {
        Ok(RuntimeTurnResponse {
            activity: Some(report(2).map_err(TurnFailure::Uncertain)?),
            ..completed(request)
        })
    })
    .await?;
    let running = accept(&state, "running", "conversation").await?;
    let queued = accept(&state, "queued", "conversation").await?;
    foreign_turn(&mut state, directory.path()).await?;
    let (release, worker) = hold_running(&state, "test-runtime", &running.turn_id).await?;

    let (status, body) = jobs_call(&state, "test-interface", Some("test-only"), "").await?;
    assert_eq!(status, StatusCode::OK);
    let accepted = |index: usize| body["jobs"][index]["origin"]["accepted_at"].clone();
    assert_eq!(
        body,
        json!({"jobs": [
            {
                "origin": {"turn_id": running.turn_id, "accepted_at": accepted(0)},
                "target_id": "test-destination",
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

    let _ = release.send(());
    assert_eq!(worker.await??, WorkerStep::Advanced);
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
    run_next_turn(&state, "test-runtime", async |_, _, request| {
        Ok(RuntimeTurnResponse {
            activity: Some(report(2).map_err(TurnFailure::Uncertain)?),
            ..completed(request)
        })
    })
    .await?;
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
    let mut held = Vec::new();
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
        held.push(hold_running(&state, &id, &receipt.turn_id).await?);
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
    for (release, worker) in held {
        let _ = release.send(());
        worker.await??;
    }
    Ok(())
}
mod attachments;
mod live;
mod maintenance;

/// A runtime endpoint written at the socket level: while `cut` is set it sends
/// a successful answer's headers and only the start of its body, then closes;
/// otherwise it answers the request whole, with `output`.
async fn cut_off_runtime(
    cut: Arc<std::sync::atomic::AtomicBool>,
    output: &'static str,
) -> Result<(String, tokio::task::JoinHandle<()>)> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let server = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut request = Vec::new();
            let mut buffer = [0_u8; 8192];
            // Read the headers, then as much body as they announce.
            let body = loop {
                let Ok(read) = socket.read(&mut buffer).await else {
                    break None;
                };
                if read == 0 {
                    break None;
                }
                request.extend_from_slice(&buffer[..read]);
                let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                let length = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if request.len() >= end + 4 + length {
                    break Some(request[end + 4..end + 4 + length].to_vec());
                }
            };
            let Some(body) = body else { continue };
            let correlation = serde_json::from_slice::<Value>(&body)
                .ok()
                .and_then(|request| request["correlation_id"].as_str().map(str::to_owned))
                .unwrap_or_default();
            let answer =
                serde_json::to_string(&json!({"correlation_id": correlation, "output": output}))
                    .unwrap_or_default();
            let (announced, body) = if cut.load(Ordering::SeqCst) {
                (answer.len() + 100, &answer.as_bytes()[..answer.len() / 2])
            } else {
                (answer.len(), answer.as_bytes())
            };
            let headers = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {announced}\r\nconnection: close\r\n\r\n"
            );
            let _ = socket.write_all(&[headers.as_bytes(), body].concat()).await;
            let _ = socket.shutdown().await;
        }
    });
    Ok((url, server))
}

#[tokio::test]
async fn a_cut_off_answer_asks_an_observation_again_but_never_replays_an_executable_turn()
-> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let first = accept(&state, "one", "conversation").await?;
    let cursor = serde_json::json!({"message": "native-message", "after": 1});
    run_next_turn(&state, "test-runtime", async move |_, _, request| {
        Ok(RuntimeTurnResponse {
            follow_up: Some(cursor),
            unanswered_left: false,
            activity: None,
            attachments_left: false,
            ..completed(request)
        })
    })
    .await?;
    let answer = state
        .core
        .database
        .claim_delivery("test-interface", "instance")?
        .context("missing reply")?;
    state.core.database.acknowledge_delivery(
        "test-interface",
        answer.id,
        "instance",
        DeliveryOutcome::Delivered,
        None,
    )?;
    let cut = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let (url, server) = cut_off_runtime(cut.clone(), "the report").await?;
    let mut runtime = state
        .config
        .provider("test-runtime", ProviderRole::Runtime)?
        .clone();
    runtime.url = url;
    let observation = format!("follow-up:{}", first.turn_id);
    let state_of = |event: &str| -> Result<Option<TurnState>> {
        Ok(state
            .core
            .recent_turns(Some(event), 1)?
            .first()
            .map(|turn| turn.state))
    };

    // The answer's body is cut off: the observation goes back to its queue,
    // with nothing delivered.
    run_next_observation(&state, "test-runtime", async |_, _, request| {
        call_runtime(&state.client, &runtime, "unused", request).await
    })
    .await?;
    assert_eq!(state_of(&observation)?, Some(TurnState::Queued));
    assert!(
        state
            .core
            .database
            .claim_delivery("test-interface", "instance")?
            .is_none()
    );
    // Asked again, it completes and its report is delivered once.
    cut.store(false, Ordering::SeqCst);
    run_next_observation(&state, "test-runtime", async |_, _, request| {
        call_runtime(&state.client, &runtime, "unused", request).await
    })
    .await?;
    assert_eq!(state_of(&observation)?, Some(TurnState::Completed));
    let report = state
        .core
        .database
        .claim_delivery("test-interface", "instance")?
        .context("missing report")?;
    assert_eq!(report.content, "the report");

    // An executable turn whose answer is cut off is interrupted, never replayed.
    cut.store(true, Ordering::SeqCst);
    accept(&state, "two", "elsewhere").await?;
    run_next_turn(&state, "test-runtime", async |_, _, request| {
        call_runtime(&state.client, &runtime, "unused", request).await
    })
    .await?;
    assert_eq!(state_of("two")?, Some(TurnState::Interrupted));
    server.abort();
    Ok(())
}

/// One async worker, as when every worker could be taken: another writer
/// holds SQLite's write lock while an interface claims a delivery and a
/// runtime worker claims a turn. Their storage waits run beside the async
/// worker, so unrelated requests are answered meanwhile, and both claims
/// complete once the lock is released.
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
        tokio::spawn(async move {
            run_next_turn(&state, "test-runtime", async |_, _, request| {
                Ok(completed(request))
            })
            .await
        })
    };
    // Both claims are now waiting for the lock.
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
    let step = tokio::time::timeout(Duration::from_secs(5), worker).await???;
    assert_eq!(step, WorkerStep::Advanced);
    Ok(())
}

/// Storage work started for an async caller finishes even when the caller
/// is stopped, as a worker is at shutdown, and the service waits for it
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
            core.database
                .claim("test-runtime", Lane::Messages, &CancellationToken::new())
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

#[tokio::test(start_paused = true)]
async fn a_turn_takes_as_long_as_its_runtime_and_ends_when_its_caller_stops() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    accept(&state, "one", "conversation").await?;
    let turn = state
        .core
        .database
        .claim("test-runtime", Lane::Messages, &CancellationToken::new())?
        .context("missing claim")?;
    let mut runtime = state
        .config
        .provider("test-runtime", ProviderRole::Runtime)?
        .clone();
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    runtime.url = format!("http://{}", listener.local_addr()?);
    // The runtime's native work runs past half an hour, in the paused
    // clock's virtual time; it reports when its request is dropped.
    let (ended, mut endings) = tokio::sync::mpsc::unbounded_channel();
    let (began, mut beginnings) = tokio::sync::mpsc::unbounded_channel();
    let app = Router::new().route(
        "/v1/turns",
        post(move |Json(request): Json<RuntimeTurnRequest>| {
            let (ended, began) = (ended.clone(), began.clone());
            async move {
                struct Ending(tokio::sync::mpsc::UnboundedSender<()>);
                impl Drop for Ending {
                    fn drop(&mut self) {
                        let _ = self.0.send(());
                    }
                }
                let _ending = Ending(ended);
                let _ = began.send(());
                tokio::time::sleep(Duration::from_mins(31)).await;
                Json(completed(request))
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let started = tokio::time::Instant::now();
    let response = call_runtime(
        &state.client,
        &runtime,
        "unused",
        state.core.runtime_request(&turn, Vec::new()),
    )
    .await;
    // No transport deadline cuts the work short.
    assert!(
        matches!(&response, Ok(answer) if answer.correlation_id == turn.turn_id),
        "{response:?}"
    );
    assert!(started.elapsed() >= Duration::from_mins(31));
    beginnings
        .recv()
        .await
        .context("the answered request never began")?;
    endings
        .recv()
        .await
        .context("the answered request did not end")?;
    // A caller that stops, as a stopping worker does, ends the request at
    // the runtime, which takes it as cancellation.
    let request = state.core.runtime_request(&turn, Vec::new());
    let (client, cancelled) = (state.client.clone(), runtime.clone());
    let call =
        tokio::spawn(async move { call_runtime(&client, &cancelled, "unused", request).await });
    beginnings
        .recv()
        .await
        .context("the request never reached the runtime")?;
    call.abort();
    tokio::time::timeout(Duration::from_secs(5), endings.recv())
        .await
        .context("the runtime kept working after its caller stopped")?
        .context("the runtime's request did not end")?;
    server.abort();
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
    run_next_turn(&state, "test-runtime", async |_, _, request| {
        Ok(RuntimeTurnResponse {
            activity: Some(report(3).map_err(TurnFailure::Uncertain)?),
            ..completed(request)
        })
    })
    .await?;
    let answered = next_delivery()?;
    assert_eq!(answered.turn_state, TurnState::Completed);
    assert_eq!(reported(&answered.turn_id)?, Some(report(3)?));
    // A failed turn's report, though the runtime could show no explanation.
    accept(&state, "failed", "conversation").await?;
    run_next_turn(&state, "test-runtime", async |_, _, _| {
        Err(TurnFailure::Reported(Box::new(Reported {
            status: 500,
            explanation: None,
            activity: Some(report(1).map_err(TurnFailure::Uncertain)?),
        })))
    })
    .await?;
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
        Some("runtime provider failed the turn with 500 and no explanation")
    );
    // A report over its bound is not kept, and costs the turn nothing.
    accept(&state, "oversized", "conversation").await?;
    run_next_turn(&state, "test-runtime", async |_, _, request| {
        let mut activity = report(1).map_err(TurnFailure::Uncertain)?;
        activity.notes = vec!["語".repeat(500); 8];
        activity.recent[0].detail = Some("d".repeat(8_000));
        Ok(RuntimeTurnResponse {
            activity: Some(activity),
            ..completed(request)
        })
    })
    .await?;
    let oversized = next_delivery()?;
    assert_eq!(oversized.turn_state, TurnState::Completed);
    assert_eq!(oversized.content, "reply to oversized");
    assert_eq!(reported(&oversized.turn_id)?, None);
    Ok(())
}

/// A runtime's failure carries the turn's report with its explanation or
/// without one; a closing runtime's answer is still no answer, report or
/// not; and an invalid report is dropped without costing an answer.
#[tokio::test]
async fn a_failed_turns_report_crosses_the_runtime_boundary_with_or_without_an_explanation()
-> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    accept(&state, "one", "conversation").await?;
    let turn = state
        .core
        .database
        .claim("test-runtime", Lane::Messages, &CancellationToken::new())?
        .context("missing claim")?;
    let mut runtime = state
        .config
        .provider("test-runtime", ProviderRole::Runtime)?
        .clone();
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    runtime.url = format!("http://{}", listener.local_addr()?);
    let app = Router::new().route(
        "/v1/turns",
        post(|Json(request): Json<RuntimeTurnRequest>| async move {
            let failure = |status, body| (status, Json(body)).into_response();
            match request.input.as_str() {
                "reported" => failure(
                    StatusCode::BAD_GATEWAY,
                    json!({"error": "The build broke.", "activity": report_value(1)}),
                ),
                "report only" => failure(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({"activity": report_value(2)}),
                ),
                "closing" => failure(
                    StatusCode::SERVICE_UNAVAILABLE,
                    json!({"activity": report_value(3)}),
                ),
                "invalid report" => failure(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({"activity": {"phase": "dreaming"}}),
                ),
                _ => Json(json!({
                    "correlation_id": request.correlation_id,
                    "output": "reply",
                    "activity": {"phase": "dreaming"},
                }))
                .into_response(),
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let call = async |input: &str| {
        let mut request = state.core.runtime_request(&turn, Vec::new());
        request.input = input.to_owned();
        call_runtime(&state.client, &runtime, "unused", request).await
    };
    for (input, status, explanation, calls) in [
        ("reported", 502, Some("The build broke."), 1),
        ("report only", 500, None, 2),
    ] {
        match call(input).await {
            Err(TurnFailure::Reported(reported)) => {
                assert_eq!(reported.status, status, "{input}");
                assert_eq!(reported.explanation.as_deref(), explanation, "{input}");
                assert_eq!(reported.activity, Some(report(calls)?), "{input}");
            }
            outcome => anyhow::bail!("{input}: {outcome:?}"),
        }
    }
    let closing = call("closing").await;
    assert!(
        matches!(closing, Err(TurnFailure::Unanswered(_))),
        "{closing:?}"
    );
    let invalid = call("invalid report").await;
    assert!(
        matches!(invalid, Err(TurnFailure::Uncertain(_))),
        "{invalid:?}"
    );
    let answered = call("answered")
        .await
        .map_err(|failure| anyhow!("{failure:?}"))?;
    assert_eq!(
        (answered.output.as_str(), answered.activity),
        ("reply", None)
    );
    server.abort();
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
    run_next_turn(&state, "replacement", async |runtime, _, request| {
        assert_eq!(runtime.id, "replacement");
        assert_eq!(request.correlation_id, plain.turn_id);
        assert_eq!(request.agent_id, "plain");
        let snapshot = fs::read_dir(&request.resource_root)
            .map_err(|error| TurnFailure::Uncertain(error.into()))?;
        assert_eq!(snapshot.count(), 0);
        Ok(completed(request))
    })
    .await?;
    run_next_turn(&state, "test-runtime", async |_, _, request| {
        assert_eq!(request.correlation_id, assistant.turn_id);
        let system = fs::read_to_string(Path::new(&request.resource_root).join("SYSTEM.md"))
            .map_err(|error| TurnFailure::Uncertain(error.into()))?;
        assert_eq!(system, "assistant");
        Ok(completed(request))
    })
    .await?;
    crate::assert_refused(
        accept_for(&state, "unknown", "unknown-turn", "conversation").await,
        "unknown agent",
    )?;
    Ok(())
}

mod cutover;

mod models;

mod health;
